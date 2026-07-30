use super::{PackageRepository, RepositoryFromUrl};
use crate::http;
use async_trait::async_trait;
use moka::future::Cache;
use r_description::lossless::{RDescription, Version};
use reqwest::Url;
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct RrepoRepository {
    url: Url,
    client: http::HttpClient,
    packages: Cache<(), Arc<http::RrepoPackagesResponse>>,
    versions: Cache<String, BTreeSet<Version>>,
    descriptions: Cache<(String, Version), Arc<RDescription>>,
}

impl RrepoRepository {
    pub fn new(client: http::HttpClient, url: Url) -> Self {
        Self {
            url,
            client,
            packages: Cache::new(1),
            versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }
}

#[async_trait]
impl RepositoryFromUrl for RrepoRepository {
    async fn from_url(client: http::HttpClient, url: Url) -> Result<Self, String> {
        http::rrepo_repository_packages(&client, &url)
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;

        Ok(Self::new(client, url))
    }
}

#[async_trait]
impl PackageRepository for RrepoRepository {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn equals(&self, other: &dyn PackageRepository) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.url == other.url)
    }

    async fn packages(&self) -> Result<BTreeMap<String, Version>, String> {
        let response = self
            .packages
            .try_get_with((), async {
                let response = http::rrepo_repository_packages(&self.client, &self.url)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?
                    .json::<http::RrepoPackagesResponse>()
                    .await
                    .map_err(|error| error.to_string())?;

                Ok::<Arc<http::RrepoPackagesResponse>, String>(Arc::new(response))
            })
            .await
            .map_err(|error| error.as_ref().clone())?;

        Ok(response
            .packages
            .iter()
            .filter_map(|package| {
                let version = match package.latest_version.parse::<Version>() {
                    Ok(version) => version,
                    Err(error) => {
                        tracing::debug!(
                            package = %package.name,
                            version = %package.latest_version,
                            repository = %self.url,
                            error = %error,
                            "skipping package with invalid latest version"
                        );
                        return None;
                    }
                };

                Some((package.name.clone(), version))
            })
            .collect())
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<Version>, String> {
        if !self.packages().await?.contains_key(package) {
            return Ok(BTreeSet::new());
        }

        let versions = self
            .versions
            .try_get_with(package.to_string(), async {
                let response = http::rrepo_package_versions(&self.client, &self.url, package)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?
                    .json::<http::RrepoPackageVersionsResponse>()
                    .await
                    .map_err(|error| error.to_string())?;

                response
                    .versions
                    .into_iter()
                    .map(|summary| {
                        summary.version.parse::<Version>().map_err(|error| {
                            format!("invalid version {} for {package}: {error}", summary.version)
                        })
                    })
                    .collect::<Result<BTreeSet<_>, String>>()
            })
            .await
            .map_err(|error| error.as_ref().clone())?;

        tracing::trace!(
            package,
            repository = %self.url,
            versions = versions.len(),
            "loaded package versions"
        );

        Ok(versions)
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, String> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let description = http::rrepo_package_description(
                    &self.client,
                    &self.url,
                    package,
                    &version.to_string(),
                )
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?
                .text()
                .await
                .map_err(|error| error.to_string())?
                .parse::<RDescription>()
                .map_err(|error| {
                    format!("failed to parse DESCRIPTION for {package} {version}: {error}")
                })?;

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url,
                    "fetched package description"
                );

                Ok::<Arc<RDescription>, String>(Arc::new(description))
            })
            .await
            .map_err(|error| error.as_ref().clone())
    }
}

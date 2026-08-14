use super::{PackageRepository, RepositoryError};
use crate::{http, resolver::PackageVersion};
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
    packages: Cache<(), Arc<http::RrepoPackagesResponse>>,
    versions: Cache<String, BTreeSet<Version>>,
    descriptions: Cache<(String, Version), Arc<RDescription>>,
}

impl std::fmt::Display for RrepoRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url.fmt(formatter)
    }
}

impl RrepoRepository {
    pub fn new(mut url: Url) -> Self {
        url.path_segments_mut()
            .expect("repository base URL should support path segments")
            .pop_if_empty();

        Self {
            url,
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

    async fn packages(&self) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let response = self
            .packages
            .try_get_with((), async {
                let response = http::rrepo_repository_packages(&self.url)
                    .await
                    .map_err(|source| RepositoryError::Request {
                        source: Arc::new(source),
                    })?
                    .error_for_status()
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?
                    .json::<http::RrepoPackagesResponse>()
                    .await
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?;

                Ok::<Arc<http::RrepoPackagesResponse>, RepositoryError>(Arc::new(response))
            })
            .await
            .map_err(Arc::unwrap_or_clone)?;

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

                Some((
                    package.name.clone(),
                    PackageVersion::new(version, Arc::clone(&repository)),
                ))
            })
            .collect())
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let versions = self
            .versions
            .try_get_with(package.to_string(), async {
                let response = http::rrepo_package_versions(&self.url, package)
                    .await
                    .map_err(|source| RepositoryError::Request {
                        source: Arc::new(source),
                    })?
                    .error_for_status()
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?
                    .json::<http::RrepoPackageVersionsResponse>()
                    .await
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?;

                response
                    .versions
                    .into_iter()
                    .map(|summary| {
                        summary.version.parse::<Version>().map_err(|details| {
                            RepositoryError::InvalidData {
                                resource: format!(
                                    "package version {} for {package}",
                                    summary.version
                                ),
                                details,
                            }
                        })
                    })
                    .collect::<Result<BTreeSet<_>, RepositoryError>>()
            })
            .await
            .map_err(Arc::unwrap_or_clone)?;

        tracing::trace!(
            package,
            repository = %self.url,
            versions = versions.len(),
            "loaded package versions"
        );

        Ok(versions
            .into_iter()
            .map(|version| PackageVersion::new(version, Arc::clone(&repository)))
            .collect())
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, RepositoryError> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let description =
                    http::rrepo_package_description(&self.url, package, &version.to_string())
                        .await
                        .map_err(|source| RepositoryError::Request {
                            source: Arc::new(source),
                        })?
                        .error_for_status()
                        .map_err(|source| RepositoryError::Response {
                            source: Arc::new(source),
                        })?
                        .text()
                        .await
                        .map_err(|source| RepositoryError::Response {
                            source: Arc::new(source),
                        })?
                        .parse::<RDescription>()
                        .map_err(|source| RepositoryError::Description {
                            location: format!("for {package} {version}"),
                            source: Arc::new(source),
                        })?;

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url,
                    "fetched package description"
                );

                Ok::<Arc<RDescription>, RepositoryError>(Arc::new(description))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }
}

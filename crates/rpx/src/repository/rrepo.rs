use super::RepositoryError;
use crate::http;
use moka::future::Cache;
use r_description::Description;
use r_metadata::Version;
use reqwest::Url;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct RrepoRepository {
    url: Url,
    packages: Cache<(), Arc<http::RrepoPackagesResponse>>,
    versions: Cache<String, Arc<BTreeMap<Version, String>>>,
    descriptions: Cache<(String, Version), Arc<Description>>,
}

impl std::fmt::Display for RrepoRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url.fmt(formatter)
    }
}

impl RrepoRepository {
    pub fn new(url: Url) -> Self {
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
    pub async fn packages(&self) -> Result<Arc<http::RrepoPackagesResponse>, RepositoryError> {
        self.packages
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
            .map_err(Arc::unwrap_or_clone)
    }

    /// Version -> source URL, retaining the endpoint's native artifact references.
    pub async fn versions(
        &self,
        package: &str,
    ) -> Result<Arc<BTreeMap<Version, String>>, RepositoryError> {
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
                        summary
                            .version
                            .parse::<Version>()
                            .map(|version| (version, summary.source_url))
                            .map_err(|source| RepositoryError::InvalidData {
                                resource: format!(
                                    "package version {} for {package}",
                                    summary.version
                                ),
                                details: source.to_string(),
                            })
                    })
                    .collect::<Result<BTreeMap<_, _>, RepositoryError>>()
                    .map(Arc::new)
            })
            .await
            .map_err(Arc::unwrap_or_clone)?;

        tracing::trace!(
            package,
            repository = %self.url,
            versions = versions.len(),
            "loaded package versions"
        );

        Ok(versions)
    }

    pub async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<Description>, RepositoryError> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let description =
                    http::rrepo_package_description(&self.url, package, version.as_ref())
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
                        })?;
                let description = Description::parse(&description);

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url,
                    "fetched package description"
                );

                Ok::<Arc<Description>, RepositoryError>(Arc::new(description))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }

    pub async fn source(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        http::rrepo_source_artifact(&self.url, package, version.as_ref()).await
    }

    pub async fn binary(
        &self,
        package: &str,
        version: &Version,
        target: &target_lexicon::Triple,
        r_version: &semver::Version,
    ) -> Result<reqwest::Response, http::BinaryArtifactRequestError> {
        http::rrepo_binary(&self.url, package, version.as_ref(), target, r_version).await
    }
}

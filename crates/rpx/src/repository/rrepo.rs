use super::RepositoryError;
use crate::http;
use moka::future::Cache;
use r_description::Description;
use r_metadata::Version;
use reqwest::Url;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct RrepoRepository {
    source: rrepo_sdk::Repository,
    packages: Cache<(), Arc<rrepo_sdk::PackagesResponse>>,
    versions: Cache<String, Arc<BTreeMap<Version, String>>>,
    descriptions: Cache<(String, Version), Arc<Description>>,
}

impl std::fmt::Display for RrepoRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url().fmt(formatter)
    }
}

impl RrepoRepository {
    pub fn new(url: Url) -> Result<Self, rrepo_sdk::InvalidBaseUrl> {
        Ok(Self {
            source: rrepo_sdk::Repository::new(url)?,
            packages: Cache::new(1),
            versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        })
    }

    pub fn url(&self) -> &Url {
        self.source.base_url()
    }

    /// Recognize rrepo and retain its fetched index on the returned handle.
    pub async fn probe(url: Url) -> Result<Self, RepositoryError> {
        let repository = Self::new(url)?;
        repository.packages().await?;
        Ok(repository)
    }

    #[cfg(test)]
    pub(crate) fn invalidate_descriptions(&self) {
        self.descriptions.invalidate_all();
    }
    pub async fn packages(&self) -> Result<Arc<rrepo_sdk::PackagesResponse>, RepositoryError> {
        self.packages
            .try_get_with((), async {
                self.source
                    .packages(&http::client())
                    .await
                    .map(Arc::new)
                    .map_err(RepositoryError::from)
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
                let response = self
                    .source
                    .versions(&http::client(), package)
                    .await
                    .map_err(RepositoryError::from)?;

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
            repository = %self.url(),
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
                let description = self
                    .source
                    .description(&http::client(), package, version.as_ref())
                    .await
                    .map_err(RepositoryError::from)?;

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url(),
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
        self.source
            .source(&http::client(), package, version.as_ref())
            .await
    }

    pub async fn binary(
        &self,
        package: &str,
        version: &Version,
        target: &target_lexicon::Triple,
        r_version: &semver::Version,
    ) -> Result<reqwest::Response, http::BinaryArtifactRequestError> {
        let client = http::client();
        let r_version: Version = format!("{}.{}", r_version.major, r_version.minor)
            .parse()
            .expect("numeric R major/minor version");
        self.source
            .binary(&client, package, version, target, &r_version)
            .await
            .map_err(|error| match error {
                rrepo_sdk::BinaryError::Request(error) => error.into(),
                rrepo_sdk::BinaryError::UnsupportedTarget => {
                    http::BinaryArtifactRequestError::UnsupportedTarget {
                        target: target.clone(),
                    }
                }
            })
    }
}

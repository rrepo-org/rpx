mod cran;
mod local;
mod rrepo;

use crate::http;
use crate::resolver::PackageVersion;
use async_trait::async_trait;
use r_description::lossless::{RDescription, Version};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt::{Debug, Display},
    path::PathBuf,
    sync::Arc,
};
use thiserror::Error;

pub use cran::CranRepository;
pub use local::LocalRepository;
pub use rrepo::RrepoRepository;

pub const DEFAULT_REGISTRY_BASE_URL: &str = "https://upstream.rrepo.dev/cran";

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveSupport {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Error)]
pub enum RepositoryError {
    #[error("request failed: {source}")]
    Request {
        #[source]
        source: Arc<reqwest_middleware::Error>,
    },

    #[error("response failed: {source}")]
    Response {
        #[source]
        source: Arc<reqwest::Error>,
    },

    #[error("failed to read source archive: {source}")]
    Archive {
        #[source]
        source: Arc<std::io::Error>,
    },

    #[error("failed to read {path}: {source}")]
    FileRead {
        path: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },

    #[error("failed to parse DESCRIPTION {location}: {source}")]
    Description {
        location: String,
        #[source]
        source: Arc<r_description::lossless::Error>,
    },

    #[error("invalid {resource}: {details}")]
    InvalidData { resource: String, details: String },

    #[error("DESCRIPTION at {path} is missing {field}")]
    MissingField { path: PathBuf, field: &'static str },

    #[error("source package does not contain {package}/DESCRIPTION")]
    DescriptionNotFound { package: String },

    #[error("local repository at {path} does not contain {package} {version}")]
    PackageVersionNotFound {
        path: PathBuf,
        package: String,
        version: Version,
    },

    #[error("invalid repository URL {value}")]
    InvalidUrl { value: String },

    #[error("{url} is not an rrepo API ({rrepo}) or CRAN-like repository ({cran})")]
    UnrecognizedRepository {
        url: String,
        rrepo: Box<RepositoryError>,
        cran: Box<RepositoryError>,
    },
}

#[async_trait]
pub trait PackageRepository: Any + Debug + Display + Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn equals(&self, other: &dyn PackageRepository) -> bool;

    async fn packages(
        &self,
        client: &http::HttpClient,
    ) -> Result<BTreeMap<String, PackageVersion>, RepositoryError>;

    async fn versions(
        &self,
        client: &http::HttpClient,
        package: &str,
    ) -> Result<BTreeSet<PackageVersion>, RepositoryError>;

    async fn description(
        &self,
        client: &http::HttpClient,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, RepositoryError>;
}

impl dyn PackageRepository {
    pub fn downcast_ref<T: PackageRepository + 'static>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }

    pub async fn from_url(
        client: &http::HttpClient,
        value: &str,
    ) -> Result<Arc<dyn PackageRepository>, RepositoryError> {
        let value = value.trim();
        let url = Url::parse(value).map_err(|_| RepositoryError::InvalidUrl {
            value: value.to_string(),
        })?;

        let rrepo_url = url.clone();
        let rrepo_probe = async {
            http::rrepo_repository_packages(client, &rrepo_url)
                .await
                .map_err(|source| RepositoryError::Request {
                    source: Arc::new(source),
                })?
                .error_for_status()
                .map_err(|source| RepositoryError::Response {
                    source: Arc::new(source),
                })?;

            Ok::<Arc<dyn PackageRepository>, RepositoryError>(Arc::new(RrepoRepository::new(
                rrepo_url,
            )))
        };

        let cran_url = url;
        let cran_probe = async {
            let packages_probe = async {
                http::cran_packages(client, &cran_url)
                    .await
                    .map_err(|source| RepositoryError::Request {
                        source: Arc::new(source),
                    })?
                    .error_for_status()
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })
            };
            let archive_probe = async {
                http::cran_archive_root(client, &cran_url)
                    .await
                    .map_err(|source| RepositoryError::Request {
                        source: Arc::new(source),
                    })?
                    .error_for_status()
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })
            };

            let (packages_result, archive_result) = tokio::join!(packages_probe, archive_probe);
            packages_result?;

            let archives = match archive_result {
                Ok(_) => ArchiveSupport::Available,
                Err(RepositoryError::Response { source })
                    if matches!(
                        source.status(),
                        Some(reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN)
                    ) =>
                {
                    ArchiveSupport::Unavailable
                }
                Err(error) => return Err(error),
            };

            Ok::<Arc<dyn PackageRepository>, RepositoryError>(Arc::new(CranRepository::new(
                cran_url, archives,
            )))
        };

        tokio::pin!(rrepo_probe);
        tokio::pin!(cran_probe);

        tokio::select! {
            rrepo_result = &mut rrepo_probe => {
                match rrepo_result {
                    Ok(repository) => Ok(repository),
                    Err(rrepo_error) => {
                        match cran_probe.await {
                            Ok(repository) => Ok(repository),
                            Err(cran_error) => Err(RepositoryError::UnrecognizedRepository {
                                url: value.to_string(),
                                rrepo: Box::new(rrepo_error),
                                cran: Box::new(cran_error),
                            }),
                        }
                    }
                }
            }

            cran_result = &mut cran_probe => {
                match cran_result {
                    Ok(repository) => Ok(repository),
                    Err(cran_error) => {
                        match rrepo_probe.await {
                            Ok(repository) => Ok(repository),
                            Err(rrepo_error) => Err(RepositoryError::UnrecognizedRepository {
                                url: value.to_string(),
                                rrepo: Box::new(rrepo_error),
                                cran: Box::new(cran_error),
                            }),
                        }
                    }
                }
            }
        }
    }
}

impl PartialEq for dyn PackageRepository {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for dyn PackageRepository {}

#[deprecated(note = "parse into Url and canonicalize with path_segments_mut().pop_if_empty()")]
pub fn normalize_repository_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

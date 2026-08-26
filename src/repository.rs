mod cran;
mod git;
mod local;
mod rrepo;

use crate::http;
use crate::resolver::PackageVersion;
use async_trait::async_trait;
use miette::Diagnostic;
use r_description::{PackageError, RDescription, Version, VersionError};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt::{Debug, Display},
    path::PathBuf,
    sync::{Arc, LazyLock},
};
use thiserror::Error;

pub use cran::CranRepository;
pub use git::GitRepository;
pub use local::LocalRepository;
pub use rrepo::RrepoRepository;

const BUILT_IN_REPOSITORY_BASE_URL: &str = "https://upstream.rrepo.dev/cran";

static BUILT_IN_REPOSITORY_URL: LazyLock<Url> = LazyLock::new(|| {
    parse_repository_url(BUILT_IN_REPOSITORY_BASE_URL)
        .expect("built-in repository URL should be valid")
});

static BUILT_IN_REPOSITORY: LazyLock<Arc<RrepoRepository>> =
    LazyLock::new(|| Arc::new(RrepoRepository::new(built_in_repository_url().clone())));

pub fn built_in_repository_url() -> &'static Url {
    &BUILT_IN_REPOSITORY_URL
}

pub fn built_in_repository() -> Arc<dyn PackageRepository> {
    BUILT_IN_REPOSITORY.clone()
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveSupport {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum RepositoryError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    CranPackages(Box<http::CranPackagesParseError>),

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

    #[error("failed to read Package from DESCRIPTION {location}: {source}")]
    PackageField {
        location: String,
        #[source]
        source: Arc<PackageError>,
    },

    #[error("failed to read Version from DESCRIPTION {location}: {source}")]
    VersionField {
        location: String,
        #[source]
        source: Arc<VersionError>,
    },

    #[error("invalid {resource}: {details}")]
    InvalidData { resource: String, details: String },

    #[error("source package does not contain {package}/DESCRIPTION")]
    DescriptionNotFound { package: String },

    #[error("local repository at {path} does not contain {package} {version}")]
    PackageVersionNotFound {
        path: PathBuf,
        package: String,
        version: Version,
    },

    #[allow(dead_code)]
    #[error("Git repository {repository} failed: {source}")]
    Git {
        repository: String,
        #[source]
        source: Arc<crate::git::GitError>,
    },

    #[allow(dead_code)]
    #[error("repository {repository} does not contain {package} {version}")]
    RepositoryPackageVersionNotFound {
        repository: String,
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

    async fn packages(&self) -> Result<BTreeMap<String, PackageVersion>, RepositoryError>;

    async fn versions(&self, package: &str) -> Result<BTreeSet<PackageVersion>, RepositoryError>;

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, RepositoryError>;
}

impl dyn PackageRepository {
    pub fn downcast_ref<T: PackageRepository + 'static>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }

    pub async fn from_url(url: Url) -> Result<Arc<dyn PackageRepository>, RepositoryError> {
        let value = url.to_string();
        let rrepo_url = url.clone();
        let rrepo_probe = async {
            http::rrepo_repository_packages(&rrepo_url)
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
                http::cran_packages(&cran_url)
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
                http::cran_archive_root(&cran_url)
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
                                url: value.clone(),
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
                                url: value,
                                rrepo: Box::new(rrepo_error),
                                cran: Box::new(cran_error),
                            }),
                        }
                    }
                }
            }
        }
    }

    pub fn from_lockfile(
        repository: &crate::lockfile::Repository,
    ) -> Result<Arc<dyn PackageRepository>, RepositoryError> {
        match repository {
            crate::lockfile::Repository::Rrepo { url } => {
                Ok(Arc::new(RrepoRepository::new(url.clone())))
            }
            crate::lockfile::Repository::CranLike {
                url,
                archive_support,
            } => {
                let archive_support = match archive_support {
                    crate::lockfile::ArchiveSupport::Available => ArchiveSupport::Available,
                    crate::lockfile::ArchiveSupport::Unavailable => ArchiveSupport::Unavailable,
                };
                Ok(Arc::new(CranRepository::new(url.clone(), archive_support)))
            }
            crate::lockfile::Repository::Git {
                url,
                reference,
                commit,
                subdirectory,
            } => {
                let remote =
                    crate::git::GitUrl::try_from(url).map_err(|source| RepositoryError::Git {
                        repository: url.to_string(),
                        source: Arc::new(source),
                    })?;
                let reference = match reference {
                    crate::lockfile::GitReference::DefaultBranch => None,
                    crate::lockfile::GitReference::Named { value } => Some(value.clone()),
                    crate::lockfile::GitReference::Commit => Some(commit.to_string()),
                };
                let subdirectory = subdirectory.as_ref().map(|path| path.to_path(""));
                Ok(Arc::new(
                    GitRepository::from_parts(remote, reference, subdirectory).with_commit(*commit),
                ))
            }
        }
    }

    pub async fn to_lockfile(&self) -> Result<crate::lockfile::Repository, RepositoryError> {
        if let Some(repository) = self.downcast_ref::<RrepoRepository>() {
            return Ok(crate::lockfile::Repository::Rrepo {
                url: repository.url().clone(),
            });
        }

        if let Some(repository) = self.downcast_ref::<CranRepository>() {
            let archive_support = match repository.archive_support() {
                ArchiveSupport::Available => crate::lockfile::ArchiveSupport::Available,
                ArchiveSupport::Unavailable => crate::lockfile::ArchiveSupport::Unavailable,
            };
            return Ok(crate::lockfile::Repository::CranLike {
                url: repository.url().clone(),
                archive_support,
            });
        }

        if let Some(repository) = self.downcast_ref::<GitRepository>() {
            let commit = repository.commit().await?;
            let url = reqwest::Url::try_from(repository.remote()).map_err(|source| {
                RepositoryError::Git {
                    repository: repository.to_string(),
                    source: Arc::new(source),
                }
            })?;
            let url = parse_repository_url(url.as_str())?;
            let reference = match repository.reference() {
                None => crate::lockfile::GitReference::DefaultBranch,
                Some(reference) if is_commit_reference(reference, commit) => {
                    crate::lockfile::GitReference::Commit
                }
                Some(value) => crate::lockfile::GitReference::Named {
                    value: value.to_string(),
                },
            };
            let subdirectory = repository
                .subdirectory()
                .map(relative_path::RelativePathBuf::from_path)
                .transpose()
                .map_err(|error| RepositoryError::InvalidData {
                    resource: format!("subdirectory in {repository}"),
                    details: error.to_string(),
                })?;

            return Ok(crate::lockfile::Repository::Git {
                url,
                reference,
                commit,
                subdirectory,
            });
        }

        Err(RepositoryError::InvalidData {
            resource: "lockfile repository".to_string(),
            details: format!("unsupported repository {self}"),
        })
    }
}

fn is_commit_reference(reference: &str, commit: git2::Oid) -> bool {
    (4..=40).contains(&reference.len())
        && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
        && commit.to_string().starts_with(reference)
}

pub fn parse_repository_url(value: &str) -> Result<Url, RepositoryError> {
    let value = value.trim();
    let mut url = Url::parse(value).map_err(|_| RepositoryError::InvalidUrl {
        value: value.to_string(),
    })?;
    url.path_segments_mut()
        .map_err(|()| RepositoryError::InvalidUrl {
            value: value.to_string(),
        })?
        .pop_if_empty();
    Ok(url)
}

impl PartialEq for dyn PackageRepository {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for dyn PackageRepository {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_repository_urls() {
        assert_eq!(
            parse_repository_url("  https://example.test/cran/  ")
                .unwrap()
                .as_str(),
            "https://example.test/cran"
        );
        assert_eq!(
            parse_repository_url("https://example.test/")
                .unwrap()
                .as_str(),
            "https://example.test/"
        );
        assert!(parse_repository_url("mailto:packages@example.test").is_err());
    }
}

mod cran;
mod git;
mod local;
mod rrepo;

use crate::description::DescriptionParseError;
use miette::Diagnostic;
use r_metadata::Version;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fmt::Display,
    path::PathBuf,
    sync::{Arc, LazyLock},
};
use thiserror::Error;

pub(crate) use cran::CranPackagesParseError;
pub use cran::CranRepository;
pub use git::GitRepository;
pub use local::LocalRepository;
pub use rrepo::RrepoRepository;

const BUILT_IN_REPOSITORY_BASE_URL: &str = "https://rrepo.dev/upstream/cran";

static BUILT_IN_REPOSITORY_URL: LazyLock<Url> = LazyLock::new(|| {
    parse_repository_url(BUILT_IN_REPOSITORY_BASE_URL)
        .expect("built-in repository URL should be valid")
});

static BUILT_IN_REPOSITORY: LazyLock<Arc<RrepoRepository>> = LazyLock::new(|| {
    Arc::new(
        RrepoRepository::new(built_in_repository_url().clone()).expect("valid built-in repository"),
    )
});

pub fn built_in_repository_url() -> &'static Url {
    &BUILT_IN_REPOSITORY_URL
}

pub fn built_in_repository() -> PackageRepository {
    PackageRepository::Rrepo(BUILT_IN_REPOSITORY.clone())
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveSupport {
    /// The repository exposes archive directory listings.
    Available,
    /// Directory listing is unavailable; known archive URLs may still work.
    Unavailable,
}

#[derive(Debug, Clone, Error, Diagnostic)]
pub enum RepositoryError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    CranPackages(Box<CranPackagesParseError>),

    #[error(transparent)]
    Cran(Arc<cran_sdk::ListingError>),

    #[error(transparent)]
    InvalidCranUrl(#[from] cran_sdk::InvalidBaseUrl),

    #[error(transparent)]
    InvalidRrepoUrl(#[from] rrepo_sdk::InvalidBaseUrl),

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

    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error("invalid {resource}: {details}")]
    InvalidData { resource: String, details: String },

    #[error("source package does not contain {package}/DESCRIPTION")]
    DescriptionNotFound { package: String },

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

/// Shared native sources. Cloning preserves caches, pinned commits, and local
/// DESCRIPTION overrides. Equality compares configuration, not resolved content.
#[derive(Debug, Clone)]
pub enum PackageRepository {
    Cran(Arc<CranRepository>),
    Rrepo(Arc<RrepoRepository>),
    Git(Arc<GitRepository>),
    Local(Arc<LocalRepository>),
}

impl PackageRepository {
    /// Resolution metadata is scoped to an exact handle, not configuration
    /// equality (two local overrides or Git revisions may share configuration).
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cran(a), Self::Cran(b)) => Arc::ptr_eq(a, b),
            (Self::Rrepo(a), Self::Rrepo(b)) => Arc::ptr_eq(a, b),
            (Self::Git(a), Self::Git(b)) => Arc::ptr_eq(a, b),
            (Self::Local(a), Self::Local(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub async fn from_url(url: Url) -> Result<Self, RepositoryError> {
        let value = url.to_string();
        let rrepo_url = url.clone();
        let rrepo_probe = async {
            let repository = Arc::new(RrepoRepository::new(rrepo_url)?);
            repository.packages().await?;
            Ok::<_, RepositoryError>(Self::Rrepo(repository))
        };

        let cran_url = url;
        let cran_probe = async {
            let repository = CranRepository::new(cran_url.clone(), ArchiveSupport::Unavailable)?;
            let packages_probe = repository.packages_index();
            let archive_probe = async {
                repository
                    .archive_root()
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

            Ok::<_, RepositoryError>(Self::Cran(Arc::new(
                repository.with_archive_support(archives),
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
                            Err(cran_error) => Err(discovery_failure(value.clone(), rrepo_error, cran_error)),
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
                            Err(rrepo_error) => Err(discovery_failure(value, rrepo_error, cran_error)),
                        }
                    }
                }
            }
        }
    }

    pub fn from_lockfile(
        repository: &crate::lockfile::Repository,
    ) -> Result<Self, RepositoryError> {
        match repository {
            crate::lockfile::Repository::Rrepo { url } => {
                Ok(Self::Rrepo(Arc::new(RrepoRepository::new(url.clone())?)))
            }
            crate::lockfile::Repository::CranLike {
                url,
                archive_support,
            } => {
                let archive_support = match archive_support {
                    crate::lockfile::ArchiveSupport::Available => ArchiveSupport::Available,
                    crate::lockfile::ArchiveSupport::Unavailable => ArchiveSupport::Unavailable,
                };
                Ok(Self::Cran(Arc::new(CranRepository::new(
                    url.clone(),
                    archive_support,
                )?)))
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
                Ok(Self::Git(Arc::new(
                    GitRepository::from_parts(remote, reference, subdirectory).with_commit(*commit),
                )))
            }
        }
    }

    pub async fn to_lockfile(&self) -> Result<crate::lockfile::Repository, RepositoryError> {
        match self {
            Self::Rrepo(repository) => Ok(crate::lockfile::Repository::Rrepo {
                url: repository.url().clone(),
            }),
            Self::Cran(repository) => {
                let archive_support = match repository.archive_support() {
                    ArchiveSupport::Available => crate::lockfile::ArchiveSupport::Available,
                    ArchiveSupport::Unavailable => crate::lockfile::ArchiveSupport::Unavailable,
                };
                Ok(crate::lockfile::Repository::CranLike {
                    url: repository.url().clone(),
                    archive_support,
                })
            }
            Self::Git(repository) => {
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

                Ok(crate::lockfile::Repository::Git {
                    url,
                    reference,
                    commit,
                    subdirectory,
                })
            }
            Self::Local(_) => Err(RepositoryError::InvalidData {
                resource: "lockfile repository".to_string(),
                details: format!("unsupported repository {self}"),
            }),
        }
    }
}

impl From<cran_sdk::FetchError> for RepositoryError {
    fn from(error: cran_sdk::FetchError) -> Self {
        match error {
            cran_sdk::FetchError::Request(source) => Self::Request {
                source: Arc::new(source),
            },
            cran_sdk::FetchError::Response(source) => Self::Response {
                source: Arc::new(source),
            },
        }
    }
}

impl From<cran_sdk::DescriptionError> for RepositoryError {
    fn from(error: cran_sdk::DescriptionError) -> Self {
        match error {
            cran_sdk::DescriptionError::Fetch(error) => error.into(),
            cran_sdk::DescriptionError::Archive(source) => Self::Archive {
                source: Arc::new(source),
            },
            cran_sdk::DescriptionError::DescriptionNotFound { package } => {
                Self::DescriptionNotFound { package }
            }
        }
    }
}

impl From<cran_sdk::PackagesError> for RepositoryError {
    fn from(error: cran_sdk::PackagesError) -> Self {
        match error {
            cran_sdk::PackagesError::Fetch(error) => error.into(),
            cran_sdk::PackagesError::Invalid(error) => Self::CranPackages(Box::new(
                CranPackagesParseError::new("CRAN PACKAGES", error.text, error.findings),
            )),
        }
    }
}

impl From<cran_sdk::ListingError> for RepositoryError {
    fn from(error: cran_sdk::ListingError) -> Self {
        match error {
            cran_sdk::ListingError::Fetch(error) => error.into(),
            error => Self::Cran(Arc::new(error)),
        }
    }
}

impl From<rrepo_sdk::FetchError> for RepositoryError {
    fn from(error: rrepo_sdk::FetchError) -> Self {
        match error {
            rrepo_sdk::FetchError::Request(source) => Self::Request {
                source: Arc::new(source),
            },
            rrepo_sdk::FetchError::Response(source) => Self::Response {
                source: Arc::new(source),
            },
        }
    }
}

fn discovery_failure(
    url: String,
    rrepo: RepositoryError,
    cran: RepositoryError,
) -> RepositoryError {
    match cran {
        error @ RepositoryError::CranPackages(_) => error,
        cran => RepositoryError::UnrecognizedRepository {
            url,
            rrepo: Box::new(rrepo),
            cran: Box::new(cran),
        },
    }
}

fn is_commit_reference(reference: &str, commit: crate::git::GitOid) -> bool {
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

impl PartialEq for PackageRepository {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Rrepo(a), Self::Rrepo(b)) => a.url() == b.url(),
            (Self::Cran(a), Self::Cran(b)) => {
                a.url() == b.url() && a.archive_support() == b.archive_support()
            }
            (Self::Git(a), Self::Git(b)) => {
                a.remote() == b.remote()
                    && a.reference() == b.reference()
                    && a.subdirectory() == b.subdirectory()
            }
            (Self::Local(a), Self::Local(b)) => a.path() == b.path(),
            _ => false,
        }
    }
}

impl Eq for PackageRepository {}

impl Display for PackageRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cran(repo) => repo.fmt(f),
            Self::Rrepo(repo) => repo.fmt(f),
            Self::Git(repo) => repo.fmt(f),
            Self::Local(repo) => repo.fmt(f),
        }
    }
}

impl From<Arc<LocalRepository>> for PackageRepository {
    fn from(value: Arc<LocalRepository>) -> Self {
        Self::Local(value)
    }
}
impl From<Arc<GitRepository>> for PackageRepository {
    fn from(value: Arc<GitRepository>) -> Self {
        Self::Git(value)
    }
}
impl From<Arc<RrepoRepository>> for PackageRepository {
    fn from(value: Arc<RrepoRepository>) -> Self {
        Self::Rrepo(value)
    }
}
impl From<Arc<CranRepository>> for PackageRepository {
    fn from(value: Arc<CranRepository>) -> Self {
        Self::Cran(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rrepo_discovery_and_clones_reuse_the_fetched_index() {
        let mut server = mockito::Server::new_async().await;
        let index = server
            .mock("GET", "/packages")
            .with_status(200)
            .with_body(r#"{"repositorySlug":"fixture","packages":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let repository = PackageRepository::from_url(server.url().parse().unwrap())
            .await
            .unwrap();
        let clone = repository.clone();
        assert!(repository.same_instance(&clone));
        let PackageRepository::Rrepo(repo) = clone else {
            panic!("expected rrepo")
        };
        assert!(repo.packages().await.unwrap().packages.is_empty());
        index.assert_async().await;
    }

    #[tokio::test]
    async fn repeated_remote_configuration_shares_discovery_without_losing_order() {
        let mut server = mockito::Server::new_async().await;
        let index = server
            .mock("GET", "/packages")
            .with_status(200)
            .with_body(r#"{"repositorySlug":"fixture","packages":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let description = r_description::Description::parse(&format!(
            "Package: root\nVersion: 1.0.0\nConfig/rpx/base-repository: {}\nAdditional_repositories: {}\n",
            server.url(),
            server.url()
        ));
        let repositories = crate::description::repositories_from_description(
            std::path::Path::new("unused"),
            &description,
        )
        .await
        .unwrap();
        assert_eq!(repositories.len(), 2);
        assert!(repositories[0].same_instance(&repositories[1]));
        index.assert_async().await;
    }

    #[tokio::test]
    async fn cran_discovery_retains_its_validated_packages_index() {
        let mut server = mockito::Server::new_async().await;
        let _api = server
            .mock("GET", "/packages")
            .with_status(404)
            .create_async()
            .await;
        let _archive = server
            .mock("GET", "/src/contrib/Archive/")
            .with_status(403)
            .create_async()
            .await;
        let index = server
            .mock("GET", "/src/contrib/PACKAGES")
            .with_status(200)
            .with_body("Package: example\nVersion: 1.0.0\n")
            .expect(1)
            .create_async()
            .await;
        let repository = PackageRepository::from_url(server.url().parse().unwrap())
            .await
            .unwrap();
        let PackageRepository::Cran(repo) = repository else {
            panic!("expected CRAN")
        };
        assert_eq!(repo.packages_index().await.unwrap().records().count(), 1);
        assert_eq!(repo.archive_support(), ArchiveSupport::Unavailable);
        index.assert_async().await;
    }

    #[tokio::test]
    async fn malformed_cran_index_retains_its_positioned_diagnostic_during_discovery() {
        use miette::Diagnostic;
        let mut server = mockito::Server::new_async().await;
        let _api = server
            .mock("GET", "/packages")
            .with_status(404)
            .create_async()
            .await;
        let _archive = server
            .mock("GET", "/src/contrib/Archive/")
            .with_status(404)
            .create_async()
            .await;
        let _index = server
            .mock("GET", "/src/contrib/PACKAGES")
            .with_status(200)
            .with_body("Package: example\nVersion: invalid\n")
            .create_async()
            .await;
        let description = r_description::Description::parse(&format!(
            "Package: root\nVersion: 1.0.0\nConfig/rpx/base-repository: {}\n",
            server.url()
        ));
        let error = crate::description::repositories_from_description(
            std::path::Path::new("unused"),
            &description,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.code().unwrap().to_string(),
            "rpx::repository::cran_packages_parse_failed"
        );
        assert!(error.source_code().is_some());
    }

    #[tokio::test]
    async fn local_overrides_are_instance_state_not_path_identity() {
        let path = std::path::PathBuf::from("unused-local-source");
        let first = PackageRepository::Local(Arc::new(
            LocalRepository::new(path.clone()).with_description(r_description::Description::parse(
                "Package: example\nVersion: 1.0.0\n",
            )),
        ));
        let second =
            PackageRepository::Local(Arc::new(LocalRepository::new(path).with_description(
                r_description::Description::parse("Package: example\nVersion: 2.0.0\n"),
            )));
        assert_eq!(first, second);
        assert!(!first.same_instance(&second));
        let PackageRepository::Local(first) = first else {
            unreachable!()
        };
        let PackageRepository::Local(second) = second else {
            unreachable!()
        };
        assert_eq!(first.package().await.unwrap().1.to_string(), "1.0.0");
        assert_eq!(second.package().await.unwrap().1.to_string(), "2.0.0");
    }

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

use super::{PackageRepository, RepositoryError};
use crate::{
    git::{self, GitUrl},
    resolver::PackageVersion,
};
use async_trait::async_trait;
use git2::Oid;
use moka::future::Cache;
use r_description::Description;
use r_metadata::{Remote, RemoteSource, Version};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::OnceCell;
#[cfg(test)]
use tokio::sync::SetError;

#[derive(Debug, Clone)]
pub struct GitRepository {
    remote: GitUrl,
    reference: Option<String>,
    subdirectory: Option<PathBuf>,
    commit: Arc<OnceCell<Oid>>,
    descriptions: Cache<Oid, Arc<Description>>,
}

impl GitRepository {
    pub fn new(remote: Remote) -> Result<Self, RepositoryError> {
        let repository = remote.to_string();
        let (reference, subdirectory) = remote_parts(&remote);
        let url = GitUrl::try_from(remote).map_err(|source| RepositoryError::Git {
            repository,
            source: Arc::new(source),
        })?;
        let subdirectory = subdirectory
            .as_deref()
            .map(validate_subdirectory)
            .transpose()?;

        Ok(Self {
            remote: url,
            reference,
            subdirectory,
            commit: Arc::new(OnceCell::new()),
            descriptions: Cache::new(1),
        })
    }

    pub fn with_commit(mut self, commit: Oid) -> Self {
        self.commit = Arc::new(OnceCell::new_with(Some(commit)));
        self
    }

    #[cfg(test)]
    fn set_commit(&self, commit: Oid) -> Result<(), SetError<Oid>> {
        self.commit.set(commit)
    }

    pub async fn commit(&self) -> Result<Oid, RepositoryError> {
        self.commit
            .get_or_try_init(|| async {
                git::resolve(&self.remote, self.reference.as_deref())
                    .await
                    .map_err(|source| self.git_error(source))
            })
            .await
            .copied()
    }

    pub async fn checkout(&self) -> Result<PathBuf, RepositoryError> {
        let commit = self.commit().await?;
        git::checkout(&self.remote, self.reference.as_deref(), commit)
            .await
            .map_err(|source| self.git_error(source))
    }

    pub async fn checkout_path(&self) -> Result<PathBuf, RepositoryError> {
        let commit = self.commit().await?;
        Ok(git::checkout_path(&self.remote, commit))
    }

    pub fn remote(&self) -> &GitUrl {
        &self.remote
    }

    pub fn reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    pub fn subdirectory(&self) -> Option<&Path> {
        self.subdirectory.as_deref()
    }

    async fn repository_description(&self) -> Result<Arc<Description>, RepositoryError> {
        let commit = self.commit().await?;
        self.descriptions
            .try_get_with(commit, async {
                let checkout = self.checkout().await?;
                let path = self
                    .subdirectory
                    .as_ref()
                    .map_or_else(
                        || checkout.clone(),
                        |subdirectory| checkout.join(subdirectory),
                    )
                    .join("DESCRIPTION");
                let contents = tokio::fs::read_to_string(&path).await.map_err(|source| {
                    RepositoryError::FileRead {
                        path: path.clone(),
                        source: Arc::new(source),
                    }
                })?;
                let description = Description::parse(&contents);

                Ok::<Arc<Description>, RepositoryError>(Arc::new(description))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }

    async fn package(self: &Arc<Self>) -> Result<(String, PackageVersion), RepositoryError> {
        let description = self.repository_description().await?;
        let location = format!("from {self}");
        let package = description
            .package()
            .filter(|value| !value.as_str().is_empty())
            .map(|value| value.as_str().to_owned())
            .ok_or_else(|| RepositoryError::PackageField {
                location: location.clone(),
                details: "Package field is missing or empty".to_string(),
            })?;
        let version = description
            .version_parsed()
            .ok_or_else(|| RepositoryError::VersionField {
                location: location.clone(),
                details: "Version field is missing".to_string(),
            })?
            .map_err(|source| RepositoryError::VersionField {
                location,
                details: source.to_string(),
            })?;
        let repository: Arc<dyn PackageRepository> = self.clone();

        Ok((package, PackageVersion::new(version, repository)))
    }

    fn git_error(&self, source: git::GitError) -> RepositoryError {
        RepositoryError::Git {
            repository: self.to_string(),
            source: Arc::new(source),
        }
    }

    pub(crate) fn from_parts(
        remote: GitUrl,
        reference: Option<String>,
        subdirectory: Option<PathBuf>,
    ) -> Self {
        Self {
            remote,
            reference,
            subdirectory,
            commit: Arc::new(OnceCell::new()),
            descriptions: Cache::new(1),
        }
    }
}

impl std::fmt::Display for GitRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "git+{}@{}",
            self.remote,
            self.reference.as_deref().unwrap_or("HEAD")
        )?;
        if let Some(subdirectory) = &self.subdirectory {
            write!(formatter, "#{}", subdirectory.display())?;
        }
        Ok(())
    }
}

#[async_trait]
impl PackageRepository for GitRepository {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn equals(&self, other: &dyn PackageRepository) -> bool {
        other.as_any().downcast_ref::<Self>().is_some_and(|other| {
            self.remote == other.remote
                && self.reference == other.reference
                && self.subdirectory == other.subdirectory
        })
    }

    async fn packages(&self) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
        let repository = Arc::new(self.clone());
        let (package, version) = repository.package().await?;
        Ok(BTreeMap::from([(package, version)]))
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository = Arc::new(self.clone());
        let (repository_package, version) = repository.package().await?;
        if package == repository_package {
            Ok(BTreeSet::from([version]))
        } else {
            Ok(BTreeSet::new())
        }
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<Description>, RepositoryError> {
        let description = self.repository_description().await?;
        let location = format!("from {self}");
        let repository_package = description
            .package()
            .filter(|value| !value.as_str().is_empty())
            .map(|value| value.as_str().to_owned())
            .ok_or_else(|| RepositoryError::PackageField {
                location: location.clone(),
                details: "Package field is missing or empty".to_string(),
            })?;
        let repository_version = description
            .version_parsed()
            .ok_or_else(|| RepositoryError::VersionField {
                location: location.clone(),
                details: "Version field is missing".to_string(),
            })?
            .map_err(|source| RepositoryError::VersionField {
                location,
                details: source.to_string(),
            })?;
        if package != repository_package || version != &repository_version {
            return Err(RepositoryError::RepositoryPackageVersionNotFound {
                repository: self.to_string(),
                package: package.to_string(),
                version: version.clone(),
            });
        }

        Ok(description)
    }
}

fn remote_parts(remote: &Remote) -> (Option<String>, Option<String>) {
    match &remote.source {
        RemoteSource::GitHub(source)
        | RemoteSource::GitLab(source)
        | RemoteSource::Bitbucket(source) => {
            (source.reference.clone(), source.subdirectory.clone())
        }
        RemoteSource::Git(source) => (source.reference.clone(), None),
        _ => (None, None),
    }
}

fn validate_subdirectory(value: &str) -> Result<PathBuf, RepositoryError> {
    if value.is_empty()
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(RepositoryError::InvalidData {
            resource: "Git package subdirectory".to_string(),
            details: format!("invalid relative path {value}"),
        });
    }
    Ok(value.split('/').collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit_file, source_repository};
    use crate::lockfile::{GitReference, Repository};
    use std::fs;

    #[tokio::test]
    async fn converts_to_a_pinned_lockfile_repository() {
        let commit = "1111111111111111111111111111111111111111"
            .parse::<Oid>()
            .expect("commit should parse");
        let remote = "github::owner/repository/subdir@main"
            .parse::<Remote>()
            .expect("remote should parse");
        let repository: Arc<dyn PackageRepository> = Arc::new(
            GitRepository::new(remote)
                .expect("repository should build")
                .with_commit(commit),
        );

        let locked = repository
            .to_lockfile()
            .await
            .expect("repository should convert");

        assert!(matches!(
            locked,
            Repository::Git {
                url,
                reference: GitReference::Named { value },
                commit: locked_commit,
                subdirectory: Some(subdirectory),
            } if url.as_str() == "https://github.com/owner/repository.git"
                && value == "main"
                && locked_commit == commit
                && subdirectory.as_str() == "subdir"
        ));
    }

    #[tokio::test]
    async fn exposes_one_package_and_caches_description_by_commit() {
        let (source_path, source, initial) = source_repository("git-repository");
        let remote = GitUrl::from_local_path(&source_path);
        let repository = GitRepository::from_parts(remote, None, None).with_commit(initial);

        let packages = repository.packages().await.expect("packages should load");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages["example"].version().to_string(), "1.0.0");

        commit_file(&source, "Package: example\nVersion: 2.0.0\n", "second");
        let packages = repository
            .packages()
            .await
            .expect("packages should be cached");
        assert_eq!(packages["example"].version().to_string(), "1.0.0");

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn set_commit_initializes_the_lazy_commit_once() {
        let (source_path, _source, initial) = source_repository("set-commit");
        let repository = GitRepository::from_parts(
            GitUrl::from_local_path(&source_path),
            Some("main".to_string()),
            None,
        );

        repository
            .set_commit(initial)
            .expect("commit should initialize");
        assert_eq!(
            repository.commit().await.expect("commit should load"),
            initial
        );
        assert!(repository.set_commit(initial).is_err());

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[test]
    fn ignores_remote_package_alias_and_extracts_source_fields() {
        let remote = "alias=github::owner/repository/subdir@main"
            .parse::<Remote>()
            .expect("remote should parse");
        let repository = GitRepository::new(remote).expect("repository should build");

        assert_eq!(repository.reference(), Some("main"));
        assert_eq!(repository.subdirectory(), Some(Path::new("subdir")));
        assert!(!repository.to_string().contains("alias"));
    }
}

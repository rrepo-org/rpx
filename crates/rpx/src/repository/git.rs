use super::RepositoryError;
use crate::{
    description::description_identity,
    git::{self, GitOid, GitUrl},
};
use moka::future::Cache;
use r_description::Description;
use r_metadata::{Remote, RemoteSource, Version};
use std::{
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
    commit: Arc<OnceCell<GitOid>>,
    descriptions: Cache<GitOid, Arc<Description>>,
}

impl GitRepository {
    pub fn new(remote: Remote) -> Result<Self, RepositoryError> {
        let (url, reference, subdirectory) = Self::configuration(remote)?;
        Ok(Self::from_parts(url, reference, subdirectory))
    }

    fn configuration(
        remote: Remote,
    ) -> Result<(GitUrl, Option<String>, Option<PathBuf>), RepositoryError> {
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

        Ok((url, reference, subdirectory))
    }

    pub fn matches_lockfile(
        remote: &Remote,
        locked: &crate::lockfile::Repository,
    ) -> Result<bool, RepositoryError> {
        let (current_url, current_reference, current_subdirectory) =
            Self::configuration(remote.clone())?;
        let crate::lockfile::Repository::Git {
            url,
            reference,
            commit,
            subdirectory,
        } = locked
        else {
            return Ok(false);
        };
        let locked_url = GitUrl::try_from(url).map_err(|source| RepositoryError::Git {
            repository: url.to_string(),
            source: Arc::new(source),
        })?;
        let locked_reference = match reference {
            crate::lockfile::GitReference::DefaultBranch => None,
            crate::lockfile::GitReference::Named { value } => Some(value.clone()),
            crate::lockfile::GitReference::Commit => Some(commit.to_string()),
        };
        Ok(current_url == locked_url
            && current_reference == locked_reference
            && current_subdirectory == subdirectory.as_ref().map(|path| path.to_path("")))
    }

    pub(crate) fn with_commit(mut self, commit: GitOid) -> Self {
        self.commit = Arc::new(OnceCell::new_with(Some(commit)));
        self
    }

    #[cfg(test)]
    fn set_commit(&self, commit: GitOid) -> Result<(), SetError<GitOid>> {
        self.commit.set(commit)
    }

    pub(crate) async fn commit(&self) -> Result<GitOid, RepositoryError> {
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

    pub fn remote(&self) -> &GitUrl {
        &self.remote
    }

    pub fn reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    pub fn subdirectory(&self) -> Option<&Path> {
        self.subdirectory.as_deref()
    }

    pub async fn description(&self) -> Result<Arc<Description>, RepositoryError> {
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

    pub async fn package(&self) -> Result<(String, Version), RepositoryError> {
        let description = self.description().await?;
        let (package, version) =
            description_identity(format!("DESCRIPTION from {self}"), &description)?;
        Ok((package, version))
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
    use crate::repository::PackageRepository;
    use std::fs;

    #[tokio::test]
    async fn converts_to_a_pinned_lockfile_repository() {
        let commit = "1111111111111111111111111111111111111111"
            .parse::<GitOid>()
            .expect("commit should parse");
        let remote = "github::owner/repository/subdir@main"
            .parse::<Remote>()
            .expect("remote should parse");
        let repository = PackageRepository::Git(Arc::new(
            GitRepository::new(remote)
                .expect("repository should build")
                .with_commit(commit),
        ));

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

        let (name, version) = repository.package().await.expect("package should load");
        assert_eq!(name, "example");
        assert_eq!(version.to_string(), "1.0.0");

        commit_file(&source, "Package: example\nVersion: 2.0.0\n", "second");
        let (_, version) = repository
            .package()
            .await
            .expect("packages should be cached");
        assert_eq!(version.to_string(), "1.0.0");

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn reports_invalid_description_identity_with_positioned_details() {
        let (source_path, source, _) = source_repository("invalid-description-identity");
        let invalid = commit_file(&source, "Package: _bad\nVersion: nope\n", "invalid");
        let repository = Arc::new(
            GitRepository::from_parts(GitUrl::from_local_path(&source_path), None, None)
                .with_commit(invalid),
        );

        let error = repository
            .package()
            .await
            .expect_err("invalid DESCRIPTION identity should be rejected");

        let RepositoryError::Description(error) = error else {
            panic!("expected positioned DESCRIPTION error");
        };
        assert_eq!(error.messages().len(), 2);
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

use super::{PackageRepository, RepositoryError};
use crate::{http, resolver::PackageVersion};
use async_trait::async_trait;
use r_description::lossless::{RDescription, Version};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct LocalRepository {
    path: PathBuf,
    description: Option<Arc<RDescription>>,
}

impl std::fmt::Display for LocalRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.display().fmt(formatter)
    }
}

impl LocalRepository {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            description: None,
        }
    }

    pub fn with_description(mut self, description: RDescription) -> Self {
        self.set_description(description);
        self
    }

    pub fn set_description(&mut self, description: RDescription) {
        self.description = Some(Arc::new(description));
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn description(&self) -> Result<Arc<RDescription>, RepositoryError> {
        if let Some(description) = &self.description {
            return Ok(Arc::clone(description));
        }

        let path = self.path.join("DESCRIPTION");
        let contents =
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|source| RepositoryError::FileRead {
                    path: path.clone(),
                    source: Arc::new(source),
                })?;
        let description =
            contents
                .parse::<RDescription>()
                .map_err(|source| RepositoryError::Description {
                    location: format!("at {}", path.display()),
                    source: Arc::new(source),
                })?;

        Ok(Arc::new(description))
    }

    pub async fn package(self: &Arc<Self>) -> Result<(String, PackageVersion), RepositoryError> {
        let description = self.description().await?;
        let package = description
            .package()
            .ok_or_else(|| RepositoryError::MissingField {
                path: self.path.join("DESCRIPTION"),
                field: "Package",
            })?;
        let version = description
            .version()
            .ok_or_else(|| RepositoryError::MissingField {
                path: self.path.join("DESCRIPTION"),
                field: "Version",
            })?
            .parse::<Version>()
            .map_err(|details| RepositoryError::InvalidData {
                resource: format!("Version in DESCRIPTION at {}", self.path.display()),
                details,
            })?;

        let repository: Arc<dyn PackageRepository> = self.clone();
        Ok((package, PackageVersion::new(version, repository)))
    }
}

#[async_trait]
impl PackageRepository for LocalRepository {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn equals(&self, other: &dyn PackageRepository) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.path == other.path)
    }

    async fn packages(
        &self,
        _client: &http::HttpClient,
    ) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
        let repository = Arc::new(self.clone());
        let (package, version) = repository.package().await?;
        Ok(BTreeMap::from([(package, version)]))
    }

    async fn versions(
        &self,
        _client: &http::HttpClient,
        package: &str,
    ) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository = Arc::new(self.clone());
        let (local_package, version) = repository.package().await?;

        if package == local_package {
            Ok(BTreeSet::from([version]))
        } else {
            Ok(BTreeSet::new())
        }
    }

    async fn description(
        &self,
        _client: &http::HttpClient,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, RepositoryError> {
        let description = self.description().await?;
        let local_package = description
            .package()
            .ok_or_else(|| RepositoryError::MissingField {
                path: self.path.join("DESCRIPTION"),
                field: "Package",
            })?;
        let local_version = description
            .version()
            .ok_or_else(|| RepositoryError::MissingField {
                path: self.path.join("DESCRIPTION"),
                field: "Version",
            })?
            .parse::<Version>()
            .map_err(|details| RepositoryError::InvalidData {
                resource: format!("Version in DESCRIPTION at {}", self.path.display()),
                details,
            })?;

        if package != local_package || version != &local_version {
            return Err(RepositoryError::PackageVersionNotFound {
                path: self.path.clone(),
                package: package.to_string(),
                version: version.clone(),
            });
        }

        Ok(description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn package_uses_staged_description_and_preserves_repository() {
        let initial: RDescription = "Package: initial\nVersion: 0.1.0\n"
            .parse()
            .expect("description should parse");
        let staged: RDescription = "Package: project\nVersion: 1.2.3\n"
            .parse()
            .expect("description should parse");
        let mut repository =
            LocalRepository::new(PathBuf::from("unused")).with_description(initial);
        repository.set_description(staged);
        let repository = Arc::new(repository);
        let expected_repository: Arc<dyn PackageRepository> = repository.clone();

        let description = repository
            .description()
            .await
            .expect("description should load");
        let (package, version) = repository.package().await.expect("package should load");

        assert_eq!(description.package().as_deref(), Some("project"));
        assert_eq!(package, "project");
        assert_eq!(version.version().to_string(), "1.2.3");
        assert!(Arc::ptr_eq(version.repository(), &expected_repository));
    }

    #[tokio::test]
    async fn description_falls_back_to_disk() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rpx-local-repository-{}-{unique}",
            std::process::id()
        ));
        tokio::fs::create_dir(&path)
            .await
            .expect("test directory should be created");
        tokio::fs::write(
            path.join("DESCRIPTION"),
            "Package: diskproject\nVersion: 2.0.0\n",
        )
        .await
        .expect("description should be written");
        let repository = LocalRepository::new(path.clone());

        let description = repository
            .description()
            .await
            .expect("description should load");

        assert_eq!(description.package().as_deref(), Some("diskproject"));
        tokio::fs::remove_dir_all(path)
            .await
            .expect("test directory should be removed");
    }
}

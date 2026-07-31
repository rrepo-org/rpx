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
    description_override: Option<Arc<RDescription>>,
}

impl LocalRepository {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            description_override: None,
        }
    }

    pub fn with_description(mut self, description: RDescription) -> Self {
        self.description_override = Some(Arc::new(description));
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn effective_description(&self) -> Result<Arc<RDescription>, RepositoryError> {
        if let Some(description) = &self.description_override {
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

    async fn package_and_version(&self) -> Result<(String, Version), RepositoryError> {
        let description = self.effective_description().await?;
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

        Ok((package, version))
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
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let (package, version) = self.package_and_version().await?;
        Ok(BTreeMap::from([(
            package,
            PackageVersion::new(version, repository),
        )]))
    }

    async fn versions(
        &self,
        _client: &http::HttpClient,
        package: &str,
    ) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let (local_package, version) = self.package_and_version().await?;

        if package == local_package {
            Ok(BTreeSet::from([PackageVersion::new(version, repository)]))
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
        let description = self.effective_description().await?;
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

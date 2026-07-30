use super::PackageRepository;
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

    async fn effective_description(&self) -> Result<Arc<RDescription>, String> {
        if let Some(description) = &self.description_override {
            return Ok(Arc::clone(description));
        }

        let path = self.path.join("DESCRIPTION");
        let contents = tokio::fs::read_to_string(&path).await.map_err(|error| {
            format!("failed to read DESCRIPTION at {}: {error}", path.display())
        })?;
        let description = contents.parse::<RDescription>().map_err(|error| {
            format!("failed to parse DESCRIPTION at {}: {error}", path.display())
        })?;

        Ok(Arc::new(description))
    }

    async fn package_and_version(&self) -> Result<(String, Version), String> {
        let description = self.effective_description().await?;
        let package = description
            .package()
            .ok_or_else(|| format!("DESCRIPTION at {} is missing Package", self.path.display()))?;
        let version = description
            .version()
            .ok_or_else(|| format!("DESCRIPTION at {} is missing Version", self.path.display()))?
            .parse::<Version>()
            .map_err(|error| {
                format!(
                    "invalid Version in DESCRIPTION at {}: {error}",
                    self.path.display()
                )
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

    async fn packages(&self) -> Result<BTreeMap<String, Version>, String> {
        let (package, version) = self.package_and_version().await?;
        Ok(BTreeMap::from([(package, version)]))
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<Version>, String> {
        let (local_package, version) = self.package_and_version().await?;

        if package == local_package {
            Ok(BTreeSet::from([version]))
        } else {
            Ok(BTreeSet::new())
        }
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, String> {
        let description = self.effective_description().await?;
        let local_package = description
            .package()
            .ok_or_else(|| format!("DESCRIPTION at {} is missing Package", self.path.display()))?;
        let local_version = description
            .version()
            .ok_or_else(|| format!("DESCRIPTION at {} is missing Version", self.path.display()))?
            .parse::<Version>()
            .map_err(|error| {
                format!(
                    "invalid Version in DESCRIPTION at {}: {error}",
                    self.path.display()
                )
            })?;

        if package != local_package || version != &local_version {
            return Err(format!(
                "local repository at {} does not contain {package} {version}",
                self.path.display()
            ));
        }

        Ok(description)
    }
}

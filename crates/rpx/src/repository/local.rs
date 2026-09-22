use super::RepositoryError;
use crate::description::root_package;
use r_description::Description;
use r_metadata::Version;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct LocalRepository {
    path: PathBuf,
    description: Option<Arc<Description>>,
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

    pub fn with_description(mut self, description: Description) -> Self {
        self.set_description(description);
        self
    }

    pub fn set_description(&mut self, description: Description) {
        self.description = Some(Arc::new(description));
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn description(&self) -> Result<Arc<Description>, RepositoryError> {
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
        let description = Description::parse(&contents);

        Ok(Arc::new(description))
    }

    pub async fn package(&self) -> Result<(String, Version), RepositoryError> {
        let description = self.description().await?;
        let (package, version) = root_package(&self.path, &description)?;

        Ok((package, version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn package_uses_staged_description_and_preserves_repository() {
        let initial = Description::parse("Package: initial\nVersion: 0.1.0\n");
        let staged = Description::parse("Package: project\nVersion: 1.2.3\n");
        let mut repository =
            LocalRepository::new(PathBuf::from("unused")).with_description(initial);
        repository.set_description(staged);
        let repository = Arc::new(repository);
        let wrapped = super::super::PackageRepository::Local(repository.clone());
        let cloned = wrapped.clone();

        let description = repository
            .description()
            .await
            .expect("description should load");
        let (package, version) = repository.package().await.expect("package should load");

        assert_eq!(description.package().unwrap().as_str(), "project");
        assert_eq!(package, "project");
        assert_eq!(version.to_string(), "1.2.3");
        assert!(wrapped.same_instance(&cloned));
        let super::super::PackageRepository::Local(cloned) = cloned else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(
            &description,
            &cloned.description().await.unwrap()
        ));
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

        assert_eq!(description.package().unwrap().as_str(), "diskproject");
        tokio::fs::remove_dir_all(path)
            .await
            .expect("test directory should be removed");
    }
}

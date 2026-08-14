use crate::project::cache_dir_path;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::hash_map::DefaultHasher,
    fmt, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompiledPackageCacheKey {
    package: String,
    version: String,
    r_version: semver::Version,
    platform: String,
}

impl CompiledPackageCacheKey {
    pub fn new(package: &str, version: &str, r_version: &semver::Version) -> Self {
        Self::with_platform(package, version, r_version, host_platform_key())
    }

    pub fn with_platform(
        package: &str,
        version: &str,
        r_version: &semver::Version,
        platform: impl Into<String>,
    ) -> Self {
        Self {
            package: package.to_string(),
            version: version.to_string(),
            r_version: r_version.clone(),
            platform: platform.into(),
        }
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    fn cache_dir_name(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.package,
            self.version,
            self.platform,
            self.digest()
        )
    }

    fn digest(&self) -> String {
        let input = format!(
            "{}\n{}\n{}\n{}",
            self.package, self.version, self.r_version, self.platform
        );
        let mut hasher = DefaultHasher::new();
        input.hash(&mut hasher);

        format!("{:016x}", hasher.finish())
    }
}

impl fmt::Display for CompiledPackageCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.cache_dir_name())
    }
}

pub async fn exists(key: &CompiledPackageCacheKey) -> bool {
    package_cache_path(key).exists()
}

pub async fn restore(key: &CompiledPackageCacheKey, target_library: &Path) -> Result<(), String> {
    let source = package_cache_path(key);
    let target = target_library.join(key.package());

    tokio::task::spawn_blocking(move || copy_package_dir(&source, &target))
        .await
        .map_err(|error| format!("failed to join cache restore task: {error}"))?
}

pub async fn store(key: &CompiledPackageCacheKey, package_dir: &Path) -> Result<(), String> {
    let target = package_cache_path(key);
    let package_dir = package_dir.to_path_buf();

    tokio::task::spawn_blocking(move || copy_package_dir(&package_dir, &target))
        .await
        .map_err(|error| format!("failed to join cache store task: {error}"))?
}

pub fn repository_metadata_cache_path(repository_url: &str, parts: &[&str]) -> PathBuf {
    let mut path = cache_dir_path()
        .join("metadata")
        .join("repositories")
        .join(hash_string(repository_url));

    for part in parts {
        path = path.join(part);
    }

    path
}

pub async fn read_json_metadata_cache<T>(path: &Path, ttl: Option<Duration>) -> Option<T>
where
    T: DeserializeOwned,
{
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if let Some(ttl) = ttl
        && metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > ttl)
    {
        return None;
    }

    let contents = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&contents).ok()
}

pub async fn write_json_metadata_cache<T>(path: &Path, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("failed to create metadata cache directory: {error}"))?;
    }

    let contents = serde_json::to_string(value)
        .map_err(|error| format!("failed to serialize metadata cache: {error}"))?;
    tokio::fs::write(path, contents)
        .await
        .map_err(|error| format!("failed to write metadata cache: {error}"))
}

fn host_platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn package_cache_path(key: &CompiledPackageCacheKey) -> PathBuf {
    cache_dir_path()
        .join("builds")
        .join(key.cache_dir_name())
        .join("package")
}

fn hash_string(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn copy_package_dir(source: &Path, destination: &Path) -> Result<(), String> {
    if !source.exists() {
        return Err(format!(
            "cached package directory is missing: {}",
            source.display()
        ));
    }

    if destination.exists() {
        fs::remove_dir_all(destination)
            .map_err(|error| format!("failed to replace package directory: {error}"))?;
    }
    fs::create_dir_all(destination)
        .map_err(|error| format!("failed to create package directory: {error}"))?;

    for entry in fs::read_dir(source)
        .map_err(|error| format!("failed to read package directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed to read package entry: {error}"))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect package entry: {error}"))?;

        if file_type.is_dir() {
            copy_package_dir(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)
                .map_err(|error| format!("failed to copy package file: {error}"))?;
        }
    }

    Ok(())
}

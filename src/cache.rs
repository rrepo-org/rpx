use crate::project::cache_dir_path;
use std::{
    collections::hash_map::DefaultHasher,
    fmt, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

pub(crate) fn artifact_cache_path(package: &str, version: &str, file_name: &str) -> PathBuf {
    let path = cache_dir_path()
        .join("artifacts")
        .join(package)
        .join(version)
        .join(file_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create cache directory");
    }
    path
}

pub(crate) fn build_temp_library_path(package: &str, unique: &str) -> PathBuf {
    let path = cache_dir_path()
        .join("build-temp")
        .join(format!("{package}-{unique}"))
        .join("library");
    fs::create_dir_all(&path).expect("failed to create temporary build library");
    path
}

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

fn host_platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn package_cache_path(key: &CompiledPackageCacheKey) -> PathBuf {
    cache_dir_path()
        .join("builds")
        .join(key.cache_dir_name())
        .join("package")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    fn unique(name: &str) -> String {
        format!(
            "rpx-cache-test-{name}-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn remove_dir_if_present(path: &Path) {
        if path.exists() {
            fs::remove_dir_all(path).expect("test cache directory should be removed");
        }
    }

    #[test]
    fn artifact_cache_path_has_exact_layout_and_creates_only_parent() {
        let package = unique("artifact");
        let root = cache_dir_path().join("artifacts").join(&package);
        remove_dir_if_present(&root);
        let path = artifact_cache_path(&package, "1.2.3", "package.tar.gz");
        assert_eq!(path, root.join("1.2.3").join("package.tar.gz"));
        assert!(path.parent().expect("artifact should have parent").is_dir());
        assert!(!path.exists());
        remove_dir_if_present(&root);
    }

    #[test]
    fn build_temp_library_path_has_exact_layout_and_creates_directory() {
        let package = unique("build");
        let root = cache_dir_path()
            .join("build-temp")
            .join(format!("{package}-unique"));
        remove_dir_if_present(&root);
        let path = build_temp_library_path(&package, "unique");
        assert_eq!(path, root.join("library"));
        assert!(path.is_dir());
        remove_dir_if_present(&root);
    }
}

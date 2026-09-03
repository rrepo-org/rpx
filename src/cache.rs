use crate::{
    git::{GitOid, GitUrl},
    project::cache_dir_path,
};
use r_metadata::Version;
use semver::Version as RVersion;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
};
use target_lexicon::{OperatingSystem, Triple};
use url::Url;

const SOURCE_ARTIFACT_CACHE_VERSION: &str = "v1";
const BINARY_ARTIFACT_CACHE_VERSION: &str = "v1";
const COMPILED_PACKAGE_CACHE_VERSION: &str = "v1";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RegistryIdentity {
    Cran(Url),
    Rrepo(Url),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SourceArtifactIdentity {
    Registry(RegistryIdentity),
    Git {
        remote: GitUrl,
        commit: GitOid,
        subdirectory: Option<PathBuf>,
    },
    Local(PathBuf),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SourceArtifactCacheKey {
    source: SourceArtifactIdentity,
    package: String,
    version: Version,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct BinaryArtifactCacheKey {
    repository: RegistryIdentity,
    package: String,
    version: Version,
    target: Triple,
    r_version: RVersion,
}

impl SourceArtifactCacheKey {
    pub(crate) fn new(
        source: SourceArtifactIdentity,
        package: impl Into<String>,
        version: Version,
    ) -> Self {
        Self {
            source,
            package: package.into(),
            version,
        }
    }
}

impl BinaryArtifactCacheKey {
    pub(crate) fn new(
        repository: RegistryIdentity,
        package: impl Into<String>,
        version: Version,
        target: Triple,
        r_version: RVersion,
    ) -> Self {
        Self {
            repository,
            package: package.into(),
            version,
            target,
            r_version,
        }
    }
}

fn cache_key_digest(key: &impl Hash) -> String {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn source_artifact_cache_path(key: &SourceArtifactCacheKey) -> PathBuf {
    cache_dir_path()
        .join("artifacts")
        .join("source")
        .join(SOURCE_ARTIFACT_CACHE_VERSION)
        .join(&key.package)
        .join(cache_key_digest(key))
        .join("artifact.tar.gz")
}

pub(crate) fn binary_artifact_cache_path(key: &BinaryArtifactCacheKey) -> PathBuf {
    let file_name = match key.target.operating_system {
        OperatingSystem::Windows => format!("{}_{}.zip", key.package, key.version),
        OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => {
            format!("{}_{}.tgz", key.package, key.version)
        }
        _ => "artifact.bin".to_string(),
    };
    cache_dir_path()
        .join("artifacts")
        .join("binary")
        .join(BINARY_ARTIFACT_CACHE_VERSION)
        .join(&key.package)
        .join(cache_key_digest(key))
        .join(file_name)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CompiledPackageCacheKey {
    repository: RegistryIdentity,
    package: String,
    version: Version,
    target: Triple,
    r_version: RVersion,
}

impl CompiledPackageCacheKey {
    pub(crate) fn new(
        repository: RegistryIdentity,
        package: impl Into<String>,
        version: Version,
        target: Triple,
        r_version: RVersion,
    ) -> Self {
        Self {
            repository,
            package: package.into(),
            version,
            target,
            r_version,
        }
    }
}

pub(crate) fn compiled_package_cache_path(key: &CompiledPackageCacheKey) -> PathBuf {
    cache_dir_path()
        .join("builds")
        .join(COMPILED_PACKAGE_CACHE_VERSION)
        .join(&key.package)
        .join(cache_key_digest(key))
        .join("package")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

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
    fn source_artifact_cache_path_is_versioned_and_has_no_side_effects() {
        let package = unique("artifact");
        let root = cache_dir_path()
            .join("artifacts")
            .join("source")
            .join(SOURCE_ARTIFACT_CACHE_VERSION)
            .join(&package);
        remove_dir_if_present(&root);
        let key = SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(RegistryIdentity::Cran(
                "https://example.test/cran".parse().unwrap(),
            )),
            &package,
            "1.2.3".parse().unwrap(),
        );
        let path = source_artifact_cache_path(&key);
        assert_eq!(
            path.parent()
                .and_then(Path::parent)
                .expect("artifact should be nested below its package"),
            root
        );
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("artifact.tar.gz")
        );
        assert!(!root.exists());
        assert!(!path.exists());
    }

    #[test]
    fn source_artifact_key_uses_version_equivalence() {
        let key = |version: &str| {
            SourceArtifactCacheKey::new(
                SourceArtifactIdentity::Registry(RegistryIdentity::Cran(
                    "https://example.test/cran".parse().unwrap(),
                )),
                "package",
                version.parse().unwrap(),
            )
        };
        let hyphen = key("2.5-1");
        let trailing_zeroes = key("2.5.1.0");

        assert_eq!(hyphen, trailing_zeroes);
        assert_eq!(
            source_artifact_cache_path(&hyphen),
            source_artifact_cache_path(&trailing_zeroes)
        );
    }

    #[test]
    fn artifact_stores_use_distinct_compatibility_keys() {
        let repository = || RegistryIdentity::Cran("https://example.test/cran".parse().unwrap());
        let source = source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(repository()),
            "package",
            "1.2.3".parse().unwrap(),
        ));
        let other_repository = source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(RegistryIdentity::Cran(
                "https://mirror.example.test/cran".parse().unwrap(),
            )),
            "package",
            "1.2.3".parse().unwrap(),
        ));
        let windows_430 = binary_artifact_cache_path(&BinaryArtifactCacheKey::new(
            repository(),
            "package",
            "1.2.3".parse().unwrap(),
            "x86_64-pc-windows-msvc".parse().unwrap(),
            "4.3.0".parse().unwrap(),
        ));
        let windows_431 = binary_artifact_cache_path(&BinaryArtifactCacheKey::new(
            repository(),
            "package",
            "1.2.3".parse().unwrap(),
            "x86_64-pc-windows-msvc".parse().unwrap(),
            "4.3.1".parse().unwrap(),
        ));
        let windows_arm = binary_artifact_cache_path(&BinaryArtifactCacheKey::new(
            repository(),
            "package",
            "1.2.3".parse().unwrap(),
            "aarch64-pc-windows-msvc".parse().unwrap(),
            "4.3.0".parse().unwrap(),
        ));

        assert_ne!(source, other_repository);
        assert_ne!(source, windows_430);
        assert_ne!(windows_430, windows_431);
        assert_ne!(windows_430, windows_arm);
    }

    #[test]
    fn compiled_package_cache_uses_binary_compatibility_identity() {
        let repository = || RegistryIdentity::Cran("https://example.test/cran".parse().unwrap());
        let key = |repository, version: &str, target: &str, r_version: &str| {
            CompiledPackageCacheKey::new(
                repository,
                "package",
                version.parse().unwrap(),
                target.parse().unwrap(),
                r_version.parse().unwrap(),
            )
        };
        let base = compiled_package_cache_path(&key(
            repository(),
            "1.2.3",
            "x86_64-pc-windows-msvc",
            "4.3.0",
        ));

        assert_ne!(
            base,
            compiled_package_cache_path(&key(
                RegistryIdentity::Cran("https://mirror.example.test/cran".parse().unwrap()),
                "1.2.3",
                "x86_64-pc-windows-msvc",
                "4.3.0",
            ))
        );
        assert_ne!(
            base,
            compiled_package_cache_path(&key(
                repository(),
                "1.2.3",
                "aarch64-pc-windows-msvc",
                "4.3.0",
            ))
        );
        assert_ne!(
            base,
            compiled_package_cache_path(&key(
                repository(),
                "1.2.3",
                "x86_64-pc-windows-msvc",
                "4.3.1",
            ))
        );
        assert_eq!(
            base,
            compiled_package_cache_path(&key(
                repository(),
                "1.2.3.0",
                "x86_64-pc-windows-msvc",
                "4.3.0",
            ))
        );
    }
}

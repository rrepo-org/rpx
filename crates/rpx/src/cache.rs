use crate::{
    git::{GitOid, GitUrl},
    project::cache_dir_path,
    repository::PackageRepository,
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
pub(crate) const INSTALLER_CACHE_VERSION: &str = "v1";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RegistryCacheKey {
    // Preserve the v1 registry enum's discriminant encoding, without using a
    // second repository enum for dispatch. These tags are part of the cache ABI.
    kind: isize,
    url: Url,
}

impl RegistryCacheKey {
    pub(crate) fn from_repository(repository: &PackageRepository) -> Option<Self> {
        match repository {
            PackageRepository::Cran(repo) => Some(Self {
                kind: 0,
                url: repo.url().clone(),
            }),
            PackageRepository::Rrepo(repo) => Some(Self {
                kind: 1,
                url: repo.url().clone(),
            }),
            PackageRepository::Git(_) | PackageRepository::Local(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SourceArtifactIdentity {
    Registry(RegistryCacheKey),
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
    repository: RegistryCacheKey,
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
        repository: RegistryCacheKey,
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

pub(crate) fn installer_cache_path() -> PathBuf {
    cache_dir_path()
        .join("installer")
        .join(INSTALLER_CACHE_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cran(value: &str) -> RegistryCacheKey {
        RegistryCacheKey {
            kind: 0,
            url: value.parse().unwrap(),
        }
    }
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
            SourceArtifactIdentity::Registry(cran("https://example.test/cran")),
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
                SourceArtifactIdentity::Registry(cran("https://example.test/cran")),
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
        let repository = || cran("https://example.test/cran");
        let source = source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(repository()),
            "package",
            "1.2.3".parse().unwrap(),
        ));
        let other_repository = source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(cran("https://mirror.example.test/cran")),
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
    fn installer_cache_is_versioned() {
        assert!(installer_cache_path().ends_with("installer/v1"));
    }

    #[test]
    fn registry_key_encoding_matches_pre_enum_refactor() {
        #[derive(Hash)]
        enum LegacyRegistry {
            Cran(Url),
            Rrepo(Url),
        }
        #[derive(Hash)]
        #[allow(dead_code)]
        enum LegacySource {
            Registry(LegacyRegistry),
            Git {
                remote: GitUrl,
                commit: GitOid,
                subdirectory: Option<PathBuf>,
            },
            Local(PathBuf),
        }
        let url: Url = "https://example.test/repository".parse().unwrap();
        [
            (0, LegacyRegistry::Cran(url.clone())),
            (1, LegacyRegistry::Rrepo(url.clone())),
        ]
        .into_iter()
        .for_each(|(kind, old)| {
            let new = RegistryCacheKey {
                kind,
                url: url.clone(),
            };
            assert_eq!(cache_key_digest(&old), cache_key_digest(&new));
            let version: Version = "1.2.3".parse().unwrap();
            assert_eq!(
                cache_key_digest(&(LegacySource::Registry(old), "package", &version)),
                cache_key_digest(&SourceArtifactCacheKey::new(
                    SourceArtifactIdentity::Registry(new),
                    "package",
                    version
                ))
            );
        });
    }
}

use directories::ProjectDirs;
use miette::Diagnostic;
use r_description::lossless::{RDescription, Relation, Version};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::{
    lockfile::{
        LOCKFILE_VERSION, LockedRepository, LockedRepositoryKind, Lockfile,
        locked_repository_for_source,
    },
    r::{RVersionError, r_version_async},
    repository::{
        ArchiveSupport, BUILT_IN_REPOSITORY_BASE_URL, CranRepository, LocalRepository,
        PackageRepository, RrepoRepository, parse_repository_url,
    },
    resolver::{PackageVersion, is_base_package},
};

pub const LOCKFILE_NAME: &str = "rpx.lock";
pub const DESCRIPTION_NAME: &str = "DESCRIPTION";

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectDiscoveryError {
    #[error("failed to get current directory: {source}")]
    #[diagnostic(code(rpx::project::current_dir_failed))]
    CurrentDirectory {
        #[source]
        source: std::io::Error,
    },

    #[error("project root not found in current directory or any parent directory")]
    #[diagnostic(code(rpx::project::not_found))]
    NotFound,
}

#[derive(Debug, Error, Diagnostic)]
pub enum ManifestReadError {
    #[error("failed to read DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::manifest_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::manifest_parse_failed))]
    Parse {
        path: PathBuf,
        #[source]
        source: r_description::lossless::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockfileReadError {
    #[error("rpx.lock not found at {}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_not_found))]
    NotFound { path: PathBuf },

    #[error("failed to read rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_parse_failed))]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "unsupported rpx.lock schema version {version} at {} (supported version: {supported})",
        path.display()
    )]
    #[diagnostic(code(rpx::project::lockfile_unsupported_version))]
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
        supported: u32,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum ManifestWriteError {
    #[error("failed to write DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::manifest_write_failed))]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockfileWriteError {
    #[error("failed to serialize rpx.lock: {source}")]
    #[diagnostic(code(rpx::project::lockfile_serialize_failed))]
    Serialize {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to write rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_write_failed))]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockedResolutionError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Manifest(#[from] ManifestReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lockfile(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error("repository configuration no longer matches rpx.lock")]
    #[diagnostic(
        code(rpx::project::repositories_changed),
        help("Run `rpx lock` to update rpx.lock.")
    )]
    RepositoriesChanged,

    #[error("package requirements in DESCRIPTION no longer match rpx.lock")]
    #[diagnostic(
        code(rpx::project::requirements_changed),
        help("Run `rpx lock` to update rpx.lock.")
    )]
    PackageRequirementsChanged,

    #[error("rpx.lock was generated for R {locked}, but current R is {current}")]
    #[diagnostic(
        code(rpx::project::r_version_changed),
        help("Run `rpx lock` to update rpx.lock.")
    )]
    RVersionChanged { locked: String, current: String },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockedPackagesError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Manifest(#[from] ManifestReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lockfile(#[from] LockfileReadError),

    #[error("DESCRIPTION at {} is missing required field {field}", path.display())]
    #[diagnostic(code(rpx::project::manifest_missing_field))]
    MissingField { path: PathBuf, field: &'static str },

    #[error("invalid Version in DESCRIPTION at {}: {details}", path.display())]
    #[diagnostic(code(rpx::project::manifest_invalid_version))]
    InvalidVersion { path: PathBuf, details: String },

    #[error("invalid locked version {version} for {package}: {details}")]
    #[diagnostic(code(rpx::project::locked_package_invalid_version))]
    InvalidLockedVersion {
        package: String,
        version: String,
        details: String,
    },

    #[error("locked package {package} is missing source_url")]
    #[diagnostic(code(rpx::project::locked_package_missing_source_url))]
    MissingSourceUrl { package: String },

    #[error(
        "locked package {package} source URL does not match any locked repository: {source_url}"
    )]
    #[diagnostic(code(rpx::project::locked_package_repository_not_found))]
    RepositoryNotFound { package: String, source_url: String },

    #[error("invalid locked repository URL {url}: {details}")]
    #[diagnostic(code(rpx::project::locked_repository_invalid_url))]
    InvalidRepository { url: String, details: String },
}

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Discovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Manifest(#[from] ManifestReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lockfile(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ManifestWrite(#[from] ManifestWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileWrite(#[from] LockfileWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),
}

pub struct Project {
    path: PathBuf,
    description: OnceCell<RDescription>,
    lockfile: OnceCell<Lockfile>,
}

impl Project {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            description: OnceCell::new(),
            lockfile: OnceCell::new(),
        }
    }

    pub fn discover() -> Result<Self, ProjectDiscoveryError> {
        let current_dir = env::current_dir()
            .map_err(|source| ProjectDiscoveryError::CurrentDirectory { source })?;

        current_dir
            .ancestors()
            .find(|directory| {
                directory.join(".git").exists()
                    || directory.join(DESCRIPTION_NAME).is_file()
                    || directory.join(LOCKFILE_NAME).is_file()
            })
            .map(|directory| Self::new(directory.to_path_buf()))
            .ok_or(ProjectDiscoveryError::NotFound)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn description(&self) -> Result<&RDescription, ManifestReadError> {
        if let Some(description) = self.description.get() {
            return Ok(description);
        }

        let path = self.path.join(DESCRIPTION_NAME);
        let contents = fs::read_to_string(&path).map_err(|source| ManifestReadError::Read {
            path: path.clone(),
            source,
        })?;
        let description = contents
            .parse()
            .map_err(|source| ManifestReadError::Parse { path, source })?;

        Ok(self.description.get_or_init(|| description))
    }

    pub fn lockfile(&self) -> Result<&Lockfile, LockfileReadError> {
        let lockfile = self.read_lockfile()?;
        if lockfile.version != LOCKFILE_VERSION {
            return Err(LockfileReadError::UnsupportedVersion {
                path: self.path.join(LOCKFILE_NAME),
                version: lockfile.version,
                supported: LOCKFILE_VERSION,
            });
        }

        Ok(lockfile)
    }

    fn read_lockfile(&self) -> Result<&Lockfile, LockfileReadError> {
        if let Some(lockfile) = self.lockfile.get() {
            return Ok(lockfile);
        }

        let path = self.path.join(LOCKFILE_NAME);
        let contents = fs::read_to_string(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                LockfileReadError::NotFound { path: path.clone() }
            } else {
                LockfileReadError::Read {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        let lockfile = serde_json::from_str::<Lockfile>(&contents).map_err(|source| {
            LockfileReadError::Parse {
                path: path.clone(),
                source,
            }
        })?;

        Ok(self.lockfile.get_or_init(|| lockfile))
    }

    pub fn lockfile_optional(&self) -> Result<Option<&Lockfile>, LockfileReadError> {
        let path = self.path.join(LOCKFILE_NAME);
        if !path
            .try_exists()
            .map_err(|source| LockfileReadError::Read {
                path: path.clone(),
                source,
            })?
        {
            return Ok(None);
        }

        self.read_lockfile().map(Some)
    }

    pub fn write_description(&self, description: &RDescription) -> Result<(), ManifestWriteError> {
        let path = self.path.join(DESCRIPTION_NAME);
        fs::write(&path, description.to_string())
            .map_err(|source| ManifestWriteError::Write { path, source })
    }

    pub fn write_lockfile(&self, lockfile: &Lockfile) -> Result<(), LockfileWriteError> {
        let contents = serde_json::to_string_pretty(lockfile)
            .map_err(|source| LockfileWriteError::Serialize { source })?;
        let path = self.path.join(LOCKFILE_NAME);
        fs::write(&path, format!("{contents}\n"))
            .map_err(|source| LockfileWriteError::Write { path, source })
    }

    pub fn locked_packages(&self) -> Result<BTreeMap<String, PackageVersion>, LockedPackagesError> {
        let lockfile = self.lockfile()?;
        let mut packages = lockfile
            .packages
            .iter()
            .filter(|(name, _)| !is_base_package(name))
            .map(|(name, package)| {
                let version = package.version.parse().map_err(|details| {
                    LockedPackagesError::InvalidLockedVersion {
                        package: name.clone(),
                        version: package.version.clone(),
                        details,
                    }
                })?;
                let source_url = package.source_url.as_deref().ok_or_else(|| {
                    LockedPackagesError::MissingSourceUrl {
                        package: name.clone(),
                    }
                })?;
                let repository = locked_repository_for_source(source_url, &lockfile.repositories)
                    .ok_or_else(|| LockedPackagesError::RepositoryNotFound {
                    package: name.clone(),
                    source_url: source_url.to_string(),
                })?;
                let repository = package_repository(repository)?;

                Ok((name.clone(), PackageVersion::new(version, repository)))
            })
            .collect::<Result<BTreeMap<_, _>, LockedPackagesError>>()?;

        let (package, version) = self.root_package()?;
        packages.insert(package, version);

        Ok(packages)
    }

    fn root_package(&self) -> Result<(String, PackageVersion), LockedPackagesError> {
        let description = self.description()?;
        let path = self.path.join(DESCRIPTION_NAME);
        let package = description
            .package()
            .ok_or_else(|| LockedPackagesError::MissingField {
                path: path.clone(),
                field: "Package",
            })?;
        let version = description
            .version()
            .ok_or_else(|| LockedPackagesError::MissingField {
                path: path.clone(),
                field: "Version",
            })?
            .parse::<Version>()
            .map_err(|details| LockedPackagesError::InvalidVersion { path, details })?;
        let repository: Arc<dyn PackageRepository> =
            Arc::new(LocalRepository::new(self.path.clone()).with_description(description.clone()));

        Ok((package, PackageVersion::new(version, repository)))
    }

    pub async fn validate_locked_resolution(&self) -> Result<(), LockedResolutionError> {
        let lockfile = self.lockfile()?;
        let description = self.description()?;
        let r_version = r_version_async()
            .await
            .map_err(LockedResolutionError::RVersion)?;

        if !repositories_match(description, lockfile) {
            return Err(LockedResolutionError::RepositoriesChanged);
        }
        if !roots_match(description, lockfile) {
            return Err(LockedResolutionError::PackageRequirementsChanged);
        }
        if lockfile.r.version != r_version {
            return Err(LockedResolutionError::RVersionChanged {
                locked: lockfile.r.version.clone(),
                current: r_version,
            });
        }

        Ok(())
    }
}

fn package_repository(
    repository: &LockedRepository,
) -> Result<Arc<dyn PackageRepository>, LockedPackagesError> {
    let url = parse_repository_url(&repository.url).map_err(|error| {
        LockedPackagesError::InvalidRepository {
            url: repository.url.clone(),
            details: error.to_string(),
        }
    })?;

    Ok(match repository.kind {
        LockedRepositoryKind::Rrepo => Arc::new(RrepoRepository::new(url)),
        LockedRepositoryKind::CranLike => Arc::new(CranRepository::new(
            url,
            repository
                .cran_archive_support
                .unwrap_or(ArchiveSupport::Unavailable),
        )),
    })
}

fn repositories_match(description: &RDescription, lockfile: &Lockfile) -> bool {
    let Some(mut expected) = description
        .additional_repositories()
        .unwrap_or_default()
        .iter()
        .map(|repository| canonical_repository_url(repository))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let Some(locked) = lockfile
        .repositories
        .iter()
        .map(|repository| canonical_repository_url(&repository.url))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let Some(default_repository) = canonical_repository_url(BUILT_IN_REPOSITORY_BASE_URL) else {
        return false;
    };

    if locked.contains(&default_repository) && !expected.contains(&default_repository) {
        expected.insert(0, default_repository);
    }

    locked == expected
}

fn canonical_repository_url(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value.trim()).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty();
    Some(url.to_string())
}

fn roots_match(description: &RDescription, lockfile: &Lockfile) -> bool {
    let description_roots = description_roots(description);
    let lockfile_roots = lockfile
        .roots
        .iter()
        .map(|root| {
            let constraint = root.constraint.trim();
            if constraint.is_empty() || constraint == "*" {
                Some(Relation::simple(&root.package))
            } else {
                format!("{} ({constraint})", root.package).parse().ok()
            }
        })
        .collect::<Option<BTreeSet<_>>>();

    lockfile_roots.is_some_and(|roots| roots == description_roots)
}

fn description_roots(description: &RDescription) -> BTreeSet<Relation> {
    description
        .imports()
        .into_iter()
        .flat_map(|relations| relations.iter())
        .chain(
            description
                .depends()
                .into_iter()
                .flat_map(|relations| relations.iter()),
        )
        .chain(
            description
                .linking_to()
                .into_iter()
                .flat_map(|relations| relations.iter()),
        )
        .chain(
            description
                .suggests()
                .into_iter()
                .flat_map(|relations| relations.iter()),
        )
        .filter(|relation| relation.name() != "R")
        .collect()
}

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectPathError {
    #[error("failed to get current directory: {source}")]
    #[diagnostic(code(rpx::project::current_dir_failed))]
    CurrentDirFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("{DESCRIPTION_NAME} not found in current directory or any parent directory")]
    #[diagnostic(code(rpx::project::description_not_found))]
    DescriptionNotFound,
}

pub fn new_project_description_path() -> Result<PathBuf, ProjectPathError> {
    Ok(current_dir()?.join(DESCRIPTION_NAME))
}

pub fn project_library_path() -> PathBuf {
    let library_path = project_library_root_path().join("library");

    fs::create_dir_all(&library_path).expect("failed to create project library");
    library_path
}

pub fn project_library_root_path() -> PathBuf {
    let project_key = hash_path(&project_root());
    project_dirs()
        .data_dir()
        .join("libraries")
        .join(project_key)
}

pub fn cache_dir_path() -> PathBuf {
    project_dirs().cache_dir().to_path_buf()
}

pub fn artifact_cache_path(package: &str, version: &str, file_name: &str) -> PathBuf {
    let path = project_dirs()
        .cache_dir()
        .join("artifacts")
        .join(package)
        .join(version)
        .join(file_name);
    ensure_parent_dir(&path);
    path
}

pub fn build_temp_library_path(package: &str, unique: &str) -> PathBuf {
    let path = project_dirs()
        .cache_dir()
        .join("build-temp")
        .join(format!("{package}-{unique}"))
        .join("library");
    fs::create_dir_all(&path).expect("failed to create temporary build library");
    path
}

#[deprecated]
pub fn project_root() -> PathBuf {
    project_root_result().unwrap_or_else(|error| panic!("{error}"))
}

#[deprecated]
pub fn project_root_result() -> Result<PathBuf, ProjectPathError> {
    let current_dir = current_dir()?;
    let current_dir = current_dir
        .canonicalize()
        .unwrap_or_else(|_| current_dir.clone());

    for candidate in current_dir.ancestors() {
        if candidate.join(DESCRIPTION_NAME).exists() {
            return Ok(candidate.to_path_buf());
        }
    }

    Err(ProjectPathError::DescriptionNotFound)
}

#[deprecated]
fn current_dir() -> Result<PathBuf, ProjectPathError> {
    env::current_dir().map_err(|source| ProjectPathError::CurrentDirFailed { source })
}

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("de", "scalerail", "rpx").expect("failed to resolve rpx data directory")
}

fn ensure_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create cache directory");
    }
}

fn hash_path(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn project_directory(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rpx-project-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("project directory should be created");
        path
    }

    #[test]
    fn caches_description_and_lockfile_after_first_read() {
        let path = project_directory("cached-files");
        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: initial\nVersion: 1.0.0\n",
        )
        .expect("DESCRIPTION should be written");
        fs::write(
            path.join(LOCKFILE_NAME),
            r#"{"version":4,"revision":1,"roots":[],"packages":{}}"#,
        )
        .expect("lockfile should be written");
        let project = Project::new(path.clone());

        assert_eq!(
            project
                .description()
                .expect("DESCRIPTION should load")
                .package()
                .as_deref(),
            Some("initial")
        );
        assert_eq!(
            project.lockfile().expect("lockfile should load").revision,
            1
        );

        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: changed\nVersion: 2.0.0\n",
        )
        .expect("DESCRIPTION should be replaced");
        fs::write(
            path.join(LOCKFILE_NAME),
            r#"{"version":4,"revision":2,"roots":[],"packages":{}}"#,
        )
        .expect("lockfile should be replaced");

        assert_eq!(
            project
                .description()
                .expect("cached DESCRIPTION should be returned")
                .package()
                .as_deref(),
            Some("initial")
        );
        assert_eq!(
            project
                .lockfile()
                .expect("cached lockfile should be returned")
                .revision,
            1
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn reads_optional_lockfile_and_writes_project_files_at_root() {
        let path = project_directory("project-files");
        let project = Project::new(path.clone());
        assert!(
            project
                .lockfile_optional()
                .expect("missing lockfile should be allowed")
                .is_none()
        );

        let description = "Package: project\nVersion: 1.0.0\n"
            .parse::<RDescription>()
            .expect("DESCRIPTION should parse");
        let lockfile = serde_json::from_str::<Lockfile>(
            r#"{"version":4,"revision":1,"roots":[],"packages":{}}"#,
        )
        .expect("lockfile should parse");

        project
            .write_description(&description)
            .expect("DESCRIPTION should be written");
        project
            .write_lockfile(&lockfile)
            .expect("lockfile should be written");

        assert_eq!(
            fs::read_to_string(path.join(DESCRIPTION_NAME))
                .expect("DESCRIPTION should be readable"),
            description.to_string()
        );
        assert_eq!(
            project
                .lockfile_optional()
                .expect("lockfile should load")
                .expect("lockfile should exist")
                .revision,
            1
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn optional_lockfile_allows_migration_from_an_older_schema() {
        let path = project_directory("old-optional-lockfile");
        fs::write(
            path.join(LOCKFILE_NAME),
            r#"{"version":3,"roots":[],"packages":{}}"#,
        )
        .expect("lockfile should be written");
        let project = Project::new(path.clone());

        assert_eq!(
            project
                .lockfile_optional()
                .expect("optional lockfile should load")
                .expect("lockfile should exist")
                .version,
            3
        );
        assert!(matches!(
            project.lockfile(),
            Err(LockfileReadError::UnsupportedVersion { version: 3, .. })
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn locked_packages_include_repository_provenance_and_project() {
        let path = project_directory("locked-packages");
        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: project\nVersion: 2.0.0\n",
        )
        .expect("DESCRIPTION should be written");
        fs::write(
            path.join(LOCKFILE_NAME),
            r#"{
                "version": 4,
                "repositories": [
                    {"url": "https://repo.test/example", "kind": "rrepo"},
                    {"url": "https://repo.test/example2", "kind": "rrepo"}
                ],
                "roots": [],
                "packages": {
                    "digest": {
                        "package": "digest",
                        "version": "0.6.39",
                        "source_url": "https://repo.test/example2/packages/digest/source"
                    },
                    "project": {
                        "package": "project",
                        "version": "1.0.0",
                        "source_url": "https://repo.test/example/packages/project/source"
                    },
                    "stats": {"package": "stats", "version": "4.5.0"}
                }
            }"#,
        )
        .expect("lockfile should be written");
        let project = Project::new(path.clone());

        let packages = project
            .locked_packages()
            .expect("locked packages should load");

        assert_eq!(packages.len(), 2);
        let digest = packages.get("digest").expect("digest should be locked");
        assert_eq!(digest.version().to_string(), "0.6.39");
        assert_eq!(
            digest
                .repository()
                .as_ref()
                .downcast_ref::<RrepoRepository>()
                .expect("digest should use an rrepo repository")
                .url()
                .as_str(),
            "https://repo.test/example2"
        );
        let root = packages.get("project").expect("project should be locked");
        assert_eq!(root.version().to_string(), "2.0.0");
        assert_eq!(
            root.repository()
                .as_ref()
                .downcast_ref::<LocalRepository>()
                .expect("project should use its local repository")
                .path(),
            path
        );
        assert!(!packages.contains_key("stats"));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }
}

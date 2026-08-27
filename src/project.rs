use directories::ProjectDirs;
use miette::Diagnostic;
use pubgrub::{DefaultStringReporter, PubGrubError, Reporter};
use r_description::{RDescription, Relation, Version, VersionRequirement};
use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use thiserror::Error;

use crate::{
    description::{
        ConfiguredRepository, DESCRIPTION_NAME, DependencyField, DescriptionParseError,
        DescriptionReadError, RepositoriesFromDescriptionError, add_dependencies,
        configured_repositories, project_dependencies, read_description,
        repositories_from_description,
    },
    git,
    lockfile::{self, LOCKFILE_NAME, Lockfile, LockfileReadError, read_lockfile},
    r::{BasePackagesError, RVersionError, r_version_async},
    repository::{GitRepository, LocalRepository, PackageRepository, RepositoryError},
    resolver::{PackageVersion, ProviderError, ResolutionError, resolve_from_registry},
};

pub type RequiredPackages = BTreeMap<String, (PackageVersion, Arc<RDescription>)>;

static NEXT_STAGED_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PackageVersionMismatch {
    pub(crate) package: String,
    pub(crate) installed: Version,
    pub(crate) expected: Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LibraryMismatches {
    pub(crate) missing_packages: Vec<String>,
    pub(crate) extra_packages: Vec<String>,
    pub(crate) version_mismatches: Vec<PackageVersionMismatch>,
}

impl LibraryMismatches {
    pub(crate) fn is_exact(&self) -> bool {
        self.missing_packages.is_empty()
            && self.extra_packages.is_empty()
            && self.version_mismatches.is_empty()
    }

    pub(crate) fn is_runnable(&self) -> bool {
        self.missing_packages.is_empty() && self.version_mismatches.is_empty()
    }

    pub(crate) fn into_runtime_mismatches(mut self) -> Self {
        self.extra_packages.clear();
        self
    }
}

impl std::fmt::Display for LibraryMismatches {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut groups = Vec::new();
        if !self.missing_packages.is_empty() {
            groups.push(format!(
                "Required packages not installed:\n- {}",
                self.missing_packages.join("\n- ")
            ));
        }
        if !self.extra_packages.is_empty() {
            groups.push(format!(
                "Unexpected packages installed:\n- {}",
                self.extra_packages.join("\n- ")
            ));
        }
        if !self.version_mismatches.is_empty() {
            let mismatches = self
                .version_mismatches
                .iter()
                .map(|mismatch| {
                    format!(
                        "{} ({} installed, {} expected)",
                        mismatch.package, mismatch.installed, mismatch.expected
                    )
                })
                .collect::<Vec<_>>()
                .join("\n- ");
            groups.push(format!(
                "Installed versions that differ from expected versions:\n- {mismatches}"
            ));
        }
        formatter.write_str(&groups.join("\n\n"))
    }
}

pub(crate) fn library_mismatches(
    expected_packages: &BTreeMap<String, Version>,
    installed: &BTreeMap<String, PackageVersion>,
    ignored_extra_package: Option<&str>,
) -> LibraryMismatches {
    let missing_packages = expected_packages
        .keys()
        .filter(|package| !installed.contains_key(*package))
        .cloned()
        .collect();
    let version_mismatches = expected_packages
        .iter()
        .filter_map(|(package, expected)| {
            installed
                .get(package)
                .filter(|installed| installed.version() != expected)
                .map(|installed| PackageVersionMismatch {
                    package: package.clone(),
                    installed: installed.version().clone(),
                    expected: expected.clone(),
                })
        })
        .collect();
    let extra_packages = installed
        .keys()
        .filter(|package| {
            ignored_extra_package != Some(package.as_str())
                && !expected_packages.contains_key(*package)
        })
        .cloned()
        .collect();

    LibraryMismatches {
        missing_packages,
        extra_packages,
        version_mismatches,
    }
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum ProjectLibraryError {
    #[error("failed to create project library at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::library_create_failed))]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectDiscoveryError {
    #[error("failed to determine the current working directory: {source}")]
    #[diagnostic(code(rpx::project::working_directory_unavailable))]
    WorkingDirectoryUnavailable {
        #[source]
        source: std::io::Error,
    },

    #[error("failed to inspect project root marker at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::root_marker_metadata))]
    RootMarkerMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "project root not found from {} or any parent directory",
        start.display()
    )]
    #[diagnostic(
        code(rpx::project::root_not_found),
        help("Expected a .git, DESCRIPTION, or rpx.lock root marker.")
    )]
    RootNotFound { start: PathBuf },
}

pub fn find_project_root() -> Result<PathBuf, ProjectDiscoveryError> {
    let current_dir = env::current_dir()
        .map_err(|source| ProjectDiscoveryError::WorkingDirectoryUnavailable { source })?;

    current_dir
        .ancestors()
        .find_map(|directory| {
            [
                (".git", false),
                (DESCRIPTION_NAME, true),
                (LOCKFILE_NAME, true),
            ]
            .into_iter()
            .find_map(|(name, file_only)| {
                let path = directory.join(name);
                match fs::metadata(&path) {
                    Ok(metadata) if !file_only || metadata.is_file() => {
                        Some(Ok(directory.to_path_buf()))
                    }
                    Ok(_) => None,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
                    Err(source) => Some(Err(ProjectDiscoveryError::RootMarkerMetadata {
                        path,
                        source,
                    })),
                }
            })
        })
        .unwrap_or_else(|| Err(ProjectDiscoveryError::RootNotFound { start: current_dir }))
}

#[derive(Debug)]
pub(crate) struct Project {
    pub(crate) root: PathBuf,
    pub(crate) description: RDescription,
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum ProjectLoadError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Discovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionReadError),
}

pub(crate) fn load_project() -> Result<Project, ProjectLoadError> {
    let root = find_project_root()?;
    let description = read_description(&root)?;
    Ok(Project { root, description })
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum LockedPackagesError {
    #[error("locked package {package} references missing repository {repository}")]
    #[diagnostic(code(rpx::project::locked_package_repository_not_found))]
    RepositoryNotFound {
        package: String,
        repository: url::Url,
    },

    #[error("failed to reconstruct repository for locked package {package}: {source}")]
    #[diagnostic(code(rpx::project::locked_package_repository_invalid))]
    Repository {
        package: String,
        #[source]
        source: RepositoryError,
    },

    #[error("failed to reconstruct DESCRIPTION for locked package {package}: {details}")]
    #[diagnostic(code(rpx::project::locked_package_description_invalid))]
    InvalidLockedDescription { package: String, details: String },
}

fn required_packages_from_lockfile(
    lockfile: &Lockfile,
) -> Result<RequiredPackages, LockedPackagesError> {
    let packages = lockfile
        .packages
        .iter()
        .map(|(name, package)| {
            let repository = lockfile
                .repos
                .iter()
                .find(|repository| repository.url() == &package.repository)
                .ok_or_else(|| LockedPackagesError::RepositoryNotFound {
                    package: name.clone(),
                    repository: package.repository.clone(),
                })?;
            let repository =
                <dyn PackageRepository>::from_lockfile(repository).map_err(|source| {
                    LockedPackagesError::Repository {
                        package: name.clone(),
                        source,
                    }
                })?;

            let description = locked_package_description(name, package)?;

            Ok((
                name.clone(),
                (
                    PackageVersion::new(package.version.clone(), repository),
                    Arc::new(description),
                ),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, LockedPackagesError>>()?;

    Ok(packages)
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockedResolutionError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Parse(#[from] DescriptionParseError),

    #[error("rpx.lock does not match the current project configuration")]
    #[diagnostic(
        code(rpx::project::locked_resolution_invalid),
        help(
            "Run `rpx lock` to update rpx.lock, then `rpx sync` to synchronize the project library."
        )
    )]
    Validation {
        #[related]
        failures: Vec<LockedResolutionFailure>,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockedResolutionFailure {
    #[error("locked repository {repository} at index {index} is invalid: {source}")]
    #[diagnostic(code(rpx::project::locked_repository_invalid))]
    InvalidRepository {
        index: usize,
        repository: url::Url,
        #[source]
        source: RepositoryError,
    },

    #[error("repository configuration no longer matches rpx.lock")]
    #[diagnostic(code(rpx::project::repositories_changed))]
    RepositoriesChanged,

    #[error("package requirements in DESCRIPTION no longer match rpx.lock")]
    #[diagnostic(code(rpx::project::requirements_changed))]
    PackageRequirementsChanged,

    #[error("rpx.lock was generated for R {locked}, but current R is {current}")]
    #[diagnostic(code(rpx::project::r_version_changed))]
    RVersionChanged {
        locked: semver::Version,
        current: semver::Version,
    },
}

pub fn validate_locked_resolution(
    project_path: &PathBuf,
    description: &RDescription,
    r_version: &semver::Version,
    lockfile: &Lockfile,
) -> Result<(), LockedResolutionError> {
    let repositories = configured_repositories(project_path, description)?;
    let repository_validation = lockfile
        .repos
        .iter()
        .zip(&repositories)
        .enumerate()
        .map(|(index, (locked, configured))| {
            let matches = match configured {
                ConfiguredRepository::Base(url) | ConfiguredRepository::Additional(url) => {
                    Ok::<_, RepositoryError>(
                        matches!(
                            locked,
                            lockfile::Repository::Rrepo { .. }
                                | lockfile::Repository::CranLike { .. }
                        ) && locked.url() == url,
                    )
                }
                ConfiguredRepository::Git(remote) => {
                    GitRepository::new(remote.clone()).and_then(|current| match locked {
                        lockfile::Repository::Git { .. } => {
                            <dyn PackageRepository>::from_lockfile(locked)
                                .map(|locked| locked.equals(&current))
                        }
                        _ => Ok(false),
                    })
                }
            };

            matches.map_err(|source| LockedResolutionFailure::InvalidRepository {
                index,
                repository: locked.url().clone(),
                source,
            })
        })
        .collect::<Vec<_>>();
    let repositories_changed = lockfile.repos.len() != repositories.len()
        || repository_validation
            .iter()
            .any(|result| matches!(result, Ok(false)));
    let roots = project_dependencies(project_path, description)?;
    let failures = repository_validation
        .into_iter()
        .filter_map(Result::err)
        .chain(repositories_changed.then_some(LockedResolutionFailure::RepositoriesChanged))
        .chain(
            (lockfile.requirements != roots)
                .then_some(LockedResolutionFailure::PackageRequirementsChanged),
        )
        .chain(
            (&lockfile.r != r_version).then(|| LockedResolutionFailure::RVersionChanged {
                locked: lockfile.r.clone(),
                current: r_version.clone(),
            }),
        )
        .collect::<Vec<_>>();

    if failures.is_empty() {
        Ok(())
    } else {
        Err(LockedResolutionError::Validation { failures })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolutionPolicy {
    AlwaysResolve,
    ReuseIfValid,
}

#[derive(Debug)]
pub(crate) struct ProjectResolution {
    pub(crate) lockfile: Lockfile,
    pub(crate) lockfile_changed: bool,
    pub(crate) packages: RequiredPackages,
    pub(crate) r_version: semver::Version,
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum LoadProjectResolutionError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum ResolveProjectError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    PreviousLockfile(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repositories(#[from] RepositoriesFromDescriptionError),

    #[error("failed to reconstruct repository from rpx.lock: {source}")]
    #[diagnostic(code(rpx::lock::repository_failed))]
    LockedRepository {
        #[source]
        source: RepositoryError,
    },

    #[error("failed to load resolved package metadata: {source}")]
    #[diagnostic(code(rpx::project::package_metadata_failed))]
    PackageMetadata {
        #[source]
        source: RepositoryError,
    },

    #[error("package requirements are incompatible\n\n{explanation}")]
    #[diagnostic(
        code(rpx::lock::no_solution),
        help("Adjust package constraints in DESCRIPTION and try again.")
    )]
    NoSolution { explanation: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error("could not access Git repository {repository}")]
    #[diagnostic(
        code(rpx::lock::git_repository_unavailable),
        help(
            "Check that the repository exists. For private repositories, configure Git credentials."
        )
    )]
    GitRepositoryUnavailable {
        repository: String,
        #[source]
        source: Box<ResolutionError>,
    },

    #[error("failed to resolve package set")]
    #[diagnostic(
        code(rpx::lock::resolve_failed),
        help("Check package names and version constraints in DESCRIPTION.")
    )]
    Resolution {
        #[source]
        source: Box<ResolutionError>,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    BuildLockfile(#[from] LockfileBuildError),
}

impl From<ResolutionError> for ResolveProjectError {
    fn from(error: ResolutionError) -> Self {
        match error {
            ResolutionError::PubGrub(PubGrubError::NoSolution(mut derivation_tree)) => {
                derivation_tree.collapse_no_versions();
                Self::NoSolution {
                    explanation: DefaultStringReporter::report(&derivation_tree),
                }
            }
            ResolutionError::BasePackages(source) => Self::BasePackages(source),
            ResolutionError::Provider(provider) => {
                let repository = match &provider {
                    ProviderError::Repository(source)
                    | ProviderError::DependencyMetadata { source, .. } => {
                        inaccessible_git_repository(source).map(str::to_owned)
                    }
                };
                let source = ResolutionError::Provider(provider);
                if let Some(repository) = repository {
                    Self::GitRepositoryUnavailable {
                        repository,
                        source: Box::new(source),
                    }
                } else {
                    Self::Resolution {
                        source: Box::new(source),
                    }
                }
            }
            source => Self::Resolution {
                source: Box::new(source),
            },
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum LockfileBuildError {
    #[error("repository operation failed: {source}")]
    #[diagnostic(code(rpx::lock::repository_failed))]
    Repository {
        #[source]
        source: RepositoryError,
    },

    #[error("could not access Git repository {repository}")]
    #[diagnostic(
        code(rpx::lock::git_repository_unavailable),
        help(
            "Check that the repository exists. For private repositories, configure Git credentials."
        )
    )]
    GitRepositoryUnavailable {
        repository: String,
        #[source]
        source: RepositoryError,
    },

    #[error("repository {repository} cannot be written to the lockfile")]
    #[diagnostic(code(rpx::lock::unsupported_repository))]
    UnsupportedRepository { repository: String },

    #[error("failed to read package requirements for {package}: {details}")]
    #[diagnostic(
        code(rpx::lock::resolve_failed),
        help("Check package names and version constraints in the package DESCRIPTION.")
    )]
    InvalidPackageRequirements { package: String, details: String },
}

impl From<RepositoryError> for LockfileBuildError {
    fn from(source: RepositoryError) -> Self {
        if let Some(repository) = inaccessible_git_repository(&source) {
            Self::GitRepositoryUnavailable {
                repository: repository.to_string(),
                source,
            }
        } else {
            Self::Repository { source }
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum ProjectWriteError {
    #[error("failed to serialize rpx.lock: {source}")]
    #[diagnostic(code(rpx::project::lockfile_serialize_failed))]
    SerializeLockfile {
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to prepare project file at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::file_prepare_failed))]
    Prepare {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to replace project file at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::file_replace_failed))]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "DESCRIPTION was updated, but failed to replace rpx.lock at {}: {source}",
        path.display()
    )]
    #[diagnostic(
        code(rpx::project::metadata_partially_written),
        help("Run `rpx lock` to bring rpx.lock back in sync with DESCRIPTION.")
    )]
    LockfileReplaceAfterDescription {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

struct PreparedProjectFile {
    path: PathBuf,
    destination: PathBuf,
}

impl PreparedProjectFile {
    fn new(destination: PathBuf, contents: &[u8]) -> Result<Self, ProjectWriteError> {
        let parent = destination
            .parent()
            .expect("project file destination should have a parent");
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .expect("project file name should be valid UTF-8");

        for _ in 0..100 {
            let unique = NEXT_STAGED_FILE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".{name}.rpx-stage-{}-{unique}", std::process::id()));
            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(ProjectWriteError::Prepare {
                        path: destination,
                        source,
                    });
                }
            };
            if let Ok(metadata) = fs::metadata(&destination)
                && let Err(source) = file.set_permissions(metadata.permissions())
            {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(ProjectWriteError::Prepare {
                    path: destination,
                    source,
                });
            }
            if let Err(source) = file.write_all(contents).and_then(|()| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(ProjectWriteError::Prepare {
                    path: destination,
                    source,
                });
            }

            return Ok(Self { path, destination });
        }

        Err(ProjectWriteError::Prepare {
            path: destination,
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "failed to allocate a unique staging file",
            ),
        })
    }

    fn replace(mut self) -> Result<(), std::io::Error> {
        #[cfg(windows)]
        if self.destination.exists() {
            fs::remove_file(&self.destination)?;
        }
        fs::rename(&self.path, &self.destination)?;
        self.path = PathBuf::new();
        Ok(())
    }
}

impl Drop for PreparedProjectFile {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn write_project_files(
    root: &Path,
    description: Option<&RDescription>,
    lockfile: &Lockfile,
) -> Result<(), ProjectWriteError> {
    let mut lockfile_contents = serde_json::to_vec_pretty(lockfile)
        .map_err(|source| ProjectWriteError::SerializeLockfile { source })?;
    lockfile_contents.push(b'\n');

    let description_path = root.join(DESCRIPTION_NAME);
    let description = description
        .map(|description| {
            let contents = description.to_string();
            PreparedProjectFile::new(description_path.clone(), contents.as_bytes())
        })
        .transpose()?;
    let lockfile_path = root.join(LOCKFILE_NAME);
    let lockfile = PreparedProjectFile::new(lockfile_path.clone(), &lockfile_contents)?;

    if let Some(description) = description {
        description
            .replace()
            .map_err(|source| ProjectWriteError::Replace {
                path: description_path,
                source,
            })?;
        lockfile.replace().map_err(
            |source| ProjectWriteError::LockfileReplaceAfterDescription {
                path: lockfile_path,
                source,
            },
        )
    } else {
        lockfile
            .replace()
            .map_err(|source| ProjectWriteError::Replace {
                path: lockfile_path,
                source,
            })
    }
}

pub(crate) async fn resolve_project(
    project: &Project,
    policy: ResolutionPolicy,
) -> Result<ProjectResolution, ResolveProjectError> {
    let previous = read_previous_lockfile(&project.root)?;
    let r_version = r_version_async().await?;
    let requirements = project_dependencies(&project.root, &project.description)?;

    let repositories = match (policy, previous.as_ref()) {
        (ResolutionPolicy::ReuseIfValid, Some(lockfile)) => {
            match validate_locked_resolution(
                &project.root,
                &project.description,
                &r_version,
                lockfile,
            ) {
                Ok(()) => {
                    return Ok(ProjectResolution {
                        lockfile: lockfile.clone(),
                        lockfile_changed: false,
                        packages: required_packages_from_lockfile(lockfile)?,
                        r_version,
                    });
                }
                Err(LockedResolutionError::Validation { failures })
                    if failures.iter().all(|failure| {
                        matches!(
                            failure,
                            LockedResolutionFailure::PackageRequirementsChanged
                                | LockedResolutionFailure::RVersionChanged { .. }
                        )
                    }) =>
                {
                    lockfile
                        .repos
                        .iter()
                        .map(<dyn PackageRepository>::from_lockfile)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|source| ResolveProjectError::LockedRepository { source })?
                }
                Err(LockedResolutionError::Validation { .. }) => {
                    repositories_from_description(&project.root, &project.description).await?
                }
                Err(LockedResolutionError::Parse(source)) => return Err(source.into()),
            }
        }
        (ResolutionPolicy::AlwaysResolve, _) | (ResolutionPolicy::ReuseIfValid, None) => {
            repositories_from_description(&project.root, &project.description).await?
        }
    };
    let preferred_versions = previous
        .as_ref()
        .map(|lockfile| {
            lockfile
                .packages
                .iter()
                .map(|(name, package)| (name.clone(), package.version.clone()))
                .collect()
        })
        .unwrap_or_default();
    let root = Arc::new(
        LocalRepository::new(project.root.clone()).with_description(project.description.clone()),
    );
    let selected = resolve_from_registry(
        repositories.clone(),
        Arc::clone(&root),
        requirements.clone(),
        preferred_versions,
    )
    .await?;
    let mut packages = hydrate_resolved_packages(selected)
        .await
        .map_err(|source| ResolveProjectError::PackageMetadata { source })?;
    packages.retain(|_, (version, _)| !version.repository().equals(root.as_ref()));
    let lockfile =
        lockfile_from_resolution(requirements, &packages, &repositories, &r_version).await?;
    let lockfile_changed = previous.as_ref() != Some(&lockfile);

    Ok(ProjectResolution {
        lockfile,
        lockfile_changed,
        packages,
        r_version,
    })
}

pub(crate) async fn load_project_resolution(
    project: &Project,
) -> Result<ProjectResolution, LoadProjectResolutionError> {
    let lockfile = read_lockfile(&project.root)?;
    let r_version = r_version_async().await?;
    validate_locked_resolution(&project.root, &project.description, &r_version, &lockfile)?;
    let packages = required_packages_from_lockfile(&lockfile)?;

    Ok(ProjectResolution {
        lockfile,
        lockfile_changed: false,
        packages,
        r_version,
    })
}

fn read_previous_lockfile(project_root: &PathBuf) -> Result<Option<Lockfile>, LockfileReadError> {
    match read_lockfile(project_root) {
        Ok(lockfile) => Ok(Some(lockfile)),
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(LockfileReadError::OutdatedLockfile { .. }) => Ok(None),
        Err(source) => Err(source),
    }
}

pub(crate) fn pin_unconstrained_dependencies(
    project: &mut Project,
    resolution: &mut ProjectResolution,
    relations: &BTreeSet<Relation>,
    dependency_field: DependencyField,
) -> Result<(), DescriptionParseError> {
    let pinned_relations = relations
        .iter()
        .filter(|relation| matches!(relation.requirement(), VersionRequirement::Any))
        .filter_map(|relation| {
            resolution
                .packages
                .get(relation.package())
                .map(|(selected, _)| (relation.package(), selected.version()))
        })
        .fold(relations.clone(), |mut relations, (package, version)| {
            pin_dependency_to_resolved_major(&mut relations, package, version);
            relations
        });

    if pinned_relations != *relations {
        add_dependencies(
            &project.root,
            &mut project.description,
            &pinned_relations,
            dependency_field,
        )?;
        resolution.lockfile.requirements =
            project_dependencies(&project.root, &project.description)?;
        resolution.lockfile_changed = true;
    }

    Ok(())
}

fn pin_dependency_to_resolved_major(
    relations: &mut BTreeSet<Relation>,
    package: &str,
    version: &r_description::Version,
) {
    let next_major = format!("{}.0.0", version.major() + 1)
        .parse::<r_description::Version>()
        .expect("next major version should be valid");

    relations.retain(|relation| {
        relation.package() != package || !matches!(relation.requirement(), VersionRequirement::Any)
    });
    relations.insert(
        Relation::new(
            package,
            VersionRequirement::GreaterThanEqual(version.clone()),
        )
        .expect("previously parsed package name should remain valid"),
    );
    relations.insert(
        Relation::new(package, VersionRequirement::LessThan(next_major))
            .expect("previously parsed package name should remain valid"),
    );
}

fn inaccessible_git_repository(error: &RepositoryError) -> Option<&str> {
    let RepositoryError::Git { source, .. } = error else {
        return None;
    };
    let git::GitError::Access { remote, .. } = source.as_ref() else {
        return None;
    };
    Some(remote)
}

pub(crate) async fn hydrate_resolved_packages(
    selected: BTreeMap<String, PackageVersion>,
) -> Result<RequiredPackages, RepositoryError> {
    // TODO: make sure the web requests are under a central semaphore in the repos not here
    futures_util::future::join_all(selected.into_iter().map(|(name, version)| async move {
        let description = version
            .repository()
            .description(&name, version.version())
            .await?;

        Ok::<_, RepositoryError>((name, (version, description)))
    }))
    .await
    .into_iter()
    .collect()
}

pub(crate) async fn lockfile_from_resolution(
    requirements: BTreeSet<r_description::Relation>,
    resolved_packages: &RequiredPackages,
    repositories: &[Arc<dyn PackageRepository>],
    r_version: &semver::Version,
) -> Result<Lockfile, LockfileBuildError> {
    let repos = futures_util::future::join_all(
        repositories
            .iter()
            .map(|repository| repository.to_lockfile()),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, RepositoryError>>()
    .map_err(LockfileBuildError::from)?;

    let packages = resolved_packages
        .iter()
        .map(|(name, (version, description))| {
            let repository = repositories
                .iter()
                .zip(&repos)
                .find(|(runtime, _)| version.repository().equals(runtime.as_ref()))
                .map(|(_, locked)| locked.url().clone())
                .ok_or_else(|| LockfileBuildError::UnsupportedRepository {
                    repository: version.repository().to_string(),
                })?;

            let depends = description.depends().map_err(|source| {
                LockfileBuildError::InvalidPackageRequirements {
                    package: name.clone(),
                    details: source.to_string(),
                }
            })?;
            let imports = description.imports().map_err(|source| {
                LockfileBuildError::InvalidPackageRequirements {
                    package: name.clone(),
                    details: source.to_string(),
                }
            })?;
            let linking_to = description.linking_to().map_err(|source| {
                LockfileBuildError::InvalidPackageRequirements {
                    package: name.clone(),
                    details: source.to_string(),
                }
            })?;
            let dependencies = depends.chain(imports).chain(linking_to).collect();

            Ok((
                name.clone(),
                lockfile::Package {
                    version: version.version().clone(),
                    repository,
                    dependencies,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, LockfileBuildError>>()?;

    Ok(Lockfile {
        version: lockfile::LOCKFILE_VERSION,
        revision: lockfile::LOCKFILE_REVISION,
        r: r_version.clone(),
        repos,
        requirements,
        packages,
    })
}

fn locked_package_description(
    name: &str,
    package: &lockfile::Package,
) -> Result<RDescription, LockedPackagesError> {
    let mut description = RDescription::parse("");
    description.set_package(name).map_err(|source| {
        LockedPackagesError::InvalidLockedDescription {
            package: name.to_string(),
            details: source.to_string(),
        }
    })?;
    description.set_version(&package.version);
    description.set_depends(package.dependencies.iter().cloned());
    Ok(description)
}

pub fn project_library_path(path: &Path) -> PathBuf {
    project_library_root_path(path).join("library")
}

pub(crate) fn ensure_project_library(path: &Path) -> Result<PathBuf, ProjectLibraryError> {
    let library_path = project_library_path(path);
    fs::create_dir_all(&library_path).map_err(|source| ProjectLibraryError::Create {
        path: library_path.clone(),
        source,
    })?;
    Ok(library_path)
}

pub fn project_library_root_path(path: &Path) -> PathBuf {
    let project_key = hash_path(path);
    project_dirs()
        .data_dir()
        .join("libraries")
        .join(project_key)
}

pub fn cache_dir_path() -> PathBuf {
    project_dirs().cache_dir().to_path_buf()
}

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("de", "scalerail", "rpx").expect("failed to resolve rpx data directory")
}

fn hash_path(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lockfile::{
            ArchiveSupport as LockedArchiveSupport, GitReference, LOCKFILE_REVISION,
            LOCKFILE_VERSION, Package, Repository,
        },
        repository::{ArchiveSupport, CranRepository, RrepoRepository, built_in_repository},
    };
    use r_description::{Relation, Version};
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicU64, Ordering},
    };

    const GIT_COMMIT: &str = "2222222222222222222222222222222222222222";
    static UNIQUE: AtomicU64 = AtomicU64::new(0);

    fn unique(name: &str) -> String {
        format!(
            "rpx-project-test-{name}-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn synthetic_path(name: &str) -> PathBuf {
        env::temp_dir().join(unique(name))
    }

    struct TestProject(PathBuf);

    impl TestProject {
        fn new(name: &str) -> Self {
            let path = synthetic_path(name);
            fs::create_dir_all(&path).expect("test project should be created");
            fs::write(
                path.join(DESCRIPTION_NAME),
                "Package: project\nVersion: 1.0.0\n",
            )
            .expect("DESCRIPTION should be written");
            Self(path)
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            remove_dir_if_present(&self.0);
        }
    }

    fn url(value: &str) -> url::Url {
        value.parse().expect("URL should parse")
    }

    fn relation(value: &str) -> Relation {
        value.parse().expect("relation should parse")
    }

    #[test]
    fn pins_unconstrained_dependency_to_resolved_major_range() {
        let mut relations = BTreeSet::from([relation("digest"), relation("cli (>= 3.0.0)")]);

        pin_dependency_to_resolved_major(&mut relations, "digest", &version("0.6.39"));

        assert_eq!(
            relations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["cli (>= 3.0.0)", "digest (< 1.0.0)", "digest (>= 0.6.39)"]
        );
    }

    #[test]
    fn pins_only_unconstrained_dependencies_present_in_resolution() {
        let test_project = TestProject::new("pin-selected-dependencies");
        let mut project = Project {
            root: test_project.0.clone(),
            description: read_description(&test_project.0)
                .expect("project DESCRIPTION should parse"),
        };
        let mut resolution = resolution(lockfile());
        resolution.packages.insert(
            "digest".to_string(),
            (
                PackageVersion::new(version("0.6.39"), built_in_repository()),
                Arc::new(project.description.clone()),
            ),
        );
        let relations = BTreeSet::from([relation("digest"), relation("stats")]);

        pin_unconstrained_dependencies(
            &mut project,
            &mut resolution,
            &relations,
            DependencyField::Suggests,
        )
        .expect("dependencies should be pinned");

        let expected = BTreeSet::from([
            relation("digest (>= 0.6.39)"),
            relation("digest (< 1.0.0)"),
            relation("stats"),
        ]);
        assert_eq!(
            project_dependencies(&project.root, &project.description)
                .expect("project dependencies should parse"),
            expected
        );
        assert_eq!(resolution.lockfile.requirements, expected);
        assert!(resolution.lockfile_changed);
    }

    fn version(value: &str) -> Version {
        value.parse().expect("package version should parse")
    }

    fn lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            r: semver::Version::new(4, 5, 0),
            repos: Vec::new(),
            requirements: BTreeSet::new(),
            packages: BTreeMap::new(),
        }
    }

    fn resolution(lockfile: Lockfile) -> ProjectResolution {
        ProjectResolution {
            r_version: lockfile.r.clone(),
            lockfile,
            lockfile_changed: false,
            packages: RequiredPackages::new(),
        }
    }

    fn rrepo(value: &str) -> Repository {
        Repository::Rrepo { url: url(value) }
    }

    fn cran(value: &str, archive_support: LockedArchiveSupport) -> Repository {
        Repository::CranLike {
            url: url(value),
            archive_support,
        }
    }

    fn git(value: &str, reference: &str, subdirectory: Option<&str>) -> Repository {
        Repository::Git {
            url: url(value),
            reference: GitReference::Named {
                value: reference.to_string(),
            },
            commit: GIT_COMMIT.parse().expect("OID should parse"),
            subdirectory: subdirectory.map(Into::into),
        }
    }

    fn package(version: &str, repository: &str, dependencies: &[&str]) -> Package {
        Package {
            version: self::version(version),
            repository: url(repository),
            dependencies: dependencies
                .iter()
                .map(|dependency| relation(dependency))
                .collect(),
        }
    }

    fn validation_fixture() -> (PathBuf, RDescription, semver::Version, Lockfile) {
        let path = synthetic_path("validation");
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: https://configured.example/base\nRemotes: bitbucket::owner/repository/subdirectory@main\nAdditional_repositories: https://additional.example/cran\nImports: cli (>= 3.0.0)\nSuggests: testthat\n",
        );
        let configured = configured_repositories(&path, &description)
            .expect("repository configuration should be valid");
        assert_eq!(configured.len(), 3);
        let base = match &configured[0] {
            ConfiguredRepository::Base(url) => Repository::Rrepo { url: url.clone() },
            repository => panic!("expected base repository, got {repository:?}"),
        };
        let git = match &configured[1] {
            ConfiguredRepository::Git(remote) => {
                let repository = GitRepository::new(remote.clone())
                    .expect("configured Git repository should be valid");
                Repository::Git {
                    url: url::Url::try_from(repository.remote())
                        .expect("Git remote should convert to URL"),
                    reference: GitReference::Named {
                        value: repository
                            .reference()
                            .expect("reference should exist")
                            .into(),
                    },
                    commit: GIT_COMMIT.parse().expect("OID should parse"),
                    subdirectory: repository
                        .subdirectory()
                        .map(relative_path::RelativePathBuf::from_path)
                        .transpose()
                        .expect("subdirectory should be relative"),
                }
            }
            repository => panic!("expected Git repository, got {repository:?}"),
        };
        let additional = match &configured[2] {
            ConfiguredRepository::Additional(url) => Repository::CranLike {
                url: url.clone(),
                archive_support: LockedArchiveSupport::Available,
            },
            repository => panic!("expected additional repository, got {repository:?}"),
        };
        let r_version = semver::Version::new(4, 5, 1);
        let mut lockfile = lockfile();
        lockfile.r = r_version.clone();
        lockfile.repos = vec![base, git, additional];
        lockfile.requirements =
            project_dependencies(&path, &description).expect("requirements should be valid");
        (path, description, r_version, lockfile)
    }

    fn failures(
        path: &PathBuf,
        description: &RDescription,
        r_version: &semver::Version,
        lockfile: &Lockfile,
    ) -> Vec<LockedResolutionFailure> {
        let LockedResolutionError::Validation { failures } =
            validate_locked_resolution(path, description, r_version, lockfile)
                .expect_err("resolution should be rejected")
        else {
            panic!("expected validation failures");
        };
        failures
    }

    fn assert_repositories_changed(failures: &[LockedResolutionFailure]) {
        assert_eq!(failures.len(), 1);
        assert!(matches!(
            failures[0],
            LockedResolutionFailure::RepositoriesChanged
        ));
    }

    fn remove_dir_if_present(path: &Path) {
        if path.exists() {
            fs::remove_dir_all(path).expect("test directory should be removed");
        }
    }

    #[test]
    fn writes_only_requested_project_files() {
        let directory = TestProject::new("write-lockfile");
        let description_before =
            fs::read(directory.0.join(DESCRIPTION_NAME)).expect("DESCRIPTION should be readable");
        let expected = lockfile();

        write_project_files(&directory.0, None, &expected).expect("lockfile should be written");

        assert_eq!(
            fs::read(directory.0.join(DESCRIPTION_NAME))
                .expect("DESCRIPTION should remain readable"),
            description_before
        );
        assert_eq!(
            read_lockfile(&directory.0).expect("lockfile should be readable"),
            expected
        );
    }

    #[test]
    fn prepares_and_writes_project_files_without_consistency_validation() {
        let directory = TestProject::new("write-metadata");
        let description =
            RDescription::parse("Package: changed\nVersion: 2.0.0\nImports: dependency\n");
        let expected = lockfile();

        write_project_files(&directory.0, Some(&description), &expected)
            .expect("metadata should be written");

        let description = read_description(&directory.0).expect("DESCRIPTION should be readable");
        assert_eq!(
            description.package().expect("Package should be valid"),
            "changed"
        );
        assert_eq!(
            read_lockfile(&directory.0).expect("lockfile should be readable"),
            expected
        );
        assert!(
            fs::read_dir(&directory.0)
                .expect("project directory should be readable")
                .all(|entry| {
                    !entry
                        .expect("project entry should be readable")
                        .file_name()
                        .to_string_lossy()
                        .contains(".rpx-stage-")
                })
        );
    }

    #[test]
    fn project_file_preparation_error_names_destination() {
        let root = synthetic_path("missing-write-root");
        let expected = root.join(LOCKFILE_NAME);

        let error = write_project_files(&root, None, &lockfile())
            .expect_err("missing project root should prevent preparation");

        assert!(matches!(
            error,
            ProjectWriteError::Prepare { path, .. } if path == expected
        ));
    }

    #[test]
    fn reports_lockfile_replacement_after_description_was_updated() {
        let directory = TestProject::new("partial-project-write");
        let lockfile_path = directory.0.join(LOCKFILE_NAME);
        fs::create_dir(&lockfile_path).expect("lockfile destination should block replacement");
        let description = RDescription::parse("Package: changed\nVersion: 2.0.0\n");

        let error = write_project_files(&directory.0, Some(&description), &lockfile())
            .expect_err("lockfile replacement should fail");

        assert!(matches!(
            error,
            ProjectWriteError::LockfileReplaceAfterDescription { path, .. }
                if path == lockfile_path
        ));
        assert_eq!(
            read_description(&directory.0)
                .expect("updated DESCRIPTION should be readable")
                .package()
                .expect("updated package name should be valid"),
            "changed"
        );
    }

    #[test]
    fn treats_missing_and_outdated_lockfiles_as_absent_previous_resolutions() {
        let directory = TestProject::new("previous-lockfile");
        assert_eq!(
            read_previous_lockfile(&directory.0).expect("missing lockfile should be accepted"),
            None
        );

        fs::write(directory.0.join(LOCKFILE_NAME), r#"{"version": 0}"#)
            .expect("outdated lockfile should be written");
        assert_eq!(
            read_previous_lockfile(&directory.0).expect("outdated lockfile should be accepted"),
            None
        );
    }

    #[test]
    fn preserves_other_previous_lockfile_errors() {
        let directory = TestProject::new("invalid-previous-lockfile");
        fs::write(directory.0.join(LOCKFILE_NAME), "not JSON")
            .expect("invalid lockfile should be written");

        assert!(matches!(
            read_previous_lockfile(&directory.0),
            Err(LockfileReadError::Parse { .. })
        ));
    }

    #[tokio::test]
    async fn strict_resolution_requires_a_current_lockfile() {
        let directory = TestProject::new("strict-lockfile");
        let project = Project {
            root: directory.0.clone(),
            description: read_description(&directory.0).expect("DESCRIPTION should be readable"),
        };

        assert!(matches!(
            load_project_resolution(&project).await,
            Err(LoadProjectResolutionError::LockfileRead(
                LockfileReadError::Read { source, .. }
            ))
                if source.kind() == std::io::ErrorKind::NotFound
        ));

        fs::write(directory.0.join(LOCKFILE_NAME), r#"{"version": 0}"#)
            .expect("outdated lockfile should be written");
        assert!(matches!(
            load_project_resolution(&project).await,
            Err(LoadProjectResolutionError::LockfileRead(
                LockfileReadError::OutdatedLockfile { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn reconstructs_locked_packages_and_descriptions() {
        let rrepo_url = "https://rrepo.example/cran";
        let cran_url = "https://cran.example/cran";
        let git_url = "https://github.com/owner/repository.git";
        let mut lockfile = lockfile();
        lockfile.repos = vec![
            rrepo(rrepo_url),
            cran(cran_url, LockedArchiveSupport::Unavailable),
            git(git_url, "main", Some("pkg")),
        ];
        lockfile.packages = BTreeMap::from([
            (
                "rrepoPkg".into(),
                package("1.2.3", rrepo_url, &["cli (>= 3.0.0)"]),
            ),
            ("cranPkg".into(), package("4.5.6", cran_url, &["digest"])),
            (
                "gitPkg".into(),
                package("7.8.9", git_url, &["rlang", "vctrs (>= 0.6.0)"]),
            ),
        ]);

        let packages =
            required_packages_from_lockfile(&lockfile).expect("locked packages should reconstruct");
        assert_eq!(packages.len(), 3, "no local root should be injected");
        for (name, expected_version, dependencies) in [
            ("rrepoPkg", "1.2.3", &["cli (>= 3.0.0)"][..]),
            ("cranPkg", "4.5.6", &["digest"][..]),
            ("gitPkg", "7.8.9", &["rlang", "vctrs (>= 0.6.0)"][..]),
        ] {
            let (package, description) = &packages[name];
            assert_eq!(package.version(), &version(expected_version));
            assert_eq!(
                description.package().expect("Package should be valid"),
                name
            );
            assert_eq!(
                description.version().expect("Version should be valid"),
                version(expected_version)
            );
            assert_eq!(
                description
                    .depends()
                    .expect("Depends should be valid")
                    .collect::<BTreeSet<_>>(),
                dependencies
                    .iter()
                    .map(|dependency| relation(dependency))
                    .collect()
            );
        }

        let rrepo = packages["rrepoPkg"]
            .0
            .repository()
            .downcast_ref::<RrepoRepository>()
            .expect("Rrepo repository should reconstruct");
        assert_eq!(rrepo.url(), &url(rrepo_url));
        let cran = packages["cranPkg"]
            .0
            .repository()
            .downcast_ref::<CranRepository>()
            .expect("CRAN repository should reconstruct");
        assert_eq!(cran.url(), &url(cran_url));
        assert_eq!(cran.archive_support(), ArchiveSupport::Unavailable);
        let git = packages["gitPkg"]
            .0
            .repository()
            .downcast_ref::<GitRepository>()
            .expect("Git repository should reconstruct");
        assert_eq!(git.remote().to_string(), git_url);
        assert_eq!(git.reference(), Some("main"));
        assert_eq!(git.subdirectory(), Some(Path::new("pkg")));
        assert_eq!(
            git.commit()
                .await
                .expect("commit should be locked")
                .to_string(),
            GIT_COMMIT
        );
    }

    #[test]
    fn first_repository_with_matching_url_wins() {
        let repository = "https://github.com/owner/repository.git";
        let mut lockfile = lockfile();
        lockfile.repos = vec![
            git(repository, "first", None),
            git(repository, "second", None),
        ];
        lockfile
            .packages
            .insert("fixture".into(), package("1.0.0", repository, &[]));
        let packages =
            required_packages_from_lockfile(&lockfile).expect("package should reconstruct");
        let repository = packages["fixture"]
            .0
            .repository()
            .downcast_ref::<GitRepository>()
            .expect("repository should be Git");
        assert_eq!(repository.reference(), Some("first"));
    }

    #[test]
    fn missing_package_repository_reports_exact_package_and_url() {
        let missing = url("https://missing.example/cran");
        let mut lockfile = lockfile();
        lockfile
            .packages
            .insert("missingPkg".into(), package("1.0.0", missing.as_str(), &[]));
        assert!(matches!(required_packages_from_lockfile(&lockfile),
            Err(LockedPackagesError::RepositoryNotFound { package, repository })
                if package == "missingPkg" && repository == missing));
    }

    #[test]
    fn invalid_locked_git_repository_reports_exact_package() {
        let repository = "ftp://example.test/repository.git";
        let mut lockfile = lockfile();
        lockfile.repos = vec![git(repository, "main", None)];
        lockfile
            .packages
            .insert("brokenGit".into(), package("1.0.0", repository, &[]));
        assert!(matches!(required_packages_from_lockfile(&lockfile),
            Err(LockedPackagesError::Repository { package, .. }) if package == "brokenGit"));
    }

    #[test]
    fn invalid_package_map_key_reports_locked_description_error() {
        let repository = "https://rrepo.example/cran";
        let invalid = "invalid\nkey";
        let mut lockfile = lockfile();
        lockfile.repos = vec![rrepo(repository)];
        lockfile
            .packages
            .insert(invalid.into(), package("1.0.0", repository, &[]));
        let error = required_packages_from_lockfile(&lockfile)
            .expect_err("invalid package key should be rejected");
        assert!(matches!(error,
            LockedPackagesError::InvalidLockedDescription { package, .. }
                if package == invalid));
    }

    #[test]
    fn accepts_matching_locked_resolution() {
        let (path, description, r_version, lockfile) = validation_fixture();
        validate_locked_resolution(&path, &description, &r_version, &lockfile)
            .expect("matching resolution should validate");
    }

    #[test]
    fn repository_shape_mismatches_report_only_repositories_changed() {
        let (path, description, r_version, lockfile) = validation_fixture();
        let mut non_git_url = lockfile.clone();
        non_git_url.repos[0] = rrepo("https://changed.example/base");
        let mut git_as_non_git = lockfile.clone();
        git_as_non_git.repos[1] = rrepo(lockfile.repos[1].url().as_str());
        let mut extra = lockfile.clone();
        extra.repos.push(rrepo("https://extra.example/cran"));
        let mut missing = lockfile.clone();
        missing.repos.pop();
        for changed in [non_git_url, git_as_non_git, extra, missing] {
            assert_repositories_changed(&failures(&path, &description, &r_version, &changed));
        }
    }

    #[test]
    fn git_reference_drift_reports_only_repositories_changed() {
        let (path, description, r_version, mut lockfile) = validation_fixture();
        let Repository::Git { reference, .. } = &mut lockfile.repos[1] else {
            panic!("fixture repository should be Git");
        };
        *reference = GitReference::Named {
            value: "develop".into(),
        };
        assert_repositories_changed(&failures(&path, &description, &r_version, &lockfile));
    }

    #[test]
    fn invalid_locked_git_reports_only_invalid_repository_at_index() {
        let (path, description, r_version, mut lockfile) = validation_fixture();
        let Repository::Git { url, .. } = &mut lockfile.repos[1] else {
            panic!("fixture repository should be Git");
        };
        *url = self::url("ftp://example.test/repository.git");
        let failures = failures(&path, &description, &r_version, &lockfile);
        assert_eq!(failures.len(), 1);
        assert!(matches!(&failures[0],
            LockedResolutionFailure::InvalidRepository { index: 1, repository, .. }
                if repository.as_str() == "ftp://example.test/repository.git"));
    }

    #[test]
    fn locked_resolution_failures_are_aggregated_in_order() {
        let (path, description, r_version, mut lockfile) = validation_fixture();
        lockfile.repos[0] = rrepo("https://changed.example/base");
        let Repository::Git { url, .. } = &mut lockfile.repos[1] else {
            panic!("fixture repository should be Git");
        };
        *url = self::url("ftp://example.test/repository.git");
        lockfile.requirements = BTreeSet::from([relation("different")]);
        lockfile.r = semver::Version::new(4, 4, 0);
        let failures = failures(&path, &description, &r_version, &lockfile);
        assert_eq!(failures.len(), 4);
        assert!(matches!(
            failures[0],
            LockedResolutionFailure::InvalidRepository { index: 1, .. }
        ));
        assert!(matches!(
            failures[1],
            LockedResolutionFailure::RepositoriesChanged
        ));
        assert!(matches!(
            failures[2],
            LockedResolutionFailure::PackageRequirementsChanged
        ));
        assert!(matches!(&failures[3],
            LockedResolutionFailure::RVersionChanged { locked, current }
                if locked == &semver::Version::new(4, 4, 0) && current == &r_version));
    }

    #[test]
    fn project_library_root_is_stable_and_uses_expected_layout() {
        let first_path = synthetic_path("library-root-first");
        let second_path = synthetic_path("library-root-second");
        let first = project_library_root_path(&first_path);
        assert_eq!(first, project_library_root_path(&first_path));
        assert_ne!(first, project_library_root_path(&second_path));
        let libraries = project_dirs().data_dir().join("libraries");
        assert_eq!(first.parent(), Some(libraries.as_path()));
        let key = first
            .file_name()
            .and_then(|key| key.to_str())
            .expect("key should be UTF-8");
        assert_eq!(key.len(), 16);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn project_library_path_does_not_create_root_or_library() {
        let project = synthetic_path("library-path");
        let root = project_library_root_path(&project);
        remove_dir_if_present(&root);
        let library = project_library_path(&project);
        assert_eq!(library, root.join("library"));
        assert!(!root.exists());
        assert!(!library.exists());
    }

    #[test]
    fn ensure_project_library_creates_root_and_library() {
        let project = synthetic_path("ensure-library");
        let root = project_library_root_path(&project);
        remove_dir_if_present(&root);
        let library = ensure_project_library(&project).expect("project library should be created");
        assert!(root.is_dir());
        assert!(library.is_dir());
        remove_dir_if_present(&root);
    }

    #[test]
    fn ensure_project_library_reports_creation_failure() {
        let project = synthetic_path("ensure-library-error");
        let root = project_library_root_path(&project);
        remove_dir_if_present(&root);
        fs::create_dir_all(root.parent().expect("library root should have a parent"))
            .expect("library parent should be created");
        fs::write(&root, "not a directory").expect("blocking file should be written");

        let error = ensure_project_library(&project).expect_err("library creation should fail");

        assert!(matches!(
            error,
            ProjectLibraryError::Create { path, .. } if path == root.join("library")
        ));
        fs::remove_file(root).expect("blocking file should be removed");
    }

    #[test]
    fn cache_dir_matches_project_directories() {
        assert_eq!(cache_dir_path(), project_dirs().cache_dir());
    }
}

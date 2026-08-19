use directories::ProjectDirs;
use miette::Diagnostic;
use r_description::RDescription;
use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::{
    description::{
        ConfiguredRepositoriesError, ConfiguredRepository, DESCRIPTION_NAME, DescriptionParseError,
        configured_repositories, project_dependencies,
    },
    lockfile::{self, LOCKFILE_NAME, Lockfile},
    repository::{GitRepository, PackageRepository, RepositoryError},
    resolver::PackageVersion,
};

pub type RequiredPackages = BTreeMap<String, (PackageVersion, Arc<RDescription>)>;

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

#[derive(Debug, Error, Diagnostic)]
pub enum LockedPackagesError {
    #[error("DESCRIPTION at {} is missing required field {field}", path.display())]
    #[diagnostic(code(rpx::project::description_missing_field))]
    MissingField { path: PathBuf, field: &'static str },

    #[error("invalid Version in DESCRIPTION at {}: {details}", path.display())]
    #[diagnostic(code(rpx::project::description_invalid_version))]
    InvalidVersion { path: PathBuf, details: String },

    #[error("invalid locked version {version} for {package}: {details}")]
    #[diagnostic(code(rpx::project::locked_package_invalid_version))]
    InvalidLockedVersion {
        package: String,
        version: String,
        details: String,
    },

    #[error("locked package map key {key} does not match package field {package}")]
    #[diagnostic(code(rpx::project::locked_package_name_mismatch))]
    LockedPackageNameMismatch { key: String, package: String },

    #[error("locked package {package} is missing source_url")]
    #[diagnostic(code(rpx::project::locked_package_missing_source_url))]
    MissingSourceUrl { package: String },

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

    #[error("invalid locked repository URL {url}: {details}")]
    #[diagnostic(code(rpx::project::locked_repository_invalid_url))]
    InvalidRepository { url: String, details: String },

    #[error("failed to reconstruct DESCRIPTION for locked package {package}: {details}")]
    #[diagnostic(code(rpx::project::locked_package_description_invalid))]
    InvalidLockedDescription { package: String, details: String },
}

pub fn required_packages_from_lockfile(
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

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repositories(#[from] ConfiguredRepositoriesError),

    #[error("rpx.lock does not match the current project configuration")]
    #[diagnostic(
        code(rpx::project::locked_resolution_invalid),
        help("Run `rpx lock` to update rpx.lock.")
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

pub fn project_library_path(path: &PathBuf) -> PathBuf {
    let library_path = project_library_root_path(path).join("library");

    fs::create_dir_all(&library_path).expect("failed to create project library");
    library_path
}

pub fn project_library_root_path(path: &PathBuf) -> PathBuf {
    let project_key = hash_path(path);
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
    use crate::{
        lockfile::{
            ArchiveSupport as LockedArchiveSupport, GitReference, LOCKFILE_REVISION,
            LOCKFILE_VERSION, Package, Repository, SystemRequirements,
        },
        repository::{ArchiveSupport, CranRepository, RrepoRepository},
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

    fn url(value: &str) -> url::Url {
        value.parse().expect("URL should parse")
    }

    fn relation(value: &str) -> Relation {
        value.parse().expect("relation should parse")
    }

    fn version(value: &str) -> Version {
        value.parse().expect("package version should parse")
    }

    fn lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            r: semver::Version::new(4, 5, 0),
            sysreqs: SystemRequirements {
                db_commit: None,
                rules: BTreeMap::new(),
            },
            repos: Vec::new(),
            requirements: BTreeSet::new(),
            packages: BTreeMap::new(),
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
    fn project_library_path_creates_root_and_library() {
        let project = synthetic_path("library-path");
        let root = project_library_root_path(&project);
        remove_dir_if_present(&root);
        let library = project_library_path(&project);
        assert_eq!(library, root.join("library"));
        assert!(root.is_dir());
        assert!(library.is_dir());
        remove_dir_if_present(&root);
    }

    #[test]
    fn cache_dir_matches_project_directories() {
        assert_eq!(cache_dir_path(), project_dirs().cache_dir());
    }

    #[test]
    fn artifact_cache_path_has_exact_layout_and_creates_only_parent() {
        let package = unique("artifact");
        let root = project_dirs().cache_dir().join("artifacts").join(&package);
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
        let root = project_dirs()
            .cache_dir()
            .join("build-temp")
            .join(format!("{package}-unique"));
        remove_dir_if_present(&root);
        let path = build_temp_library_path(&package, "unique");
        assert_eq!(path, root.join("library"));
        assert!(path.is_dir());
        remove_dir_if_present(&root);
    }
}

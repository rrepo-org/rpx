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
            ArchiveSupport as LockedArchiveSupport, GitReference, Package, Repository,
            SystemRequirements,
        },
        repository::RrepoRepository,
    };
    use r_description::Version;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SYSREQ_COMMIT: &str = "1111111111111111111111111111111111111111";
    const GIT_COMMIT: &str = "2222222222222222222222222222222222222222";

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

    fn oid(value: &str) -> git2::Oid {
        value.parse().expect("OID should parse")
    }

    fn url(value: &str) -> url::Url {
        value.parse().expect("URL should parse")
    }

    fn relation(value: &str) -> Relation {
        value.parse().expect("relation should parse")
    }

    fn package_version(value: &str) -> Version {
        value.parse().expect("package version should parse")
    }

    fn minimal_lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            revision: 0,
            r: semver::Version::new(4, 5, 0),
            sysreqs: SystemRequirements {
                db_commit: Some(oid(SYSREQ_COMMIT)),
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

    fn git_repository(value: &str, reference: &str, commit: &str) -> Repository {
        Repository::Git {
            url: url(value),
            reference: GitReference::Named {
                value: reference.to_string(),
            },
            commit: oid(commit),
            subdirectory: None,
        }
    }

    fn package(version: &str, repository: &str, dependencies: &[&str]) -> Package {
        Package {
            version: package_version(version),
            repository: url(repository),
            dependencies: dependencies
                .iter()
                .map(|dependency| relation(dependency))
                .collect(),
        }
    }

    fn write_lockfile(path: &Path, lockfile: &Lockfile) {
        let contents = serde_json::to_string(lockfile).expect("lockfile should serialize");
        fs::write(path.join(LOCKFILE_NAME), contents).expect("lockfile should be written");
    }

    #[test]
    fn parses_supported_git_remotes_from_description() {
        let contents = "Package: project\nVersion: 1.0.0\nRemotes: github::owner/github-package@main,\n gitlab@code.example::group/gitlab-package,\n bitbucket::owner/bitbucket-package/subdir@v1,\n generic=git::ssh://git@example.com/team/generic-package.git@develop\n";
        let description = RDescription::parse(contents);
        let remotes = git_remotes(&description, Path::new(DESCRIPTION_NAME))
            .expect("Git remotes should parse");

        assert_eq!(remotes.len(), 4);
        assert!(matches!(remotes[0].source, RemoteSource::GitHub(_)));
        assert!(matches!(remotes[1].source, RemoteSource::GitLab(_)));
        assert!(matches!(remotes[2].source, RemoteSource::Bitbucket(_)));
        assert!(matches!(remotes[3].source, RemoteSource::Git(_)));
        assert_eq!(remotes[1].host.as_deref(), Some("code.example"));
        assert_eq!(remotes[3].package.as_deref(), Some("generic"));
    }

    #[test]
    fn derives_repository_urls_from_additional_repositories_and_remotes() {
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://extra.test/cran\nRemotes: github::owner/repository@main\n",
        );

        assert_eq!(
            description_repository_urls(&description),
            Some(vec![
                url("https://extra.test/cran"),
                url("https://github.com/owner/repository.git"),
            ])
        );
    }

    #[test]
    fn rejects_malformed_remotes_when_description_is_loaded() {
        let path = project_directory("malformed-remotes");
        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner\n",
        )
        .expect("DESCRIPTION should be written");
        let project = Project::new(path.clone());
        let description = project.description().expect("DESCRIPTION should load");

        assert!(matches!(
            git_remotes(description, &path.join(DESCRIPTION_NAME)),
            Err(DescriptionReadError::InvalidField {
                field: "Remotes",
                ..
            })
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn rejects_description_with_syntax_issues_without_caching_it() {
        let path = project_directory("recovered-description");
        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: project\nVersion: 1.0.0\nthis line is malformed\nImports: cli\n",
        )
        .expect("DESCRIPTION should be written");
        let project = Project::new(path.clone());

        assert!(matches!(
            project.description(),
            Err(DescriptionReadError::Parse(_))
        ));

        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: project\nVersion: 1.0.0\nImports: cli\n",
        )
        .expect("DESCRIPTION should be rewritten");
        let description = project.description().expect("DESCRIPTION should load");
        assert_eq!(
            description.package().expect("Package should be valid"),
            "project"
        );
        assert_eq!(
            description
                .imports()
                .expect("Imports should be valid")
                .map(|relation| relation.package().to_string())
                .collect::<Vec<_>>(),
            vec!["cli"]
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn rejects_unsupported_remote_sources() {
        let contents = "Package: project\nVersion: 1.0.0\nRemotes: archive=url::https://example.com/pkg.tar.gz\n";
        let description = RDescription::parse(contents);

        assert!(matches!(
            git_remotes(&description, Path::new(DESCRIPTION_NAME)),
            Err(DescriptionReadError::UnsupportedRemote { kind, .. }) if kind == "url"
        ));
    }

    #[test]
    fn rejects_duplicate_remote_package_aliases() {
        let contents = "Package: project\nVersion: 1.0.0\nRemotes: dependency=owner/first, dependency=owner/second\n";
        let description = RDescription::parse(contents);

        assert!(matches!(
            git_remotes(&description, Path::new(DESCRIPTION_NAME)),
            Err(DescriptionReadError::DuplicateRemotePackage { package, .. })
                if package == "dependency"
        ));
    }

    #[test]
    fn caches_description_and_lockfile_after_first_read() {
        let path = project_directory("cached-files");
        fs::write(
            path.join(DESCRIPTION_NAME),
            "Package: initial\nVersion: 1.0.0\n",
        )
        .expect("DESCRIPTION should be written");
        let mut initial_lockfile = minimal_lockfile();
        initial_lockfile.revision = 1;
        write_lockfile(&path, &initial_lockfile);
        let project = Project::new(path.clone());

        assert_eq!(
            project
                .description()
                .expect("DESCRIPTION should load")
                .package()
                .expect("Package should be valid"),
            "initial"
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
        let mut changed_lockfile = minimal_lockfile();
        changed_lockfile.revision = 2;
        write_lockfile(&path, &changed_lockfile);

        assert_eq!(
            project
                .description()
                .expect("cached DESCRIPTION should be returned")
                .package()
                .expect("Package should be valid"),
            "initial"
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
    fn reports_missing_lockfile() {
        let path = project_directory("missing-lockfile");
        let project = Project::new(path.clone());

        assert!(matches!(
            project.lockfile(),
            Err(LockfileReadError::NotFound { .. })
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn writes_and_reads_project_files_at_root() {
        let path = project_directory("project-files");
        let project = Project::new(path.clone());

        let description = RDescription::parse("Package: project\nVersion: 1.0.0\n");
        let mut lockfile = minimal_lockfile();
        lockfile.revision = 1;

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
            project.lockfile().expect("lockfile should load").revision,
            1
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn reports_outdated_lockfile_before_parsing_its_schema() {
        let path = project_directory("outdated-lockfile");
        let lockfile_path = path.join(LOCKFILE_NAME);
        fs::write(
            &lockfile_path,
            format!(
                "{{\"version\":{},\"repositories\":[],\"roots\":[]}}",
                LOCKFILE_VERSION - 1
            ),
        )
        .expect("old lockfile should be written");
        let project = Project::new(path.clone());
        let expected_path = path_relative_to_current_dir(&lockfile_path);

        let error = project
            .lockfile()
            .expect_err("old lockfile should require an update");
        assert!(matches!(
            &error,
            LockfileReadError::OutdatedLockfile { path } if path == &expected_path
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_outdated")
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn reports_lockfile_created_by_newer_rpx() {
        let path = project_directory("newer-lockfile");
        let lockfile_path = path.join(LOCKFILE_NAME);
        fs::write(
            &lockfile_path,
            format!("{{\"version\":{}}}", LOCKFILE_VERSION + 1),
        )
        .expect("newer lockfile should be written");
        let project = Project::new(path.clone());
        let expected_path = path_relative_to_current_dir(&lockfile_path);

        let error = project
            .lockfile()
            .expect_err("newer lockfile should require a newer rpx");
        assert!(matches!(
            &error,
            LockfileReadError::NewerLockfile { path } if path == &expected_path
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_from_newer_rpx")
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn reports_parse_error_for_malformed_current_lockfile() {
        let path = project_directory("malformed-current-lockfile");
        fs::write(
            path.join(LOCKFILE_NAME),
            format!("{{\"version\":{LOCKFILE_VERSION}}}"),
        )
        .expect("malformed lockfile should be written");
        let project = Project::new(path.clone());

        assert!(matches!(
            project.lockfile(),
            Err(LockfileReadError::Parse { .. })
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn accepts_a_matching_locked_resolution() {
        let path = project_directory("matching-resolution");
        let description = RDescription::parse("Package: project\nVersion: 1.0.0\nImports: cli\n");
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![rrepo("https://repo.test/cran")];
        lockfile.requirements = BTreeSet::from([relation("cli")]);
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());
        let repositories: Vec<Arc<dyn PackageRepository>> = vec![Arc::new(RrepoRepository::new(
            url("https://repo.test/cran"),
        ))];

        project
            .validate_locked_resolution(&description, &repositories, &semver::Version::new(4, 5, 0))
            .expect("matching resolution should validate");

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn detects_git_remote_reference_drift() {
        let path = project_directory("git-remote-drift");
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/repository@main\n",
        );
        let changed_description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/repository@develop\n",
        );
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![git_repository(
            "https://github.com/owner/repository.git",
            "main",
            GIT_COMMIT,
        )];
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());
        let repository = git_remotes(&description, Path::new(DESCRIPTION_NAME))
            .expect("Git remotes should parse")
            .into_iter()
            .next()
            .and_then(|remote| GitRepository::new(remote).ok())
            .map(|repository| Arc::new(repository) as Arc<dyn PackageRepository>)
            .expect("Git repository should construct");

        project
            .validate_locked_resolution(&description, &[repository], &semver::Version::new(4, 5, 0))
            .expect("matching remote should validate");

        let changed_repository = git_remotes(&changed_description, Path::new(DESCRIPTION_NAME))
            .expect("Git remotes should parse")
            .into_iter()
            .next()
            .and_then(|remote| GitRepository::new(remote).ok())
            .map(|repository| Arc::new(repository) as Arc<dyn PackageRepository>)
            .expect("Git repository should construct");
        let error = project
            .validate_locked_resolution(
                &changed_description,
                &[changed_repository],
                &semver::Version::new(4, 5, 0),
            )
            .expect_err("changed remote should invalidate the lockfile");
        let LockedResolutionError::Validation { failures } = error else {
            panic!("changed remote should produce validation failures");
        };

        assert_eq!(failures.len(), 1);
        assert!(matches!(
            failures[0],
            LockedResolutionFailure::RepositoriesChanged
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn collects_all_locked_resolution_failures() {
        let path = project_directory("invalid-resolution");
        let description =
            RDescription::parse("Package: project\nVersion: 1.0.0\nImports: digest\n");
        let mut lockfile = minimal_lockfile();
        lockfile.r = semver::Version::new(4, 5, 0);
        lockfile.requirements = BTreeSet::from([relation("cli")]);
        lockfile.repos = vec![
            Repository::Git {
                url: url("ftp://example.test/repository.git"),
                reference: GitReference::DefaultBranch,
                commit: oid(GIT_COMMIT),
                subdirectory: None,
            },
            rrepo("https://repo.test/cran"),
        ];
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());

        let error = project
            .validate_locked_resolution(&description, &[], &semver::Version::new(4, 4, 0))
            .expect_err("all resolution mismatches should be reported");
        let LockedResolutionError::Validation { failures } = error else {
            panic!("validation failures should be aggregated");
        };

        assert_eq!(failures.len(), 4);
        assert!(matches!(
            &failures[0],
            LockedResolutionFailure::InvalidRepository { index: 0, repository, .. }
                if repository.as_str() == "ftp://example.test/repository.git"
        ));
        assert!(matches!(
            failures[1],
            LockedResolutionFailure::RepositoriesChanged
        ));
        assert!(matches!(
            failures[2],
            LockedResolutionFailure::PackageRequirementsChanged
        ));
        assert!(matches!(
            &failures[3],
            LockedResolutionFailure::RVersionChanged { locked, current }
                if locked == &semver::Version::new(4, 5, 0)
                    && current == &semver::Version::new(4, 4, 0)
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn infers_default_repository_policy_from_exact_identity_and_order() {
        let default_url = url("https://default.test/cran");
        let extra_url = url("https://extra.test/cran");
        let expected = vec![extra_url.clone()];
        let default = RrepoRepository::new(default_url.clone());
        let infer = |repos| {
            let mut lockfile = minimal_lockfile();
            lockfile.repos = repos;
            infer_locked_default_repository_enabled(&expected, &lockfile, &default)
        };

        assert_eq!(infer(vec![rrepo(extra_url.as_str())]), Some(false));
        assert_eq!(
            infer(vec![rrepo(default_url.as_str()), rrepo(extra_url.as_str())]),
            Some(true)
        );
        assert_eq!(
            infer(vec![
                rrepo("https://arbitrary.test/cran"),
                rrepo(extra_url.as_str())
            ]),
            None
        );
        assert_eq!(
            infer(vec![
                Repository::CranLike {
                    url: default_url.clone(),
                    archive_support: LockedArchiveSupport::Unavailable,
                },
                rrepo(extra_url.as_str())
            ]),
            None
        );
        assert_eq!(
            infer(vec![
                rrepo(default_url.as_str()),
                rrepo("https://unexpected.test/cran")
            ]),
            None
        );

        let expected = vec![default_url.clone()];
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![rrepo(default_url.as_str()), rrepo(default_url.as_str())];
        assert_eq!(
            infer_locked_default_repository_enabled(&expected, &lockfile, &default),
            Some(true)
        );
    }

    #[tokio::test]
    async fn infers_disabled_default_repository_with_git_remote_tail() {
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://extra.test/cran\nRemotes: github::owner/repository@main\n",
        );
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![
            rrepo("https://extra.test/cran"),
            git_repository(
                "https://github.com/owner/repository.git",
                "main",
                GIT_COMMIT,
            ),
        ];

        assert_eq!(
            locked_default_repository_enabled(&description, &lockfile).await,
            Some(false)
        );
    }

    #[test]
    fn hydrates_locked_packages_and_local_root() {
        let path = project_directory("locked-packages");
        let description = RDescription::parse("Package: project\nVersion: 2.0.0\n");
        let repository = "https://repo.test/example";
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![rrepo(repository)];
        lockfile.packages.insert(
            "digest".to_string(),
            package("0.6.39", repository, &["cli"]),
        );
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());

        let packages = project
            .required_packages_from_lockfile(&description)
            .expect("required packages should load");

        assert_eq!(packages.len(), 2);
        let (digest, description) = packages.get("digest").expect("digest should be locked");
        assert_eq!(digest.version().to_string(), "0.6.39");
        assert_eq!(
            description
                .depends()
                .expect("Depends should be valid")
                .collect::<Vec<_>>(),
            vec![relation("cli")]
        );
        assert_eq!(
            digest
                .repository()
                .as_ref()
                .downcast_ref::<RrepoRepository>()
                .expect("digest should use an rrepo repository")
                .url()
                .as_str(),
            repository
        );
        let (root, _) = packages.get("project").expect("project should be locked");
        assert_eq!(root.version().to_string(), "2.0.0");
        assert_eq!(
            root.repository()
                .as_ref()
                .downcast_ref::<LocalRepository>()
                .expect("project should use its local repository")
                .path(),
            path
        );

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn hydrates_package_from_first_repository_with_matching_url() {
        let path = project_directory("first-matching-repository");
        let description = RDescription::parse("Package: project\nVersion: 2.0.0\n");
        let repository = "https://github.com/owner/repository.git";
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![
            git_repository(repository, "first", GIT_COMMIT),
            git_repository(
                repository,
                "second",
                "3333333333333333333333333333333333333333",
            ),
        ];
        lockfile
            .packages
            .insert("fixture".to_string(), package("1.0.0", repository, &[]));
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());

        let packages = project
            .required_packages_from_lockfile(&description)
            .expect("required packages should load");
        let repository = packages["fixture"]
            .0
            .repository()
            .downcast_ref::<GitRepository>()
            .expect("locked package should use a Git repository");

        assert_eq!(repository.reference(), Some("first"));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn excludes_base_packages_from_locked_packages() {
        let path = project_directory("base-packages");
        let description = RDescription::parse("Package: project\nVersion: 1.0.0\n");
        let repository = "https://repo.test/cran";
        let mut lockfile = minimal_lockfile();
        lockfile.repos = vec![rrepo(repository)];
        lockfile
            .packages
            .insert("stats".to_string(), package("4.5.0", repository, &[]));
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());

        let packages = project
            .required_packages_from_lockfile(&description)
            .expect("required packages should load");

        assert_eq!(packages.len(), 1);
        assert!(packages.contains_key("project"));
        assert!(!packages.contains_key("stats"));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn rejects_locked_package_with_missing_repository() {
        let path = project_directory("missing-package-repository");
        let description = RDescription::parse("Package: project\nVersion: 1.0.0\n");
        let mut lockfile = minimal_lockfile();
        lockfile.packages.insert(
            "digest".to_string(),
            package("0.6.39", "https://missing.test/cran", &[]),
        );
        write_lockfile(&path, &lockfile);
        let project = Project::new(path.clone());

        assert!(matches!(
            project.required_packages_from_lockfile(&description),
            Err(LockedPackagesError::RepositoryNotFound { package, repository })
                if package == "digest" && repository.as_str() == "https://missing.test/cran"
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }

    #[test]
    fn rejects_invalid_root_package_descriptions() {
        let path = project_directory("invalid-root");
        let project = Project::new(path.clone());

        for (contents, expected) in [
            ("Version: 1.0.0\n", "Package"),
            ("Package: project\n", "Version"),
        ] {
            let description = RDescription::parse(contents);
            assert!(matches!(
                project.root_package_for(&description),
                Err(LockedPackagesError::MissingField { field, .. }) if field == expected
            ));
        }

        let description = RDescription::parse("Package: project\nVersion: invalid\n");
        assert!(matches!(
            project.root_package_for(&description),
            Err(LockedPackagesError::InvalidVersion { .. })
        ));

        fs::remove_dir_all(path).expect("project directory should be removed");
    }
}

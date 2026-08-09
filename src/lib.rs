use clap::Parser;
use futures_util::StreamExt;
use miette::Diagnostic;
use pubgrub::{DefaultStringReporter, PubGrubError, Reporter};
use r_description::{
    VersionConstraint,
    lossless::{RDescription, Relation, Relations, Version},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{Mutex, Semaphore, oneshot, watch},
};
use tracing::Instrument;
use tracing_indicatif::{
    filter::{IndicatifFilter, hide_indicatif_span_fields},
    span_ext::IndicatifSpanExt,
    style::ProgressStyle,
};
use tracing_subscriber::{
    EnvFilter,
    fmt::format::DefaultFields,
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

mod cache;
mod cli;
mod description;
#[allow(dead_code)]
mod git;
mod http;
mod lockfile;
mod output;
mod project;
mod r;
mod repository;
mod resolver;
mod sysreqs;
mod ui;

use cli::{Cli, Commands, RepoCommands};
use description::init_description;
use lockfile::{
    LOCKFILE_REVISION, LOCKFILE_VERSION, LockedR, LockedRepository, LockedRepositoryKind,
    LockedSystemRequirements, Lockfile,
};
use output::{blank_note_line, note, prompt, status, warning};
use project::{
    Project, RequiredPackages, artifact_cache_path, build_temp_library_path, cache_dir_path,
    locked_default_repository_enabled, project_library_path, project_library_root_path,
};
use r::{install_local_package, install_project_package, installed_packages};
use resolver::{ResolutionError, is_base_package, resolve_from_registry};
use sysreqs::{
    SystemDependencyPlan, cached_latest_snapshot, current_host_platform,
    empty_snapshot as empty_sysreq_snapshot, install as install_system_dependencies,
    latest_snapshot as latest_sysreq_snapshot, preview_commands as sysreq_preview_commands,
    recheck_missing_packages as recheck_system_missing_packages,
    refresh_metadata as refresh_system_metadata,
    refresh_preview_command as system_metadata_refresh_preview,
    resolve_plan as resolve_system_plan,
};
use ui::SystemDepsUi;

use crate::{
    cache::CompiledPackageCacheKey,
    lockfile::LockedPackage,
    r::{RVirtualEnv, r_version_async, remove_packages_from_venv},
    repository::{
        CranRepository, LocalRepository, PackageRepository, RepositoryError, RrepoRepository,
        built_in_repository, parse_repository_url,
    },
    resolver::PackageVersion,
};

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_INSTALL_WORKERS: usize = 8;

#[derive(Debug, Error, Diagnostic)]
enum RpxError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Project(#[from] project::ProjectError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] description::DescriptionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repo(#[from] RepoError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Add(#[from] AddError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Run(#[from] RunError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Sync(Box<SyncError>),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Clean(#[from] CleanError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    R(#[from] r::RError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Status(#[from] StatusError),
}

#[derive(Debug, Error, Diagnostic)]
enum RepoError {
    #[error("failed to add repository {url}: {source}")]
    #[diagnostic(code(rpx::repo::add_failed))]
    Add {
        url: String,
        #[source]
        source: RepositoryError,
    },

    #[error("failed to remove repository credential: {details}")]
    #[diagnostic(code(rpx::repo::credential_remove_failed))]
    CredentialRemove { details: String },

    #[error("failed to inspect repository credential: {details}")]
    #[diagnostic(code(rpx::repo::credential_inspect_failed))]
    CredentialInspect { details: String },
}

#[derive(Debug, Error, Diagnostic)]
enum AddError {
    #[error("package not found in configured repositories: {packages}")]
    #[diagnostic(code(rpx::add::package_not_found), help("{help}"))]
    PackageNotFound { packages: String, help: String },

    #[error("invalid package constraint {package}: {details}")]
    #[diagnostic(
        code(rpx::add::invalid_constraint),
        help("Use PACKAGE@OPERATORVERSION, for example digest@>=0.6.37.")
    )]
    InvalidConstraint { package: String, details: String },
}

#[derive(Debug)]
struct PackageVersionMismatch {
    package: String,
    installed: Version,
    expected: Version,
}

#[derive(Debug, Default)]
struct StatusMismatches {
    missing_packages: Vec<String>,
    extra_packages: Vec<String>,
    version_mismatches: Vec<PackageVersionMismatch>,
    missing_system_packages: Vec<String>,
    unsupported_system_rules: Vec<String>,
}

impl StatusMismatches {
    fn is_empty(&self) -> bool {
        self.missing_packages.is_empty()
            && self.extra_packages.is_empty()
            && self.version_mismatches.is_empty()
            && self.missing_system_packages.is_empty()
            && self.unsupported_system_rules.is_empty()
    }
}

impl std::fmt::Display for StatusMismatches {
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
        if !self.missing_system_packages.is_empty() {
            groups.push(format!(
                "Missing system packages for this host:\n- {}",
                self.missing_system_packages.join("\n- ")
            ));
        }
        if !self.unsupported_system_rules.is_empty() {
            groups.push(format!(
                "System requirement rules without a host mapping:\n- {}",
                self.unsupported_system_rules.join("\n- ")
            ));
        }

        formatter.write_str(&groups.join("\n\n"))
    }
}

#[derive(Debug, Error, Diagnostic)]
enum StatusError {
    #[error("project is out of sync\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::status::out_of_sync),
        help("Run `rpx sync` to synchronize the project.")
    )]
    OutOfSync { mismatches: StatusMismatches },
}

impl From<SyncError> for RpxError {
    fn from(error: SyncError) -> Self {
        Self::Sync(Box::new(error))
    }
}

#[derive(Debug, Error, Diagnostic)]
enum RunError {
    #[error("failed to run {program}")]
    #[diagnostic(code(rpx::run::command_failed))]
    CommandFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
enum LockError {
    #[error("failed to resolve package set from registry: {details}")]
    #[diagnostic(
        code(rpx::lock::resolve_failed),
        help("Check package names and version constraints in DESCRIPTION.")
    )]
    ResolveFailed { details: String },

    #[error("package requirements are incompatible\n\n{explanation}")]
    #[diagnostic(
        code(rpx::lock::no_solution),
        help("Adjust package constraints in DESCRIPTION and try again.")
    )]
    NoSolution { explanation: String },

    #[error("repository operation failed: {source}")]
    #[diagnostic(code(rpx::lock::repository_failed))]
    Repository {
        #[source]
        source: RepositoryError,
    },

    #[error("failed to resolve package set from registry")]
    #[diagnostic(
        code(rpx::lock::resolve_failed),
        help("Check package names and version constraints in DESCRIPTION.")
    )]
    Resolution {
        #[source]
        source: ResolutionError,
    },

    #[error("repository {repository} cannot be written to the lockfile")]
    #[diagnostic(code(rpx::lock::unsupported_repository))]
    UnsupportedRepository { repository: String },
}

fn lock_error_from_resolution(error: ResolutionError) -> LockError {
    match error {
        ResolutionError::PubGrub(PubGrubError::NoSolution(mut derivation_tree)) => {
            derivation_tree.collapse_no_versions();
            LockError::NoSolution {
                explanation: DefaultStringReporter::report(&derivation_tree),
            }
        }
        source => LockError::Resolution { source },
    }
}

#[derive(Debug, Error, Diagnostic)]
enum SyncError {
    #[error(
        "system dependency installation is currently supported only on supported Linux distributions/package managers"
    )]
    #[diagnostic(code(rpx::sync::unsupported_system_install))]
    UnsupportedSystemInstall,

    #[error("failed to refresh package metadata: {details}")]
    #[diagnostic(code(rpx::sync::metadata_refresh_failed))]
    MetadataRefreshFailed { details: String },

    #[error("failed to install system dependencies: {details}")]
    #[diagnostic(code(rpx::sync::system_dependencies_failed))]
    SystemDependenciesFailed { details: String },

    #[error("failed to prepare source artifacts: {details}")]
    #[diagnostic(code(rpx::sync::download_failed))]
    DownloadArtifactsFailed { details: String },

    #[error("failed to install project package: {source}")]
    #[diagnostic(code(rpx::sync::project_install_failed))]
    ProjectPackageInstall {
        #[source]
        source: r::PackageInstallError,
    },
}

#[derive(Debug, Error, Diagnostic)]
enum CleanError {
    #[error("failed to remove {label} at {path}")]
    #[diagnostic(code(rpx::clean::remove_failed))]
    RemoveFailed {
        label: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
enum RpxWarning {
    #[error("using cached system requirements database snapshot")]
    #[diagnostic(
        severity(Warning),
        code(rpx::sysreqs::cached_snapshot),
        help("Run `rpx lock` later to refresh locked system dependency metadata.")
    )]
    CachedSysreqSnapshot,

    #[error("using system requirements database pinned by the existing lockfile ({commit})")]
    #[diagnostic(
        severity(Warning),
        code(rpx::sysreqs::pinned_snapshot),
        help("Run `rpx lock` later to refresh locked system dependency metadata.")
    )]
    PinnedSysreqSnapshot { commit: String },

    #[error(
        "system requirements database unavailable; continuing without updating locked system dependency rules"
    )]
    #[diagnostic(
        severity(Warning),
        code(rpx::sysreqs::unavailable),
        help("Check network access and run `rpx lock` again when the database is reachable.")
    )]
    SysreqUnavailable,

    #[error("failed to prepare system dependency plan: {details}")]
    #[diagnostic(
        severity(Warning),
        code(rpx::sysreqs::plan_failed),
        help("rpx will continue with the system requirement rules recorded in rpx.lock.")
    )]
    SystemPlanFailed { details: String },

    #[error("some system requirement rules do not have an install mapping for {host}: {rules}")]
    #[diagnostic(severity(Warning), code(rpx::sysreqs::unsupported_rules))]
    UnsupportedSystemRequirementRules { host: String, rules: String },

    #[error("continuing with R package sync without installing system dependencies")]
    #[diagnostic(
        severity(Warning),
        code(rpx::sync::system_dependencies_skipped),
        help("Run `rpx sync --install-system` to install missing system dependencies first.")
    )]
    ContinuingWithoutSystemDependencies,
}

/// Runs the CLI application.
///
/// # Errors
///
/// Returns an error when command execution or diagnostic rendering fails.
pub async fn run() -> miette::Result<()> {
    run_inner().await?;
    Ok(())
}

fn init_tracing() {
    let indicatif_layer = tracing_indicatif::IndicatifLayer::new()
        .with_span_field_formatter(hide_indicatif_span_fields(DefaultFields::new()))
        .with_progress_style(progress_spinner_style())
        .with_max_progress_bars(
            10,
            Some(
                ProgressStyle::with_template("...and {pending_progress_bars} more packages")
                    .expect("progress footer style should be valid"),
            ),
        );
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,reqwest_tracing=info,rpx=info"));
    let fmt_layer =
        tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer());

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(indicatif_layer.with_filter(IndicatifFilter::new(false)))
        .try_init();
}

fn progress_spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{span_child_prefix}{spinner} {msg}")
        .expect("progress spinner style should be valid")
}

fn progress_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{span_child_prefix}{spinner} {msg} [{bar:24.cyan/blue}] {bytes}/{total_bytes}",
    )
    .expect("progress bar style should be valid")
}

async fn run_inner() -> Result<(), RpxError> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Add {
            default_repo,
            no_default_repo,
            packages,
        } => {
            cmd_add(
                &packages,
                DefaultRepositoryPreference::from_flags(default_repo, no_default_repo),
            )
            .await
        }
        Commands::Remove {
            default_repo,
            no_default_repo,
            packages,
        } => {
            cmd_remove(
                &packages,
                DefaultRepositoryPreference::from_flags(default_repo, no_default_repo),
            )
            .await
        }
        Commands::Run { command } => cmd_run(&command).await,
        Commands::Lock {
            default_repo,
            no_default_repo,
        } => {
            cmd_lock(DefaultRepositoryPreference::from_flags(
                default_repo,
                no_default_repo,
            ))
            .await
        }
        Commands::Status => cmd_status().await,
        Commands::Sync {
            install_system,
            install_only_system,
        } => cmd_sync(install_system, install_only_system).await,
        Commands::Clean => cmd_clean(),
        Commands::Repo { command } => cmd_repo(command).await,
    }
}

fn cmd_init() -> Result<(), RpxError> {
    let path = init_description()?;
    status(format_args!("Initialized project at {path}"));
    status("Next: run `rpx add <package>` or `rpx lock`");
    Ok(())
}

async fn cmd_add(
    packages: &[String],
    repository_preference: DefaultRepositoryPreference,
) -> Result<(), RpxError> {
    let packages = packages
        .iter()
        .map(|package| parse_add_package(package))
        .collect::<Result<Vec<_>, _>>()?;
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let mut description = project
        .description()
        .map_err(project::ProjectError::Manifest)?
        .clone();
    let old_lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled =
        repository_preference.enabled(&description, old_lockfile.as_ref());
    let repositories = effective_package_repositories(&description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    let mut desired_roots = roots_from_description(&description);
    let new_packages = packages
        .iter()
        .filter(|package| package.relation.is_none())
        .filter(|package| !roots_contain_package(&desired_roots, &package.name))
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    let mut added_relations = add_relations_for_packages(&repositories, &new_packages).await?;
    desired_roots.extend(added_relations.iter().cloned());
    for package in &packages {
        let Some(relation) = &package.relation else {
            continue;
        };

        desired_roots.retain(|existing| existing.name() != relation.name());
        desired_roots.insert(relation.clone());
        added_relations.insert(relation.clone());
    }
    apply_added_packages_to_description(&mut description, &added_relations)?;
    let (lockfile, required) =
        match project.validate_locked_resolution(&description, &locked_repositories, &r_version) {
            Ok(lockfile) => (
                lockfile.clone(),
                project
                    .required_packages_from_lockfile(&description)
                    .map_err(project::ProjectError::LockedPackages)?,
            ),
            Err(error) if error.allows_relock() => {
                let preferred_versions = preferred_versions_from_lockfile(
                    old_lockfile.as_ref(),
                    &repositories,
                    &new_packages.iter().cloned().collect::<BTreeSet<_>>(),
                )?;
                let root = project_repository(&project, &description);
                let selected = resolve_from_registry(
                    repositories.clone(),
                    Arc::clone(&root),
                    desired_roots.clone(),
                    preferred_versions,
                )
                .await
                .map_err(lock_error_from_resolution)?;
                let required = hydrate_required_packages(root, selected).await?;
                let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile.as_ref()).await;
                let lockfile = lockfile_from_required_packages(
                    desired_roots,
                    &required,
                    &sysreq_db,
                    &repositories,
                    &r_version,
                )?;
                (lockfile, required)
            }
            Err(source) => {
                return Err(project::ProjectError::LockedResolution(source).into());
            }
        };

    project
        .write_description(&description)
        .map_err(project::ProjectError::ManifestWrite)?;
    project
        .write_lockfile(&lockfile)
        .map_err(project::ProjectError::LockfileWrite)?;
    sync_system_dependencies(&lockfile, false, false)?;
    let installed = installed_packages()
        .await
        .map_err(r::RError::InstalledPackages)?;
    sync_packages(required, installed, &r_version).await?;
    status(format_args!(
        "Added {}",
        packages
            .iter()
            .map(|package| package.display())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(())
}

async fn cmd_repo(command: RepoCommands) -> Result<(), RpxError> {
    match command {
        RepoCommands::Add { url } => cmd_repo_add(&url).await,
        RepoCommands::Remove {
            url,
            remove_credential,
        } => cmd_repo_remove(&url, remove_credential).await,
        RepoCommands::List => cmd_repo_list().await,
    }
}

async fn cmd_repo_add(url: &str) -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let mut description = project
        .description()
        .map_err(project::ProjectError::Manifest)?
        .clone();
    let old_lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled = DefaultRepositoryPreference::FromLockfileOrDefault
        .enabled(&description, old_lockfile.as_ref());
    let new_repo_url = parse_repository_url(url).map_err(|source| RepoError::Add {
        url: url.trim().to_string(),
        source,
    })?;

    let mut additional_repositories = description.additional_repositories().unwrap_or_default();
    if additional_repositories.iter().any(|existing| {
        parse_repository_url(existing).is_ok_and(|existing| existing == new_repo_url)
    }) {
        status(format_args!(
            "Repository already configured: {}",
            new_repo_url.as_str()
        ));
        return Ok(());
    }

    additional_repositories.push(new_repo_url.to_string());
    let additional_repositories = additional_repositories
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    description.set_additional_repositories(&additional_repositories);
    let repositories = effective_package_repositories(&description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    match project.validate_locked_resolution(&description, &locked_repositories, &r_version) {
        Ok(_) => {}
        Err(error) if error.allows_relock() => {}
        Err(source) => {
            return Err(project::ProjectError::LockedResolution(source).into());
        }
    }

    let roots = roots_from_description(&description);
    let preferred_versions =
        preferred_versions_from_lockfile(old_lockfile.as_ref(), &repositories, &BTreeSet::new())?;
    let root = project_repository(&project, &description);
    let selected = resolve_from_registry(
        repositories.clone(),
        Arc::clone(&root),
        roots.clone(),
        preferred_versions,
    )
    .await
    .map_err(lock_error_from_resolution)?;
    let required = hydrate_required_packages(root, selected).await?;
    let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile.as_ref()).await;
    let lockfile =
        lockfile_from_required_packages(roots, &required, &sysreq_db, &repositories, &r_version)?;

    project
        .write_description(&description)
        .map_err(project::ProjectError::ManifestWrite)?;
    project
        .write_lockfile(&lockfile)
        .map_err(project::ProjectError::LockfileWrite)?;
    status(format_args!("Added repository {new_repo_url}"));
    Ok(())
}

async fn cmd_repo_remove(url: &str, remove_credential: bool) -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let mut description = project
        .description()
        .map_err(project::ProjectError::Manifest)?
        .clone();
    let old_lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled = DefaultRepositoryPreference::FromLockfileOrDefault
        .enabled(&description, old_lockfile.as_ref());
    let base_url = parse_repository_url(url).map_err(|source| RepoError::Add {
        url: url.trim().to_string(),
        source,
    })?;
    let normalized_url = base_url.to_string();

    let mut additional_repositories = description.additional_repositories().unwrap_or_default();
    let previous_len = additional_repositories.len();
    additional_repositories.retain(|existing| {
        parse_repository_url(existing).map_or(true, |existing| existing != base_url)
    });

    if additional_repositories.len() == previous_len {
        status(format_args!("Repository not configured: {normalized_url}"));
        return Ok(());
    }

    let additional_repositories = additional_repositories
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    description.set_additional_repositories(&additional_repositories);
    let repositories = effective_package_repositories(&description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    match project.validate_locked_resolution(&description, &locked_repositories, &r_version) {
        Ok(_) => {}
        Err(error) if error.allows_relock() => {}
        Err(source) => {
            return Err(project::ProjectError::LockedResolution(source).into());
        }
    }

    let roots = roots_from_description(&description);
    let preferred_versions =
        preferred_versions_from_lockfile(old_lockfile.as_ref(), &repositories, &BTreeSet::new())?;
    let root = project_repository(&project, &description);
    let selected = resolve_from_registry(
        repositories.clone(),
        Arc::clone(&root),
        roots.clone(),
        preferred_versions,
    )
    .await
    .map_err(lock_error_from_resolution)?;
    let required = hydrate_required_packages(root, selected).await?;
    let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile.as_ref()).await;
    let lockfile =
        lockfile_from_required_packages(roots, &required, &sysreq_db, &repositories, &r_version)?;

    if remove_credential {
        http::remove_stored_credential(&base_url).map_err(|error| RepoError::CredentialRemove {
            details: error.to_string(),
        })?;
    }

    project
        .write_description(&description)
        .map_err(project::ProjectError::ManifestWrite)?;
    project
        .write_lockfile(&lockfile)
        .map_err(project::ProjectError::LockfileWrite)?;
    status(format_args!("Removed repository {normalized_url}"));
    Ok(())
}

async fn cmd_repo_list() -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let description = project
        .description()
        .map_err(project::ProjectError::Manifest)?;
    let lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = if lockfile.is_some() {
        Some(r_version_async().await.map_err(r::RError::Version)?)
    } else {
        None
    };
    let lockfile = if lockfile.is_some() {
        let default_repository_enabled = DefaultRepositoryPreference::FromLockfileOrDefault
            .enabled(description, lockfile.as_ref());
        let repositories = effective_package_repositories(description, default_repository_enabled)
            .await
            .map_err(|source| LockError::Repository { source })?;
        let locked_repositories = locked_package_repositories(&repositories)?;
        match project.validate_locked_resolution(
            description,
            &locked_repositories,
            r_version
                .as_deref()
                .expect("R version should be present when the lockfile exists"),
        ) {
            Ok(_) => lockfile,
            Err(error) if error.allows_relock() => None,
            Err(source) => {
                return Err(project::ProjectError::LockedResolution(source).into());
            }
        }
    } else {
        None
    };
    let additional_repositories = description.additional_repositories().unwrap_or_default();

    if additional_repositories.is_empty() {
        status("No additional repositories configured");
        return Ok(());
    }

    for url in additional_repositories {
        let base_url = parse_repository_url(&url).map_err(|source| RepoError::Add {
            url: url.clone(),
            source,
        })?;
        let normalized_url = base_url.to_string();
        let credential = http::has_stored_credential(&base_url).map_err(|error| {
            RepoError::CredentialInspect {
                details: error.to_string(),
            }
        })?;
        status(format_args!(
            "{} [{}; {}]",
            normalized_url,
            repository_kind_label(lockfile.as_ref(), &normalized_url),
            if credential {
                "credential stored"
            } else {
                "no credential"
            }
        ));
    }

    Ok(())
}

async fn cmd_remove(
    packages: &[String],
    repository_preference: DefaultRepositoryPreference,
) -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let mut description = project
        .description()
        .map_err(project::ProjectError::Manifest)?
        .clone();
    let old_lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled =
        repository_preference.enabled(&description, old_lockfile.as_ref());
    let repositories = effective_package_repositories(&description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    let removed_packages = packages.iter().cloned().collect::<BTreeSet<_>>();
    remove_packages_from_description_dependencies(&mut description, &removed_packages);
    let desired_roots = roots_from_description(&description);
    let (lockfile, required) =
        match project.validate_locked_resolution(&description, &locked_repositories, &r_version) {
            Ok(lockfile) => (
                lockfile.clone(),
                project
                    .required_packages_from_lockfile(&description)
                    .map_err(project::ProjectError::LockedPackages)?,
            ),
            Err(error) if error.allows_relock() => {
                let preferred_versions = preferred_versions_from_lockfile(
                    old_lockfile.as_ref(),
                    &repositories,
                    &removed_packages,
                )?;
                let root = project_repository(&project, &description);
                let selected = resolve_from_registry(
                    repositories.clone(),
                    Arc::clone(&root),
                    desired_roots.clone(),
                    preferred_versions,
                )
                .await
                .map_err(lock_error_from_resolution)?;
                let required = hydrate_required_packages(root, selected).await?;
                let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile.as_ref()).await;
                let lockfile = lockfile_from_required_packages(
                    desired_roots,
                    &required,
                    &sysreq_db,
                    &repositories,
                    &r_version,
                )?;
                (lockfile, required)
            }
            Err(source) => {
                return Err(project::ProjectError::LockedResolution(source).into());
            }
        };

    project
        .write_description(&description)
        .map_err(project::ProjectError::ManifestWrite)?;
    project
        .write_lockfile(&lockfile)
        .map_err(project::ProjectError::LockfileWrite)?;
    sync_system_dependencies(&lockfile, false, false)?;
    let installed = installed_packages()
        .await
        .map_err(r::RError::InstalledPackages)?;
    let removed = packages
        .iter()
        .filter(|package| installed.contains_key(package.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = packages
        .iter()
        .filter(|package| !installed.contains_key(package.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    sync_packages(required, installed, &r_version).await?;

    if let Some(removed) = removed.into_iter().reduce(|mut packages, package| {
        packages.push_str(", ");
        packages.push_str(&package);
        packages
    }) {
        status(format_args!("Removed {removed}"));
    }
    if let Some(missing) = missing.into_iter().reduce(|mut packages, package| {
        packages.push_str(", ");
        packages.push_str(&package);
        packages
    }) {
        status(format_args!(
            "{missing} is already missing from the project library"
        ));
    }

    Ok(())
}

async fn cmd_run(command: &[String]) -> Result<(), RpxError> {
    let (program, args) = command
        .split_first()
        .expect("run command requires at least one argument");

    let status = Command::with_venv(program)
        .args(args)
        .status()
        .await
        .map_err(|source| RunError::CommandFailed {
            program: program.clone(),
            source,
        })?;

    exit_with_status(status.code());
    Ok(())
}

async fn cmd_lock(repository_preference: DefaultRepositoryPreference) -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let description = project
        .description()
        .map_err(project::ProjectError::Manifest)?;
    let old_lockfile = project
        .lockfile_optional()
        .map_err(project::ProjectError::Lockfile)?
        .cloned();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled =
        repository_preference.enabled(description, old_lockfile.as_ref());
    let repositories = effective_package_repositories(description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    match project.validate_locked_resolution(description, &locked_repositories, &r_version) {
        Ok(_) => {}
        Err(error) if error.allows_relock() => {}
        Err(source) => {
            return Err(project::ProjectError::LockedResolution(source).into());
        }
    }
    let roots = roots_from_description(description);
    let preferred_versions =
        preferred_versions_from_lockfile(old_lockfile.as_ref(), &repositories, &BTreeSet::new())?;

    let root = project_repository(&project, description);
    let selected = resolve_from_registry(
        repositories.clone(),
        Arc::clone(&root),
        roots.clone(),
        preferred_versions,
    )
    .await
    .map_err(lock_error_from_resolution)?;
    let required = hydrate_required_packages(root, selected).await?;
    let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile.as_ref()).await;
    let lockfile =
        lockfile_from_required_packages(roots, &required, &sysreq_db, &repositories, &r_version)?;
    let changed = old_lockfile.as_ref() != Some(&lockfile);
    project
        .write_lockfile(&lockfile)
        .map_err(project::ProjectError::LockfileWrite)?;

    if changed {
        status("Updated rpx.lock");
    } else {
        status("rpx.lock is already up to date");
    }
    Ok(())
}

async fn cmd_sync(install_system: bool, install_only_system: bool) -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let description = project
        .description()
        .map_err(project::ProjectError::Manifest)?;
    let old_lockfile = project
        .lockfile()
        .map_err(project::ProjectError::Lockfile)?
        .clone();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled = DefaultRepositoryPreference::FromLockfileOrDefault
        .enabled(description, Some(&old_lockfile));
    let repositories = effective_package_repositories(description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    let lockfile = project
        .validate_locked_resolution(description, &locked_repositories, &r_version)
        .map_err(project::ProjectError::LockedResolution)?;

    sync_system_dependencies(&lockfile, install_system, install_only_system)?;
    if install_only_system {
        return Ok(());
    }

    let required = project
        .required_packages_from_lockfile(description)
        .map_err(project::ProjectError::LockedPackages)?;
    let installed = installed_packages()
        .await
        .map_err(r::RError::InstalledPackages)?;
    sync_packages(required, installed, &r_version).await?;
    status("Synchronized project library");
    Ok(())
}

async fn cmd_status() -> Result<(), RpxError> {
    let project = Project::discover().map_err(project::ProjectError::Discovery)?;
    let description = project
        .description()
        .map_err(project::ProjectError::Manifest)?;
    let old_lockfile = project
        .lockfile()
        .map_err(project::ProjectError::Lockfile)?
        .clone();
    let r_version = r_version_async().await.map_err(r::RError::Version)?;
    let default_repository_enabled = DefaultRepositoryPreference::FromLockfileOrDefault
        .enabled(description, Some(&old_lockfile));
    let repositories = effective_package_repositories(description, default_repository_enabled)
        .await
        .map_err(|source| LockError::Repository { source })?;
    let locked_repositories = locked_package_repositories(&repositories)?;
    let lockfile = project
        .validate_locked_resolution(description, &locked_repositories, &r_version)
        .map_err(project::ProjectError::LockedResolution)?;
    let locked_packages = project
        .locked_packages()
        .map_err(project::ProjectError::LockedPackages)?;

    let installed = installed_packages()
        .await
        .map_err(r::RError::InstalledPackages)?;
    let missing_packages = locked_packages
        .keys()
        .filter(|package| !installed.contains_key(*package))
        .cloned()
        .collect();
    let version_mismatches = locked_packages
        .iter()
        .filter_map(|(package, expected)| {
            installed
                .get(package)
                .filter(|installed| *installed != expected)
                .map(|installed| PackageVersionMismatch {
                    package: package.clone(),
                    installed: installed.version().clone(),
                    expected: expected.version().clone(),
                })
        })
        .collect();
    let extra_packages = installed
        .keys()
        .filter(|package| !locked_packages.contains_key(*package))
        .cloned()
        .collect();
    let mut mismatches = StatusMismatches {
        missing_packages,
        extra_packages,
        version_mismatches,
        ..StatusMismatches::default()
    };

    let system_plan = if host_supports_system_sync() {
        system_plan_from_lockfile(lockfile).ok()
    } else {
        None
    };
    if let Some(plan) = system_plan {
        mismatches.missing_system_packages = plan.missing_packages;
        mismatches.unsupported_system_rules = plan.unsupported_rules;
    }

    if !mismatches.is_empty() {
        return Err(StatusError::OutOfSync { mismatches }.into());
    }

    status("Project is in sync");
    Ok(())
}

fn cmd_clean() -> Result<(), RpxError> {
    let mut removed_any = false;

    removed_any |= remove_dir_if_exists(&project_library_root_path(), "project library")?;
    removed_any |= remove_dir_if_exists(&cache_dir_path(), "cache directory")?;

    if removed_any {
        status("Removed project library and cache directories");
    } else {
        status("Project library and cache directories are already clean");
    }
    Ok(())
}

fn remove_dir_if_exists(path: &Path, label: &str) -> Result<bool, RpxError> {
    if !path.exists() {
        return Ok(false);
    }

    fs::remove_dir_all(path).map_err(|source| CleanError::RemoveFailed {
        label: label.to_string(),
        path: path.display().to_string(),
        source,
    })?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultRepositoryPreference {
    FromLockfileOrDefault,
    Enabled,
    Disabled,
}

impl DefaultRepositoryPreference {
    fn from_flags(default_repo: bool, no_default_repo: bool) -> Self {
        if default_repo {
            Self::Enabled
        } else if no_default_repo {
            Self::Disabled
        } else {
            Self::FromLockfileOrDefault
        }
    }

    fn enabled(self, description: &RDescription, lockfile: Option<&Lockfile>) -> bool {
        match self {
            Self::Enabled => true,
            Self::Disabled => false,
            Self::FromLockfileOrDefault => lockfile
                .and_then(|lockfile| locked_default_repository_enabled(description, lockfile))
                .unwrap_or(true),
        }
    }
}

async fn effective_package_repositories(
    description: &RDescription,
    default_repository_enabled: bool,
) -> Result<Vec<Arc<dyn PackageRepository>>, RepositoryError> {
    let additional_repositories = description.additional_repositories().unwrap_or_default();
    let mut repositories = futures_util::future::join_all(
        additional_repositories
            .iter()
            .map(|url| async move { <dyn PackageRepository>::from_url(url).await }),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;

    if default_repository_enabled {
        repositories.insert(0, default_repository().await?);
    }

    Ok(repositories)
}

fn roots_from_description(description: &RDescription) -> BTreeSet<Relation> {
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

fn project_repository(project: &Project, description: &RDescription) -> Arc<LocalRepository> {
    Arc::new(
        LocalRepository::new(project.path().to_path_buf()).with_description(description.clone()),
    )
}

fn roots_contain_package(roots: &BTreeSet<Relation>, package: &str) -> bool {
    roots.iter().any(|relation| relation.name() == package)
}

#[derive(Debug)]
struct AddPackage {
    name: String,
    relation: Option<Relation>,
}

impl AddPackage {
    fn display(&self) -> String {
        self.relation
            .as_ref()
            .map_or_else(|| self.name.clone(), ToString::to_string)
    }
}

fn parse_add_package(package: &str) -> Result<AddPackage, AddError> {
    if package.is_empty() || package.chars().any(char::is_whitespace) {
        return Err(invalid_add_constraint(
            package,
            "package specifications cannot contain whitespace",
        ));
    }

    let Some((name, constraint)) = package.split_once('@') else {
        return Ok(AddPackage {
            name: package.to_string(),
            relation: None,
        });
    };
    if name.is_empty() {
        return Err(invalid_add_constraint(package, "package name is missing"));
    }

    let (operator, version) = [">=", "<=", "==", "!=", ">", "<"]
        .into_iter()
        .find_map(|operator| {
            constraint
                .strip_prefix(operator)
                .map(|version| (operator, version))
        })
        .ok_or_else(|| {
            invalid_add_constraint(package, "version constraint operator is missing or invalid")
        })?;
    if version.is_empty() {
        return Err(invalid_add_constraint(package, "version is missing"));
    }

    let operator = operator
        .parse::<VersionConstraint>()
        .map_err(|details| invalid_add_constraint(package, &details))?;
    let version = version
        .parse::<Version>()
        .map_err(|details| invalid_add_constraint(package, &details))?;

    Ok(AddPackage {
        name: name.to_string(),
        relation: Some(Relation::new(name, Some((operator, version)))),
    })
}

fn invalid_add_constraint(package: &str, details: impl Into<String>) -> AddError {
    AddError::InvalidConstraint {
        package: package.to_string(),
        details: details.into(),
    }
}

async fn add_relations_for_packages(
    repositories: &[Arc<dyn PackageRepository>],
    packages: &[String],
) -> Result<BTreeSet<Relation>, RpxError> {
    let non_base_packages = packages
        .iter()
        .filter(|package| !is_base_package(package))
        .cloned()
        .collect::<Vec<_>>();
    let latest_versions = latest_package_versions_for_add(repositories, &non_base_packages).await?;
    let mut relations = BTreeSet::new();

    for package in packages {
        if is_base_package(package) {
            relations.insert(Relation::simple(package));
            continue;
        }

        let latest = latest_versions
            .get(package)
            .expect("latest version should exist for every non-base package");
        relations.extend(
            pinned_package_relations(package, latest.version())
                .map_err(|details| LockError::ResolveFailed { details })?,
        );
    }

    Ok(relations)
}

async fn latest_package_versions_for_add(
    repositories: &[Arc<dyn PackageRepository>],
    packages: &[String],
) -> Result<BTreeMap<String, PackageVersion>, RpxError> {
    if packages.is_empty() {
        return Ok(BTreeMap::new());
    }

    let requested = packages.iter().cloned().collect::<BTreeSet<_>>();
    let mut selected = BTreeMap::<String, PackageVersion>::new();
    let mut known_packages = BTreeSet::<String>::new();
    let package_indexes =
        futures_util::future::join_all(repositories.iter().map(|repository| async {
            repository
                .packages()
                .await
                .map_err(|details| (repository.to_string(), details))
        }))
        .await;

    for result in package_indexes {
        let available = result.map_err(|(url, details)| LockError::ResolveFailed {
            details: format!("failed to load package index from {url}: {details}"),
        })?;
        known_packages.extend(available.keys().cloned());

        for package in &requested {
            let Some(version) = available.get(package) else {
                continue;
            };

            match selected.entry(package.clone()) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if version.version() > entry.get().version() {
                        entry.insert(version.clone());
                    }
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(version.clone());
                }
            }
        }
    }

    let missing = requested
        .iter()
        .filter(|package| !selected.contains_key(*package))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(AddError::PackageNotFound {
            help: package_not_found_help(&missing, &known_packages),
            packages: missing.join(", "),
        }
        .into());
    }

    Ok(selected)
}

const PACKAGE_SUGGESTION_THRESHOLD: f64 = 0.84;
const MAX_PACKAGE_SUGGESTIONS: usize = 5;

fn package_not_found_help(missing: &[String], known_packages: &BTreeSet<String>) -> String {
    let suggestions = missing
        .iter()
        .filter_map(|package| {
            let suggestions = package_suggestions(package, known_packages);
            (!suggestions.is_empty())
                .then(|| format!("For {package}, did you mean {}?", suggestions.join(", ")))
        })
        .collect::<Vec<_>>();

    if suggestions.is_empty() {
        "Check the package name or add a repository that contains it.".to_string()
    } else {
        suggestions.join(" ")
    }
}

fn package_suggestions(package: &str, known_packages: &BTreeSet<String>) -> Vec<String> {
    let package_lower = package.to_ascii_lowercase();
    let mut scored = known_packages
        .iter()
        .filter(|candidate| candidate.as_str() != package)
        .filter_map(|candidate| {
            let candidate_lower = candidate.to_ascii_lowercase();
            let score = strsim::jaro_winkler(&package_lower, &candidate_lower);
            (score >= PACKAGE_SUGGESTION_THRESHOLD).then(|| (score, candidate.clone()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
    });

    scored
        .into_iter()
        .take(MAX_PACKAGE_SUGGESTIONS)
        .map(|(_, package)| package)
        .collect()
}

fn pinned_package_relations(package: &str, latest: &Version) -> Result<Vec<Relation>, String> {
    let next_major = next_major_version(latest)?;
    Ok(vec![
        Relation::new(
            package,
            Some((VersionConstraint::GreaterThanEqual, latest.clone())),
        ),
        Relation::new(package, Some((VersionConstraint::LessThan, next_major))),
    ])
}

fn next_major_version(version: &Version) -> Result<Version, String> {
    let major = version
        .components
        .first()
        .ok_or_else(|| format!("latest version is not semver-like: {version}"))?;
    let next_major = major
        .checked_add(1)
        .ok_or_else(|| format!("latest version major component is too large: {version}"))?;

    format!("{next_major}.0.0")
        .parse()
        .map_err(|error| format!("failed to build next major version for {version}: {error}"))
}

fn locked_root_from_relation(relation: &Relation) -> lockfile::LockedRoot {
    lockfile::LockedRoot {
        package: relation.name(),
        constraint: relation.version().map_or_else(
            || "*".to_string(),
            |(operator, version)| format!("{operator} {version}"),
        ),
    }
}

fn preferred_versions_from_lockfile(
    lockfile: Option<&Lockfile>,
    repositories: &[Arc<dyn PackageRepository>],
    excluded_packages: &BTreeSet<String>,
) -> Result<BTreeMap<String, PackageVersion>, RpxError> {
    let Some(lockfile) = lockfile else {
        return Ok(BTreeMap::new());
    };

    lockfile
        .packages
        .iter()
        .filter(|(name, _)| !excluded_packages.contains(name.as_str()))
        .map(|(name, package)| {
            let repository = repository_for_locked_package(repositories, package)?;
            let version = package
                .version
                .parse()
                .map_err(|error| LockError::ResolveFailed {
                    details: format!(
                        "invalid locked version {} for {name}: {error}",
                        package.version
                    ),
                })?;

            let version = PackageVersion::new(version, repository);

            Ok((name.clone(), version))
        })
        .collect()
}

fn repository_for_locked_package(
    repositories: &[Arc<dyn PackageRepository>],
    package: &LockedPackage,
) -> Result<Arc<dyn PackageRepository>, RpxError> {
    if let Some(source_url) = package.source_url.as_deref()
        && let Some(repository) = repositories.iter().find(|repository| {
            remote_repository_url(repository.as_ref())
                .is_some_and(|url| source_url.starts_with(url.as_str()))
        })
    {
        return Ok(Arc::clone(repository));
    }

    repositories.first().cloned().ok_or_else(|| {
        LockError::ResolveFailed {
            details: format!(
                "no repository available for locked package {}",
                package.package
            ),
        }
        .into()
    })
}

fn remote_repository_url(repository: &dyn PackageRepository) -> Option<&reqwest::Url> {
    if let Some(repository) = repository.downcast_ref::<RrepoRepository>() {
        Some(repository.url())
    } else {
        repository
            .downcast_ref::<CranRepository>()
            .map(CranRepository::url)
    }
}

fn apply_added_packages_to_description(
    description: &mut RDescription,
    added_relations: &BTreeSet<Relation>,
) -> Result<(), RpxError> {
    let added_packages = added_relations
        .iter()
        .map(|relation| relation.name())
        .collect::<BTreeSet<_>>();
    remove_packages_from_description_dependencies(description, &added_packages);

    let mut imports = description.imports().unwrap_or_default();
    let mut imports_changed = false;

    for relation in added_relations {
        if is_base_package(&relation.name()) {
            continue;
        }

        imports.push(relation.clone());
        imports_changed = true;
    }

    if imports_changed {
        description.set_imports(imports);
    }

    Ok(())
}

fn remove_packages_from_description_dependencies(
    description: &mut RDescription,
    packages: &BTreeSet<String>,
) {
    if let Some(depends) = description.depends() {
        let retained = depends
            .iter()
            .filter(|dependency| {
                let name = dependency.name();
                !packages.contains(name.as_str())
            })
            .collect::<Vec<_>>();
        description.set_depends(Relations::from(retained));
    }

    if let Some(imports) = description.imports() {
        let retained = imports
            .iter()
            .filter(|dependency| {
                let name = dependency.name();
                !packages.contains(name.as_str())
            })
            .collect::<Vec<_>>();
        description.set_imports(Relations::from(retained));
    }

    if let Some(linking_to) = description.linking_to() {
        let retained = linking_to
            .iter()
            .filter(|dependency| {
                let name = dependency.name();
                !packages.contains(name.as_str())
            })
            .collect::<Vec<_>>();
        description.set_linking_to(Relations::from(retained));
    }

    if let Some(suggests) = description.suggests() {
        let retained = suggests
            .iter()
            .filter(|dependency| {
                let name = dependency.name();
                !packages.contains(name.as_str())
            })
            .collect::<Vec<_>>();
        description.set_suggests(Relations::from(retained));
    }

    if let Some(enhances) = description.enhances() {
        let retained = enhances
            .iter()
            .filter(|dependency| {
                let name = dependency.name();
                !packages.contains(name.as_str())
            })
            .collect::<Vec<_>>();
        description.set_enhances(Relations::from(retained));
    }
}

async fn load_sysreq_snapshot_for_lock(
    existing_lockfile: Option<&Lockfile>,
) -> sysreqs::SysreqDbSnapshot {
    let existing_commit = existing_lockfile
        .map(|lockfile| lockfile.sysreqs.db_commit.as_str())
        .filter(|commit| !commit.is_empty())
        .map(ToString::to_string);

    tokio::task::spawn_blocking(move || load_sysreq_snapshot_for_lock_blocking(existing_commit))
        .await
        .unwrap_or_else(|_| empty_sysreq_snapshot())
}

fn load_sysreq_snapshot_for_lock_blocking(
    existing_commit: Option<String>,
) -> sysreqs::SysreqDbSnapshot {
    if let Ok(snapshot) = latest_sysreq_snapshot() {
        return snapshot;
    }

    if let Ok(Some(snapshot)) = cached_latest_snapshot() {
        warning(RpxWarning::CachedSysreqSnapshot);
        return snapshot;
    }

    if let Some(commit) = existing_commit
        && let Ok(snapshot) = sysreqs::snapshot_for_commit(&commit)
    {
        warning(RpxWarning::PinnedSysreqSnapshot { commit });
        return snapshot;
    }

    warning(RpxWarning::SysreqUnavailable);
    empty_sysreq_snapshot()
}

async fn sync_packages(
    required: RequiredPackages,
    installed: BTreeMap<String, PackageVersion>,
    r_version: &str,
) -> Result<(), RpxError> {
    let packages_to_remove = installed
        .iter()
        .filter(|(name, installed_version)| {
            required
                .get(*name)
                .is_none_or(|(required_version, _)| required_version != *installed_version)
        })
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let retained = installed
        .iter()
        .filter(|(name, installed_version)| {
            required
                .get(*name)
                .is_some_and(|(required_version, _)| required_version == *installed_version)
        })
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let packages_to_install = required
        .into_iter()
        .filter(|(name, (required_version, _))| {
            installed
                .get(name)
                .is_none_or(|installed_version| installed_version != required_version)
        })
        .collect();

    remove_packages_from_venv(&packages_to_remove).map_err(r::RError::Remove)?;
    install_required_packages(packages_to_install, retained, r_version).await?;

    Ok(())
}

pub(crate) fn exit_with_status(code: Option<i32>) {
    if code != Some(0) {
        std::process::exit(code.unwrap_or(1));
    }
}

async fn default_repository() -> Result<Arc<dyn PackageRepository>, RepositoryError> {
    match env::var("RPX_REGISTRY_BASE_URL") {
        Ok(url) => <dyn PackageRepository>::from_url(&url).await,
        Err(_) => Ok(built_in_repository()),
    }
}

fn repository_kind_label(lockfile: Option<&Lockfile>, url: &str) -> &'static str {
    let Ok(url) = parse_repository_url(url) else {
        return "unknown";
    };
    lockfile
        .and_then(|lockfile| {
            lockfile.repositories.iter().find(|repository| {
                parse_repository_url(&repository.url).is_ok_and(|repository| repository == url)
            })
        })
        .map(|repository| match repository.kind {
            LockedRepositoryKind::Rrepo => "rrepo",
            LockedRepositoryKind::CranLike => "CRAN-like",
        })
        .unwrap_or("unknown")
}

async fn hydrate_required_packages(
    root: Arc<LocalRepository>,
    selected: BTreeMap<String, PackageVersion>,
) -> Result<RequiredPackages, RpxError> {
    // TODO: make sure the web requests are under a central semaphore in the repos not here
    let descriptions =
        futures_util::future::join_all(selected.into_iter().map(|(name, version)| async move {
            let description = version
                .repository()
                .description(&name, version.version())
                .await
                .map_err(|source| LockError::Repository { source })?;
            Ok::<_, LockError>((name, (version, description)))
        }))
        .await;
    let mut required = descriptions
        .into_iter()
        .collect::<Result<RequiredPackages, _>>()?;
    let root_versions = root
        .packages()
        .await
        .map_err(|source| LockError::Repository { source })?;
    let (root_name, root_version) =
        root_versions
            .into_iter()
            .next()
            .ok_or_else(|| LockError::ResolveFailed {
                details: "project repository did not provide its root package".to_string(),
            })?;
    let root_description = root
        .description()
        .await
        .map_err(|source| LockError::Repository { source })?;
    required.insert(root_name, (root_version, root_description));

    Ok(required)
}

fn lockfile_from_required_packages(
    roots: BTreeSet<Relation>,
    required: &RequiredPackages,
    sysreq_db: &sysreqs::SysreqDbSnapshot,
    repositories: &[Arc<dyn PackageRepository>],
    r_version: &str,
) -> Result<Lockfile, LockError> {
    let mut packages = BTreeMap::new();
    let mut sysreq_packages = BTreeMap::new();

    for (name, (version, description)) in required {
        if version
            .repository()
            .as_ref()
            .downcast_ref::<LocalRepository>()
            .is_some()
        {
            continue;
        }

        let dependencies = locked_dependencies_from_description(description)
            .map_err(|details| LockError::ResolveFailed { details })?;

        let rules = sysreqs::match_rules(description, sysreq_db);

        if !rules.is_empty() {
            sysreq_packages.insert(name.clone(), rules);
        }

        packages.insert(
            name.clone(),
            LockedPackage {
                package: name.clone(),
                version: version.version().to_string(),
                source: Some("repository".to_string()),
                source_url: Some(package_source_url(&name, &version)?),
                dependencies,
            },
        );
    }

    let sysreq_rules = sysreq_packages
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let required_base_packages = locked_base_packages_from_locked(&roots, packages.values());

    Ok(Lockfile {
        version: LOCKFILE_VERSION,
        revision: LOCKFILE_REVISION,
        repositories: locked_package_repositories(repositories)?,
        r: LockedR {
            version: r_version.to_string(),
            base_packages: required_base_packages,
        },
        sysreqs: LockedSystemRequirements {
            db_commit: sysreq_db.commit.clone(),
            rules: sysreq_rules,
            packages: sysreq_packages,
        },
        roots: roots.iter().map(locked_root_from_relation).collect(),
        packages,
    })
}

fn locked_dependencies_from_description(
    description: &RDescription,
) -> Result<Vec<lockfile::LockedDependency>, String> {
    let depends = description.depends();
    let imports = description.imports();
    let linking_to = description.linking_to();

    locked_dependencies_from_relations_fields(
        depends.as_ref(),
        imports.as_ref(),
        linking_to.as_ref(),
    )
}

fn package_dependency_names(description: &RDescription) -> BTreeSet<String> {
    description
        .depends()
        .into_iter()
        .flat_map(|relations| relations.iter())
        .chain(
            description
                .imports()
                .into_iter()
                .flat_map(|relations| relations.iter()),
        )
        .chain(
            description
                .linking_to()
                .into_iter()
                .flat_map(|relations| relations.iter()),
        )
        .map(|relation| relation.name())
        .filter(|package| package != "R")
        .collect()
}

fn locked_dependencies_from_relations_fields(
    depends: Option<&r_description::lossless::Relations>,
    imports: Option<&r_description::lossless::Relations>,
    linking_to: Option<&r_description::lossless::Relations>,
) -> Result<Vec<lockfile::LockedDependency>, String> {
    let mut dependencies = Vec::new();

    dependencies.extend(locked_dependencies_from_relations("Depends", depends)?);
    dependencies.extend(locked_dependencies_from_relations("Imports", imports)?);
    dependencies.extend(locked_dependencies_from_relations("LinkingTo", linking_to)?);

    Ok(dependencies)
}

fn locked_dependencies_from_relations(
    kind: &str,
    relations: Option<&r_description::lossless::Relations>,
) -> Result<Vec<lockfile::LockedDependency>, String> {
    relations
        .into_iter()
        .flat_map(|relations| relations.iter())
        .filter(|relation| relation.name() != "R")
        .map(|relation| {
            let (min_version, max_version_exclusive) = relation_bounds(&relation);

            Ok(lockfile::LockedDependency {
                package: relation.name().to_string(),
                kind: kind.to_string(),
                min_version,
                max_version_exclusive,
            })
        })
        .collect()
}

fn relation_bounds(
    relation: &r_description::lossless::Relation,
) -> (Option<String>, Option<String>) {
    let version = relation.version();

    let Some((operator, version)) = version.as_ref() else {
        return (None, None);
    };

    let version = version.to_string();

    match operator {
        VersionConstraint::Equal => {
            // A lockfile with min/max-exclusive cannot represent exact equality perfectly
            // unless your lockfile semantics define max as the same version or you compute
            // the next version. Keep this aligned with the old lossy behavior.
            (Some(version), None)
        }

        VersionConstraint::GreaterThan => {
            // Same caveat: strict lower bound cannot be represented exactly by min_version.
            // Match existing behavior unless you have a stricter representation.
            (Some(version), None)
        }

        VersionConstraint::GreaterThanEqual => (Some(version), None),

        VersionConstraint::LessThan => (None, Some(version)),

        VersionConstraint::LessThanEqual => {
            // Existing max_version_exclusive cannot precisely represent <=.
            // Match whatever the previous lossy_relation_bounds did.
            (None, Some(version))
        }

        VersionConstraint::NotEqual => {
            // Existing code did not return Result from bounds, so either ignore or
            // change relation_bounds to Result if you want to reject this.
            (None, None)
        }
    }
}

fn package_source_url(name: &str, version: &PackageVersion) -> Result<String, LockError> {
    let repository = version.repository().as_ref();
    let mut url = if let Some(repository) = repository.downcast_ref::<RrepoRepository>() {
        let mut url = repository.url().clone();
        let version = version.version().to_string();
        url.path_segments_mut()
            .expect("repository base URL should support path segments")
            .pop_if_empty()
            .extend(["packages", name, "versions", &version, "source"]);
        return Ok(url.to_string());
    } else if let Some(repository) = repository.downcast_ref::<CranRepository>() {
        repository.url().clone()
    } else {
        return Err(LockError::UnsupportedRepository {
            repository: repository.to_string(),
        });
    };

    let file_name = format!("{name}_{}.tar.gz", version.version());
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", name, &file_name]);
    Ok(url.to_string())
}

fn locked_package_repositories(
    repositories: &[Arc<dyn PackageRepository>],
) -> Result<Vec<LockedRepository>, LockError> {
    repositories
        .iter()
        .map(|repository| {
            let repository = repository.as_ref();
            let (url, kind, cran_archive_support) =
                if let Some(repository) = repository.downcast_ref::<RrepoRepository>() {
                    (repository.url(), LockedRepositoryKind::Rrepo, None)
                } else if let Some(repository) = repository.downcast_ref::<CranRepository>() {
                    (
                        repository.url(),
                        LockedRepositoryKind::CranLike,
                        Some(repository.archive_support()),
                    )
                } else {
                    return Err(LockError::UnsupportedRepository {
                        repository: repository.to_string(),
                    });
                };

            Ok(LockedRepository {
                url: url.to_string(),
                kind,
                cran_archive_support,
            })
        })
        .collect()
}

fn locked_base_packages_from_locked<'a>(
    roots: &BTreeSet<Relation>,
    packages: impl Iterator<Item = &'a LockedPackage>,
) -> Vec<String> {
    let mut base_packages = roots
        .iter()
        .filter_map(|root| {
            let package = root.name();
            is_base_package(&package).then_some(package)
        })
        .collect::<BTreeSet<_>>();

    base_packages.extend(
        packages
            .flat_map(|package| &package.dependencies)
            .filter(|dependency| is_base_package(&dependency.package))
            .map(|dependency| dependency.package.clone()),
    );

    base_packages.into_iter().collect()
}

fn system_plan_from_lockfile(lockfile: &Lockfile) -> Result<SystemDependencyPlan, String> {
    if lockfile.sysreqs.db_commit.is_empty() {
        return Ok(system_plan_without_db(lockfile));
    }

    let snapshot = sysreqs::snapshot_for_commit(&lockfile.sysreqs.db_commit)?;
    Ok(resolve_system_plan(&snapshot, &lockfile.sysreqs.packages))
}

fn system_plan_without_db(lockfile: &Lockfile) -> SystemDependencyPlan {
    SystemDependencyPlan {
        host: current_host_platform(),
        missing_packages: vec![],
        install_packages: vec![],
        pre_install_commands: vec![],
        post_install_commands: vec![],
        unsupported_rules: lockfile.sysreqs.rules.clone(),
        package_rules: lockfile.sysreqs.packages.clone(),
        install_supported: false,
        can_auto_install: false,
        installed_query_error: None,
        needs_metadata_refresh: false,
    }
}

fn host_supports_system_sync() -> bool {
    matches!(current_host_platform(), sysreqs::HostPlatform::Linux { .. })
}

fn sync_system_dependencies(
    lockfile: &Lockfile,
    install_system: bool,
    install_only_system: bool,
) -> Result<(), RpxError> {
    if !host_supports_system_sync() {
        if install_system || install_only_system {
            return Err(SyncError::UnsupportedSystemInstall.into());
        }
        return Ok(());
    }

    let plan = system_plan_from_lockfile(lockfile).unwrap_or_else(|error| {
        warning(RpxWarning::SystemPlanFailed { details: error });
        system_plan_without_db(lockfile)
    });
    handle_system_requirements(&plan, install_system, install_only_system)
}

fn handle_system_requirements(
    plan: &SystemDependencyPlan,
    install_system: bool,
    install_only_system: bool,
) -> Result<(), RpxError> {
    let explicit_install = install_system || install_only_system;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut plan = plan.clone();

    if !plan.unsupported_rules.is_empty() {
        warning(RpxWarning::UnsupportedSystemRequirementRules {
            host: plan.host.label(),
            rules: plan.unsupported_rules.join(", "),
        });
    }

    if plan.needs_metadata_refresh && explicit_install {
        if interactive {
            prompt_for_metadata_refresh(&plan);
        }

        note("Refreshing system package information...");
        refresh_system_metadata(&plan)
            .map_err(|details| SyncError::MetadataRefreshFailed { details })?;
        match recheck_system_missing_packages(&plan) {
            Ok(missing_packages) => {
                plan.missing_packages = missing_packages;
                plan.installed_query_error = None;
                plan.needs_metadata_refresh = false;
            }
            Err(error) => {
                plan.installed_query_error = Some(error);
                plan.needs_metadata_refresh = false;
            }
        }
    }

    if plan.missing_packages.is_empty() {
        if install_only_system {
            status("System dependencies are already installed");
        }
        return Ok(());
    }

    if plan.installed_query_error.is_none() {
        print_system_package_summary(
            &format!("Missing system packages for {}:", plan.host.label()),
            &plan.missing_packages,
        );
    }
    let preview = sysreq_preview_commands(&plan);
    if !preview.is_empty() {
        note("rpx will run:");
        for command in &preview {
            note(format_args!("- {command}"));
        }
    }

    if explicit_install && interactive && !prompt_for_install_confirmation() {
        status("Canceled");
        std::process::exit(1);
    }

    if explicit_install {
        let ui = SystemDepsUi::start();
        if let Err(error) = install_system_dependencies(&plan) {
            ui.fail();
            return Err(SyncError::SystemDependenciesFailed { details: error }.into());
        }
        ui.finish();
        if install_only_system {
            status("System dependency sync complete.");
        }
        return Ok(());
    }

    if !interactive {
        warning(RpxWarning::ContinuingWithoutSystemDependencies);
        return Ok(());
    }

    match prompt_for_system_dependency_action() {
        SyncSystemChoice::InstallAndContinue => {
            let ui = SystemDepsUi::start();
            if let Err(error) = install_system_dependencies(&plan) {
                ui.fail();
                return Err(SyncError::SystemDependenciesFailed { details: error }.into());
            }
            ui.finish();
            Ok(())
        }
        SyncSystemChoice::TryROnly => Ok(()),
        SyncSystemChoice::Cancel => {
            status("Canceled");
            std::process::exit(1);
        }
    }
}

fn prompt_for_install_confirmation() -> bool {
    note("Proceed with system package installation? [y/N]");
    prompt("> ");

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes")
}

fn print_system_package_summary(title: &str, packages: &[String]) {
    note(title);
    let shown = packages.iter().take(8).collect::<Vec<_>>();
    for package in shown {
        note(format_args!("- {package}"));
    }
    if packages.len() > 8 {
        note(format_args!("- ... and {} more", packages.len() - 8));
    }
}

fn prompt_for_metadata_refresh(plan: &SystemDependencyPlan) {
    note("rpx could not verify which system packages are missing yet.");
    blank_note_line();
    note("rpx can run:");
    if let Some(command) = system_metadata_refresh_preview(plan) {
        note(format_args!("- {command}"));
    }
    note("to refresh apt package information and check what is missing.");
    blank_note_line();
    note("Run package metadata refresh now? [y/N]");
    prompt("> ");

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        status("Canceled");
        std::process::exit(1);
    }

    if !matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        status("Canceled");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncSystemChoice {
    InstallAndContinue,
    TryROnly,
    Cancel,
}

fn prompt_for_system_dependency_action() -> SyncSystemChoice {
    note("Choose an action:");
    note("1. Install system deps and continue");
    note("2. Try to install R packages only");
    note("3. Cancel");
    prompt("> ");

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return SyncSystemChoice::TryROnly;
    }

    match input.trim() {
        "1" | "y" | "Y" => SyncSystemChoice::InstallAndContinue,
        "2" | "r" | "R" => SyncSystemChoice::TryROnly,
        _ => SyncSystemChoice::Cancel,
    }
}

fn r_minor_version(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    Some(format!("{}.{}", parts.next()?, parts.next()?))
}

async fn install_required_packages(
    packages: RequiredPackages,
    retained: BTreeSet<String>,
    r_version: &str,
) -> Result<(), SyncError> {
    let total_packages = packages.len() as u64;
    let sync_span = tracing::info_span!(
        "sync_packages",
        total = total_packages,
        completed = 0_u64,
        running = 0_u64,
        pending = total_packages,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    sync_span.pb_set_style(&progress_spinner_style());
    sync_span.pb_set_message(&format!("sync packages 0/{total_packages}"));
    sync_span.pb_set_length(total_packages);
    sync_span.pb_start();

    required_package_install_order(&packages)
        .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

    if packages.is_empty() {
        sync_span.record("stage", "done");
        sync_span.pb_set_finish_message("sync packages 0/0");
        return Ok(());
    }

    let r_version = Arc::new(r_version.to_string());
    let r_minor = Arc::new(r_minor_version(r_version.as_str()).ok_or_else(|| {
        SyncError::DownloadArtifactsFailed {
            details: format!("failed to parse R minor version from {r_version}"),
        }
    })?);
    let required_names = Arc::new(
        retained
            .iter()
            .cloned()
            .chain(packages.keys().cloned())
            .collect::<BTreeSet<_>>(),
    );
    let installed_packages = Arc::new(Mutex::new(retained));
    let shared_pool = Arc::new(Semaphore::new(SYNC_SHARED_WORKERS));
    let install_pool = Arc::new(Semaphore::new(SYNC_INSTALL_WORKERS));
    let (installed_tx, installed_rx) = watch::channel(());
    let mut prepare_tasks = tokio::task::JoinSet::new();
    let mut install_tasks = tokio::task::JoinSet::new();
    let mut completed = 0_u64;

    for (package_name, (package_version, description)) in packages {
        let dependencies = package_dependency_names(&description);
        if let Some(repository) = package_version
            .repository()
            .as_ref()
            .downcast_ref::<LocalRepository>()
        {
            let project_path = repository.path().to_path_buf();
            let install_required_names = Arc::clone(&required_names);
            let install_installed_packages = Arc::clone(&installed_packages);
            let install_installed_rx = installed_rx.clone();
            let install_installed_tx = installed_tx.clone();
            let install_shared_pool = Arc::clone(&shared_pool);
            let install_pool = Arc::clone(&install_pool);
            install_tasks.spawn(
                async move {
                    wait_for_package_dependencies(
                        &package_name,
                        &dependencies,
                        install_required_names,
                        Arc::clone(&install_installed_packages),
                        install_installed_rx,
                    )
                    .await
                    .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                    let _install_permit = install_pool.acquire_owned().await.map_err(|_| {
                        SyncError::DownloadArtifactsFailed {
                            details: "install pool closed before project installation".to_string(),
                        }
                    })?;
                    let _shared_permit =
                        install_shared_pool.acquire_owned().await.map_err(|_| {
                            SyncError::DownloadArtifactsFailed {
                                details: "sync work pool closed before project installation"
                                    .to_string(),
                            }
                        })?;

                    install_project_package(&project_path, &project_library_path())
                        .await
                        .map_err(|source| SyncError::ProjectPackageInstall { source })?;
                    {
                        let mut installed_packages = install_installed_packages.lock().await;
                        installed_packages.insert(package_name.clone());
                    }
                    let _ = install_installed_tx.send(());

                    Ok::<_, SyncError>(package_name)
                }
                .instrument(sync_span.clone()),
            );
            continue;
        }

        let cache_key = CompiledPackageCacheKey::new(
            &package_name,
            &package_version.version().to_string(),
            r_version.as_str(),
        );
        let (prepared_tx, prepared_rx) = oneshot::channel();

        let prepare_package_name = package_name.clone();
        let prepare_package_version = package_version.clone();
        let prepare_cache_key = cache_key.clone();
        let prepare_r_minor = Arc::clone(&r_minor);
        let prepare_shared_pool = Arc::clone(&shared_pool);
        prepare_tasks.spawn(
            async move {
                let prepared = match prepare_shared_pool.acquire_owned().await {
                    Ok(_permit) => {
                        prepare_locked_package_artifact(
                            prepare_package_name,
                            prepare_package_version,
                            prepare_cache_key,
                            prepare_r_minor,
                        )
                        .await
                    }
                    Err(_) => Err("sync work pool closed before artifact preparation".to_string()),
                };

                let _ = prepared_tx.send(prepared);
            }
            .instrument(sync_span.clone()),
        );

        let install_required_names = Arc::clone(&required_names);
        let install_installed_packages = Arc::clone(&installed_packages);
        let install_installed_rx = installed_rx.clone();
        let install_installed_tx = installed_tx.clone();
        let install_shared_pool = Arc::clone(&shared_pool);
        let install_pool = Arc::clone(&install_pool);
        install_tasks.spawn(
            async move {
                let prepared_artifact = prepared_rx
                    .await
                    .map_err(|_| SyncError::DownloadArtifactsFailed {
                        details: format!(
                            "{package_name} artifact preparation task ended without a result"
                        ),
                    })?
                    .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                // Keep package spans out of the progress UI while blocked on dependency installs.
                wait_for_package_dependencies(
                    &package_name,
                    &dependencies,
                    install_required_names,
                    Arc::clone(&install_installed_packages),
                    install_installed_rx,
                )
                .await
                .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                let _install_permit = install_pool.acquire_owned().await.map_err(|_| {
                    SyncError::DownloadArtifactsFailed {
                        details: "install pool closed before package installation".to_string(),
                    }
                })?;
                let _shared_permit = install_shared_pool.acquire_owned().await.map_err(|_| {
                    SyncError::DownloadArtifactsFailed {
                        details: "sync work pool closed before package installation".to_string(),
                    }
                })?;

                let installed = install_prepared_package(
                    package_name,
                    package_version,
                    cache_key,
                    prepared_artifact,
                )
                .await
                .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;
                {
                    let mut installed_packages = install_installed_packages.lock().await;
                    installed_packages.insert(installed.clone());
                }
                let _ = install_installed_tx.send(());

                Ok::<_, SyncError>(installed)
            }
            .instrument(sync_span.clone()),
        );
    }

    sync_span.record("running", install_tasks.len() as u64);

    while let Some(result) = install_tasks.join_next().await {
        result.map_err(|error| SyncError::DownloadArtifactsFailed {
            details: format!("install task failed to join: {error}"),
        })??;
        completed += 1;
        sync_span.record("completed", completed);
        sync_span.record("running", install_tasks.len() as u64);
        sync_span.record("pending", total_packages.saturating_sub(completed));
        sync_span.pb_set_position(completed);
        sync_span.pb_set_message(&format!("sync packages {completed}/{total_packages}"));
    }

    drop(prepare_tasks);

    sync_span.record("stage", "done");
    sync_span.pb_set_finish_message(&format!("sync packages {completed}/{total_packages}"));
    Ok(())
}

async fn wait_for_package_dependencies(
    package: &str,
    dependencies: &BTreeSet<String>,
    required_names: Arc<BTreeSet<String>>,
    installed_packages: Arc<Mutex<BTreeSet<String>>>,
    mut installed_rx: watch::Receiver<()>,
) -> Result<(), String> {
    loop {
        {
            let installed_packages = installed_packages.lock().await;
            if dependencies
                .iter()
                .filter(|dependency| required_names.contains(*dependency))
                .all(|dependency| installed_packages.contains(dependency))
            {
                return Ok(());
            }
        }

        installed_rx.changed().await.map_err(|_| {
            format!(
                "dependency notifier closed before {} dependencies were installed",
                package
            )
        })?;
    }
}

async fn prepare_locked_package_artifact(
    package: String,
    package_version: PackageVersion,
    cache_key: CompiledPackageCacheKey,
    r_minor: Arc<String>,
) -> Result<Option<(PathBuf, String)>, String> {
    let version = package_version.version().to_string();
    let span = tracing::info_span!(
        "prepare_package",
        package = %package,
        version = %version,
        repository = tracing::field::Empty,
        stage = tracing::field::Empty,
        artifact_kind = tracing::field::Empty,
        bytes = tracing::field::Empty,
        total_bytes = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&package_stage_message(&package, &version, "preparing"));
    span.pb_start();

    prepare_locked_package_artifact_inner(
        package,
        package_version,
        &cache_key,
        r_minor.as_str(),
        span.clone(),
    )
    .instrument(span)
    .await
}

async fn prepare_locked_package_artifact_inner(
    package: String,
    package_version: PackageVersion,
    cache_key: &CompiledPackageCacheKey,
    r_minor: &str,
    span: tracing::Span,
) -> Result<Option<(PathBuf, String)>, String> {
    fn response_for_status(response: reqwest::Response) -> Result<reqwest::Response, String> {
        response
            .error_for_status()
            .map_err(|error| error.to_string())
    }

    let version = package_version.version().to_string();
    record_package_stage(&span, &package, &version, "checking cache");
    if cache::exists(cache_key).await {
        record_package_stage(&span, &package, &version, "cached");
        return Ok(None);
    }

    let repository = package_version.repository();
    let (base_url, is_rrepo) =
        if let Some(repository) = repository.as_ref().downcast_ref::<RrepoRepository>() {
            (repository.url(), true)
        } else if let Some(repository) = repository.as_ref().downcast_ref::<CranRepository>() {
            (repository.url(), false)
        } else {
            return Err(format!(
                "package {package} uses an unsupported remote repository"
            ));
        };
    span.record("repository", base_url.as_str());

    record_package_stage(&span, &package, &version, "downloading binary");

    let binary = match (std::env::consts::OS, is_rrepo) {
        ("windows", true) => http::rrepo_windows_binary(base_url, &package, &version, r_minor)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("windows", false) => http::cran_windows_binary(base_url, r_minor, &package, &version)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("macos", true) => {
            let target = macos_binary_target()?;

            http::rrepo_macos_binary(base_url, &package, &version, &target, r_minor)
                .await
                .map_err(|error| error.to_string())
                .and_then(response_for_status)
                .map(|response| (response, "tgz", format!("mac.binary.{target}")))
        }

        ("macos", false) => {
            let target = macos_binary_target()?;

            http::cran_macos_binary(base_url, &target, r_minor, &package, &version)
                .await
                .map_err(|error| error.to_string())
                .and_then(response_for_status)
                .map(|response| (response, "tgz", format!("mac.binary.{target}")))
        }

        _ => Err(format!(
            "binary artifacts are not supported on {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    };

    let (response, extension, install_type) = match binary {
        Ok(binary) => {
            span.record("artifact_kind", binary.2.as_str());
            binary
        }

        Err(error) => {
            tracing::debug!(
                package = %package,
                version = %version,
                error = %error,
                "binary artifact unavailable; falling back to source"
            );

            record_package_stage(&span, &package, &version, "falling back to source");
            record_package_stage(&span, &package, &version, "downloading source");

            let response = if is_rrepo {
                http::rrepo_source_artifact(base_url, &package, &version)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(response_for_status)?
            } else {
                let current = http::cran_current_source_tarball(base_url, &package, &version)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(response_for_status);
                match current {
                    Ok(response) => response,
                    Err(_) => http::cran_archive_source_tarball(base_url, &package, &version)
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(response_for_status)?,
                }
            };

            span.record("artifact_kind", "source");

            (response, "tar.gz", "source".to_string())
        }
    };

    let artifact_path =
        write_artifact_response(&package, &version, extension, response, &span).await?;

    record_package_stage(&span, &package, &version, "prepared");

    Ok(Some((artifact_path, install_type)))
}

async fn install_prepared_package(
    package: String,
    package_version: PackageVersion,
    cache_key: CompiledPackageCacheKey,
    prepared_artifact: Option<(PathBuf, String)>,
) -> Result<String, String> {
    let version = package_version.version().to_string();
    let span = tracing::info_span!(
        "install_package",
        package = %package,
        version = %version,
        stage = tracing::field::Empty,
        artifact_kind = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&package_stage_message(&package, &version, "installing"));
    span.pb_start();

    install_prepared_package_inner(package, version, cache_key, prepared_artifact, span.clone())
        .instrument(span)
        .await
}

async fn install_prepared_package_inner(
    package: String,
    version: String,
    cache_key: CompiledPackageCacheKey,
    prepared_artifact: Option<(PathBuf, String)>,
    span: tracing::Span,
) -> Result<String, String> {
    let project_library = project_library_path();

    match prepared_artifact {
        None => {
            span.record("artifact_kind", "compiled-cache");
            record_package_stage(&span, &package, &version, "restoring from cache");
            cache::restore(&cache_key, &project_library).await?;
            record_package_stage(&span, &package, &version, "restored from cache");
            Ok(package)
        }

        Some((artifact_path, install_type)) => {
            span.record("artifact_kind", install_type.as_str());
            install_downloaded_package(
                package,
                version,
                cache_key,
                artifact_path,
                install_type,
                project_library,
                span,
            )
            .await
        }
    }
}

async fn install_downloaded_package(
    package: String,
    version: String,
    key: CompiledPackageCacheKey,
    artifact_path: PathBuf,
    install_type: String,
    project_library: PathBuf,
    span: tracing::Span,
) -> Result<String, String> {
    record_package_stage(&span, &package, &version, "installing");

    let temp_library = build_temp_library_path(&package, &unique_build_token());

    install_local_package(
        &artifact_path,
        &package,
        &version,
        &install_type,
        &temp_library,
    )
    .await
    .map_err(|failure| failure.to_string())?;

    let built_package_path = temp_library.join(&package);

    record_package_stage(&span, &package, &version, "storing cache");
    cache::store(&key, &built_package_path).await?;

    record_package_stage(&span, &package, &version, "restoring project library");
    cache::restore(&key, &project_library).await?;

    record_package_stage(&span, &package, &version, "cleaning up");
    if let Some(temp_root) = temp_library.parent() {
        tokio::fs::remove_dir_all(temp_root)
            .await
            .map_err(|error| format!("failed to clean temporary build directory: {error}"))?;
    }

    record_package_stage(&span, &package, &version, "done");

    Ok(package)
}

fn record_package_stage(span: &tracing::Span, package: &str, version: &str, stage: &'static str) {
    span.record("stage", stage);
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&package_stage_message(package, version, stage));
    span.pb_tick();
}

fn package_stage_message(package: &str, version: &str, stage: &str) -> String {
    format!("{package} {version} {stage}")
}

async fn write_artifact_response(
    package: &str,
    version: &str,
    extension: &str,
    response: reqwest::Response,
    span: &tracing::Span,
) -> Result<PathBuf, String> {
    let file_name = format!("{package}_{version}.{extension}");
    let path = artifact_cache_path(package, version, &file_name);

    if path.exists() {
        if let Ok(metadata) = path.metadata() {
            span.record("bytes", metadata.len());
            span.record("total_bytes", metadata.len());
            span.pb_set_style(&progress_bar_style());
            span.pb_set_length(metadata.len());
            span.pb_set_position(metadata.len());
            span.pb_set_message(&package_stage_message(
                package,
                version,
                "using cached artifact",
            ));
        }

        return Ok(path);
    }

    let content_length = response.content_length();

    if let Some(total) = content_length {
        span.record("total_bytes", total);
        span.pb_set_style(&progress_bar_style());
        span.pb_set_length(total);
        span.pb_set_position(0);
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            format!(
                "failed to create artifact cache directory {}: {error}",
                parent.display()
            )
        })?;
    }

    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| format!("failed to create artifact file {}: {error}", path.display()))?;

    let mut stream = response.bytes_stream();
    let mut written = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("failed to read artifact response: {error}"))?;
        let chunk_len = chunk.len() as u64;

        file.write_all(&chunk).await.map_err(|error| {
            format!("failed to write artifact file {}: {error}", path.display())
        })?;

        written += chunk_len;

        span.record("bytes", written);

        if content_length.is_some() {
            span.pb_inc(chunk_len);
        } else {
            span.pb_tick();
        }
    }

    file.flush()
        .await
        .map_err(|error| format!("failed to flush artifact file {}: {error}", path.display()))?;

    Ok(path)
}

fn macos_binary_target() -> Result<String, String> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("big-sur-arm64".to_string()),
        "x86_64" => Ok("big-sur-x86_64".to_string()),
        arch => Err(format!(
            "unsupported macOS architecture for binary packages: {arch}"
        )),
    }
}

fn unique_build_token() -> String {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("{}-{unique}", std::process::id())
}

fn required_package_install_order(packages: &RequiredPackages) -> Result<Vec<String>, String> {
    let required_names = packages.keys().cloned().collect::<BTreeSet<_>>();
    let mut indegree = required_names
        .iter()
        .map(|name| (name.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut dependents = required_names
        .iter()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();

    for (package, (_, description)) in packages {
        let internal_dependencies = package_dependency_names(description)
            .iter()
            .filter(|dependency| required_names.contains(*dependency))
            .cloned()
            .collect::<BTreeSet<_>>();

        *indegree
            .get_mut(package)
            .expect("required package should have indegree") += internal_dependencies.len();

        for dependency in internal_dependencies {
            dependents
                .get_mut(&dependency)
                .expect("required dependency should exist")
                .insert(package.clone());
        }
    }

    let mut ready = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(packages.len());

    while let Some(name) = ready.pop_first() {
        ordered.push(name.clone());

        for dependent in dependents.get(&name).cloned().unwrap_or_default() {
            let count = indegree
                .get_mut(&dependent)
                .expect("dependent should have indegree entry");
            *count -= 1;
            if *count == 0 {
                ready.insert(dependent);
            }
        }
    }

    if ordered.len() != packages.len() {
        let unresolved = indegree
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        return Err(format!(
            "cyclic or unresolved package dependencies: {}",
            unresolved.join(", ")
        ));
    }

    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::{
        DefaultRepositoryPreference, LockError, RequiredPackages,
        apply_added_packages_to_description, lock_error_from_resolution,
        locked_dependencies_from_description, locked_package_repositories, package_not_found_help,
        parse_add_package, pinned_package_relations, remove_packages_from_description_dependencies,
        required_package_install_order, roots_from_description,
    };
    use crate::lockfile::{
        LOCKFILE_REVISION, LOCKFILE_VERSION, LockedR, LockedRepository, LockedRepositoryKind,
        LockedSystemRequirements, Lockfile,
    };
    use crate::repository::{LocalRepository, PackageRepository, built_in_repository};
    use crate::resolver::{PackageVersion, RDependencyProvider, ResolutionError};
    use pubgrub::{DerivationTree, External, PubGrubError, Ranges};
    use r_description::{
        VersionConstraint,
        lossless::{RDescription, Relation},
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
        sync::Arc,
    };

    fn required_packages(packages: &[(&str, &[&str])]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, dependencies)| {
                let imports = (!dependencies.is_empty())
                    .then(|| format!("Imports: {}\n", dependencies.join(", ")))
                    .unwrap_or_default();
                let description = format!("Package: {name}\nVersion: 1.0.0\n{imports}")
                    .parse::<RDescription>()
                    .expect("DESCRIPTION should parse");
                let version = PackageVersion::new(
                    "1.0.0".parse().expect("version should parse"),
                    built_in_repository(),
                );

                ((*name).to_string(), (version, Arc::new(description)))
            })
            .collect()
    }

    #[test]
    fn rejects_local_repository_locking() {
        let repositories: Vec<Arc<dyn PackageRepository>> = vec![Arc::new(LocalRepository::new(
            PathBuf::from("vendor/example"),
        ))];

        let error = locked_package_repositories(&repositories)
            .expect_err("local repositories should not be lockable");

        assert!(matches!(error, LockError::UnsupportedRepository { .. }));
    }

    #[test]
    fn resolves_default_repository_preference_from_flags_and_lock() {
        let description =
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://extra.test/cran\n"
                .parse::<RDescription>()
                .expect("DESCRIPTION should parse");
        let lockfile = |repositories: &[&str]| Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            repositories: repositories
                .iter()
                .map(|url| LockedRepository {
                    url: (*url).to_string(),
                    kind: LockedRepositoryKind::Rrepo,
                    cran_archive_support: None,
                })
                .collect(),
            r: LockedR::default(),
            sysreqs: LockedSystemRequirements::default(),
            roots: vec![],
            packages: BTreeMap::new(),
        };
        let enabled = lockfile(&[
            "https://custom-default.test/cran",
            "https://extra.test/cran",
        ]);
        let disabled = lockfile(&["https://extra.test/cran"]);

        assert!(DefaultRepositoryPreference::Enabled.enabled(&description, Some(&disabled)));
        assert!(!DefaultRepositoryPreference::Disabled.enabled(&description, Some(&enabled)));
        assert!(
            DefaultRepositoryPreference::FromLockfileOrDefault
                .enabled(&description, Some(&enabled))
        );
        assert!(
            !DefaultRepositoryPreference::FromLockfileOrDefault
                .enabled(&description, Some(&disabled))
        );
        assert!(DefaultRepositoryPreference::FromLockfileOrDefault.enabled(&description, None));
    }

    #[test]
    fn renders_pubgrub_no_solution_report() {
        let error: PubGrubError<RDependencyProvider> =
            PubGrubError::NoSolution(DerivationTree::External(External::NoVersions(
                "testthat".to_string(),
                Ranges::empty(),
            )));

        let error = lock_error_from_resolution(ResolutionError::PubGrub(error));
        let LockError::NoSolution { explanation } = &error else {
            panic!("no-solution error should have a dedicated diagnostic");
        };

        assert!(explanation.contains("testthat"));
        let rendered = format!("{:?}", miette::Report::new(error));
        assert!(rendered.contains("package requirements are incompatible"));
        assert!(rendered.contains("testthat"));
        assert!(!rendered.contains("There is no solution"));
    }

    #[test]
    fn install_graph_includes_project_dependencies_and_dependents() {
        let packages = required_packages(&[
            ("hard", &["leaf"]),
            ("leaf", &[]),
            ("project", &["hard"]),
            ("reverse", &["project"]),
            ("unrelated", &[]),
        ]);

        let order = required_package_install_order(&packages).unwrap();
        let position = |name: &str| order.iter().position(|package| package == name).unwrap();

        assert!(position("leaf") < position("hard"));
        assert!(position("hard") < position("project"));
        assert!(position("project") < position("reverse"));
        assert!(order.contains(&"unrelated".to_string()));
    }

    #[test]
    fn builds_root_relations_from_description_constraints() {
        let description: RDescription = "Package: testpkg\nVersion: 0.1.0\nTitle: Test Package\nDescription: Test package for unit tests.\nLicense: MIT\nImports: cli (>= 3.6.0), digest\nDepends: R (>= 4.2), jsonlite (== 1.8.9)\nLinkingTo: cpp11\nSuggests: testthat (>= 3.0.0)\nEnhances: shiny\n"
            .parse()
            .expect("description should parse");

        assert_eq!(
            roots_from_description(&description)
                .into_iter()
                .map(|relation| relation.to_string())
                .collect::<Vec<_>>(),
            vec![
                "cli (>= 3.6.0)".to_string(),
                "cpp11".to_string(),
                "digest".to_string(),
                "jsonlite (== 1.8.9)".to_string(),
                "testthat (>= 3.0.0)".to_string(),
            ]
        );
    }

    #[test]
    fn locks_only_hard_dependencies_of_selected_packages() {
        let description: RDescription = "Package: suggestedpkg\nVersion: 0.1.0\nTitle: Suggested Package\nDescription: Test package for unit tests.\nLicense: MIT\nDepends: hardDepends\nImports: hardImports\nLinkingTo: hardLinking\nSuggests: nestedSuggestion\nEnhances: enhancedPackage\n"
            .parse()
            .expect("description should parse");

        assert_eq!(
            locked_dependencies_from_description(&description)
                .expect("dependencies should serialize")
                .into_iter()
                .map(|dependency| (dependency.package, dependency.kind))
                .collect::<Vec<_>>(),
            vec![
                ("hardDepends".to_string(), "Depends".to_string()),
                ("hardImports".to_string(), "Imports".to_string()),
                ("hardLinking".to_string(), "LinkingTo".to_string()),
            ]
        );
    }

    #[test]
    fn builds_pinned_package_relations_from_latest_version() {
        let latest = "1.1.4".parse().unwrap();

        assert_eq!(
            pinned_package_relations("digest", &latest)
                .unwrap()
                .into_iter()
                .map(|relation| relation.to_string())
                .collect::<Vec<_>>(),
            vec![
                "digest (>= 1.1.4)".to_string(),
                "digest (< 2.0.0)".to_string(),
            ]
        );
    }

    #[test]
    fn parses_explicit_add_constraint() {
        let package = parse_add_package("dplyr@>=1.0.0").expect("constraint should parse");

        assert_eq!(package.name, "dplyr");
        assert_eq!(
            package
                .relation
                .expect("relation should be present")
                .to_string(),
            "dplyr (>= 1.0.0)"
        );
        assert_eq!(
            parse_add_package("dplyr@!=1.0.0")
                .expect("constraint should parse")
                .relation
                .expect("relation should be present")
                .to_string(),
            "dplyr (!= 1.0.0)"
        );
    }

    #[test]
    fn rejects_invalid_explicit_add_constraint() {
        assert!(parse_add_package("dplyr@=1.0.0").is_err());
        assert!(parse_add_package("dplyr@>=").is_err());
        assert!(parse_add_package("dplyr@>= 1.0.0").is_err());
    }

    #[test]
    fn constrained_add_replaces_existing_dependency_relations() {
        let mut description: RDescription = "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for unit tests.
License: MIT
Depends: dplyr (>= 1.0.0), keepDepends
Imports: dplyr (< 2.0.0), keepImports
LinkingTo: dplyr, keepLinking
Suggests: dplyr, keepSuggests
Enhances: dplyr, keepEnhances
"
        .parse()
        .expect("description should parse");
        let relation = Relation::new(
            "dplyr",
            Some((VersionConstraint::Equal, "1.0.0".parse().unwrap())),
        );

        apply_added_packages_to_description(&mut description, &BTreeSet::from([relation]))
            .expect("description should update");

        assert_eq!(description.depends().unwrap().to_string(), "keepDepends");
        assert_eq!(
            description.imports().unwrap().to_string(),
            "keepImports,\ndplyr (== 1.0.0)"
        );
        assert_eq!(description.linking_to().unwrap().to_string(), "keepLinking");
        assert_eq!(description.suggests().unwrap().to_string(), "keepSuggests");
        assert_eq!(description.enhances().unwrap().to_string(), "keepEnhances");
    }

    #[test]
    fn removes_packages_from_all_description_dependency_fields() {
        let mut description: RDescription = "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for unit tests.
License: MIT
Depends: R (>= 4.2), removeMe (>= 1.0), keepDepends
Imports: removeMe, keepImports
LinkingTo: removeMe, keepLinking
Suggests: removeMe, keepSuggests
Enhances: removeMe, keepEnhances
"
        .parse()
        .expect("description should parse");
        let packages = BTreeSet::from(["removeMe".to_string()]);

        remove_packages_from_description_dependencies(&mut description, &packages);

        assert_eq!(
            description
                .depends()
                .unwrap()
                .iter()
                .map(|relation| relation.name())
                .collect::<Vec<_>>(),
            vec!["R".to_string(), "keepDepends".to_string()]
        );
        assert_eq!(description.imports().unwrap().to_string(), "keepImports");
        assert_eq!(description.linking_to().unwrap().to_string(), "keepLinking");
        assert_eq!(description.suggests().unwrap().to_string(), "keepSuggests");
        assert_eq!(description.enhances().unwrap().to_string(), "keepEnhances");
    }

    #[test]
    fn suggests_similar_package_names_for_missing_adds() {
        let known = ["dplyr", "digest", "ggplot2", "jsonlite"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();

        assert_eq!(
            package_not_found_help(&["dyplr".to_string(), "ggplot".to_string()], &known),
            "For dyplr, did you mean dplyr? For ggplot, did you mean ggplot2?"
        );
    }

    #[test]
    fn installs_required_packages_in_dependency_order() {
        let packages = required_packages(&[
            ("AzureKeyVault", &["AzureRMR"]),
            ("AzureRMR", &["httr2"]),
            ("httr2", &[]),
        ]);

        assert_eq!(
            required_package_install_order(&packages).unwrap(),
            vec![
                "httr2".to_string(),
                "AzureRMR".to_string(),
                "AzureKeyVault".to_string()
            ]
        );
    }

    #[test]
    fn rejects_cyclic_required_dependencies() {
        let packages = required_packages(&[("a", &["b"]), ("b", &["a"])]);

        let error = required_package_install_order(&packages).expect_err("cycle should fail");
        assert!(error.contains("cyclic or unresolved package dependencies"));
    }
}

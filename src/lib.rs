use clap::Parser;
use futures_util::StreamExt;
use miette::Diagnostic;
use pubgrub::{DefaultStringReporter, PubGrubError, Reporter};
use r_description::{RDescription, Relation, Version, VersionRequirement};
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
mod commands;
mod description;
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

use cli::{Cli, Commands};
use commands::repo;
use output::{blank_note_line, note, prompt, status, warning};
use project::{
    LockedResolutionError, LockedResolutionFailure, RequiredPackages, artifact_cache_path,
    build_temp_library_path, cache_dir_path, project_library_path, project_library_root_path,
};
use r::{base_packages, install_local_package, install_package_directory, installed_packages};
use resolver::{ResolutionError, resolve_from_registry};
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
    description::{
        DescriptionParseError, DescriptionReadError, DescriptionWriteError,
        InitialDescriptionError, NamespaceWriteError, PackageNameDerivationError,
        RepositoriesFromDescriptionError, add_dependencies, derive_package_name,
        initial_description, project_dependencies, read_description, remove_dependencies,
        repositories_from_description, root_package, write_description, write_namespace_if_missing,
    },
    lockfile::{Lockfile, LockfileReadError, LockfileWriteError, read_lockfile, write_lockfile},
    project::{
        LockedPackagesError, ProjectDiscoveryError, find_project_root,
        required_packages_from_lockfile, validate_locked_resolution,
    },
    r::{
        BasePackagesError, RVersionError, RVirtualEnv, r_version_async, remove_packages_from_venv,
    },
    repository::{
        CranRepository, GitRepository, LocalRepository, PackageRepository, RepositoryError,
        RrepoRepository,
    },
    resolver::PackageVersion,
};

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_INSTALL_WORKERS: usize = 8;

#[derive(Debug)]
struct PackageVersionMismatch {
    package: String,
    installed: Version,
    expected: Version,
}

fn lock_error_from_resolution(error: ResolutionError) -> LockError {
    if let ResolutionError::Provider(provider) = &error {
        let source = match provider {
            resolver::ProviderError::Repository(source)
            | resolver::ProviderError::DependencyMetadata { source, .. } => source,
        };
        if let Some(repository) = inaccessible_git_repository(source) {
            return LockError::GitRepositoryUnavailable {
                repository: repository.to_string(),
            };
        }
    }

    match error {
        ResolutionError::PubGrub(PubGrubError::NoSolution(mut derivation_tree)) => {
            derivation_tree.collapse_no_versions();
            LockError::NoSolution {
                explanation: DefaultStringReporter::report(&derivation_tree),
            }
        }
        ResolutionError::BasePackages(source) => LockError::BasePackages(source),
        source => LockError::Resolution { source },
    }
}

fn lock_error_from_repository(source: RepositoryError) -> LockError {
    if let Some(repository) = inaccessible_git_repository(&source) {
        LockError::GitRepositoryUnavailable {
            repository: repository.to_string(),
        }
    } else {
        LockError::Repository { source }
    }
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
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init()?,
        Commands::Add { packages } => cmd_add(&packages).await?,
        Commands::Remove { packages } => cmd_remove(&packages).await?,
        Commands::Run { command } => cmd_run(&command).await?,
        Commands::Lock {} => cmd_lock().await?,
        Commands::Status => cmd_status().await?,
        Commands::Sync {
            install_system,
            install_only_system,
        } => cmd_sync(install_system, install_only_system).await?,
        Commands::Clean => cmd_clean()?,
        Commands::Repo { command } => repo::run(command).await?,
    }

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

#[derive(Debug, Error, Diagnostic)]
pub enum InitError {
    #[error("failed to determine the current working directory: {source}")]
    #[diagnostic(code(rpx::init::working_directory_unavailable))]
    WorkingDirectoryUnavailable {
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageName(#[from] PackageNameDerivationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InitialDescription(#[from] InitialDescriptionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    WriteDescription(#[from] DescriptionWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    WriteNamespace(#[from] NamespaceWriteError),
}

fn cmd_init() -> Result<(), InitError> {
    let current_dir =
        env::current_dir().map_err(|source| InitError::WorkingDirectoryUnavailable { source })?;

    let package_name = derive_package_name(&current_dir)?;
    let description = initial_description(&package_name)?;

    write_description(&current_dir, &description)?;
    write_namespace_if_missing(&current_dir)?;

    status(format_args!(
        "Initialized project at {}",
        current_dir.display()
    ));
    status("Next: run `rpx add <package>` or `rpx lock`");
    Ok(())
}

#[derive(Debug, Error, Diagnostic)]
enum AddError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionWrite(#[from] DescriptionWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileWrite(#[from] LockfileWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageParse(#[from] AddPackageParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repositories(#[from] RepositoriesFromDescriptionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),

    #[error("failed to load resolved package metadata: {source}")]
    #[diagnostic(code(rpx::add::package_metadata_failed))]
    PackageMetadata {
        #[from]
        source: RepositoryError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] r::InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

async fn cmd_add(packages: &[String]) -> Result<(), AddError> {
    let current_dir = find_project_root()?;
    let added_relations = packages
        .iter()
        .map(|package| parse_add_package(package))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let unconstrained_packages = added_relations
        .iter()
        .filter(|relation| matches!(relation.requirement(), VersionRequirement::Any))
        .map(|relation| relation.package().to_string())
        .collect::<BTreeSet<_>>();

    let mut description = read_description(&current_dir)?;
    let old_lockfile = match read_lockfile(&current_dir) {
        Ok(lockfile) => Some(lockfile),
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(LockfileReadError::OutdatedLockfile { .. }) => None,
        Err(source) => return Err(source.into()),
    };
    let r_version = r_version_async().await?;

    add_dependencies(&current_dir, &mut description, &added_relations)?;

    let desired_roots = project_dependencies(&current_dir, &description)?;
    let (root_name, root_version) = root_package(&current_dir, &description)?;
    let root =
        Arc::new(LocalRepository::new(current_dir.clone()).with_description(description.clone()));

    let (mut lockfile, mut resolved) = match old_lockfile.as_ref().map(|lockfile| {
        (
            lockfile,
            validate_locked_resolution(&current_dir, &description, &r_version, lockfile),
        )
    }) {
        Some((lockfile, Ok(()))) => (lockfile.clone(), required_packages_from_lockfile(lockfile)?),
        Some((lockfile, Err(LockedResolutionError::Validation { failures }))) => {
            let repositories = if failures.iter().all(|failure| {
                matches!(
                    failure,
                    LockedResolutionFailure::PackageRequirementsChanged
                        | LockedResolutionFailure::RVersionChanged { .. }
                )
            }) {
                lockfile
                    .repos
                    .iter()
                    .map(<dyn PackageRepository>::from_lockfile)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(lock_error_from_repository)?
            } else {
                repositories_from_description(&current_dir, &description).await?
            };
            let preferred_versions = lockfile
                .packages
                .iter()
                .map(|(name, package)| (name.clone(), package.version.clone()))
                .collect();
            let selected = resolve_from_registry(
                repositories.clone(),
                Arc::clone(&root),
                desired_roots.clone(),
                preferred_versions,
            )
            .await
            .map_err(lock_error_from_resolution)?;
            let resolved = hydrate_resolved_packages(selected).await?;
            let sysreq_db = load_sysreq_snapshot_for_lock(Some(lockfile)).await;
            let lockfile = lockfile_from_resolution(
                desired_roots.clone(),
                &resolved
                    .iter()
                    .filter(|(_, (version, _))| !version.repository().equals(root.as_ref()))
                    .map(|(name, package)| (name.clone(), package.clone()))
                    .collect::<RequiredPackages>(),
                &sysreq_db,
                &repositories,
                &r_version,
            )
            .await?;
            (lockfile, resolved)
        }
        Some((_, Err(source))) => return Err(source.into()),
        None => {
            let repositories = repositories_from_description(&current_dir, &description).await?;
            let selected = resolve_from_registry(
                repositories.clone(),
                Arc::clone(&root),
                desired_roots.clone(),
                BTreeMap::new(),
            )
            .await
            .map_err(lock_error_from_resolution)?;
            let resolved = hydrate_resolved_packages(selected).await?;
            let sysreq_db = load_sysreq_snapshot_for_lock(None).await;
            let lockfile = lockfile_from_resolution(
                desired_roots,
                &resolved
                    .iter()
                    .filter(|(_, (version, _))| !version.repository().equals(root.as_ref()))
                    .map(|(name, package)| (name.clone(), package.clone()))
                    .collect::<RequiredPackages>(),
                &sysreq_db,
                &repositories,
                &r_version,
            )
            .await?;
            (lockfile, resolved)
        }
    };

    let base_packages = base_packages().await.map_err(LockError::BasePackages)?;
    let final_added_relations = unconstrained_packages
        .iter()
        // Base packages are supplied by R and intentionally absent from the resolved map.
        .filter(|package| !base_packages.contains(package.as_str()))
        .fold(added_relations.clone(), |mut relations, package| {
            let (selected, _) = resolved
                .get(package)
                .expect("resolved package map should contain every added package");
            let version = selected.version();
            let next_major = format!("{}.0.0", version.major() + 1)
                .parse::<Version>()
                .expect("next major version should be valid");

            relations.retain(|relation| {
                relation.package() != package
                    || !matches!(relation.requirement(), VersionRequirement::Any)
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

            relations
        });

    if final_added_relations != added_relations {
        add_dependencies(&current_dir, &mut description, &final_added_relations)?;
        lockfile.requirements = project_dependencies(&current_dir, &description)?;
    }

    // The lockfile contains only external packages, but sync also needs the local
    // root. Reinsert it with the in-memory DESCRIPTION used during resolution.
    let root =
        Arc::new(LocalRepository::new(current_dir.clone()).with_description(description.clone()));
    resolved.insert(
        root_name,
        (
            PackageVersion::new(root_version, root),
            Arc::new(description.clone()),
        ),
    );

    write_description(&current_dir, &description)?;
    write_lockfile(&current_dir, &lockfile)?;
    sync_system_dependencies(&lockfile, false, false)?;
    let project_library = project_library_path(&current_dir);
    let installed = installed_packages(&project_library).await?;
    sync_packages(&project_library, resolved, installed, &r_version).await?;
    status(format_args!(
        "Added {}",
        added_relations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(())
}

#[derive(Debug, Error, Diagnostic)]
enum RemoveError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionWrite(#[from] DescriptionWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileWrite(#[from] LockfileWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repositories(#[from] RepositoriesFromDescriptionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),

    #[error("failed to load resolved package metadata: {source}")]
    #[diagnostic(code(rpx::remove::package_metadata_failed))]
    PackageMetadata {
        #[from]
        source: RepositoryError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] r::InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

async fn cmd_remove(packages: &[String]) -> Result<(), RemoveError> {
    let current_dir = find_project_root()?;
    let mut description = read_description(&current_dir)?;
    let old_lockfile = match read_lockfile(&current_dir) {
        Ok(lockfile) => Some(lockfile),
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(LockfileReadError::OutdatedLockfile { .. }) => None,
        Err(source) => return Err(source.into()),
    };
    let r_version = r_version_async().await?;

    let removed_packages = packages.iter().cloned().collect::<BTreeSet<_>>();
    remove_dependencies(&current_dir, &mut description, &removed_packages)?;

    let desired_roots = project_dependencies(&current_dir, &description)?;
    let (root_name, root_version) = root_package(&current_dir, &description)?;
    let root =
        Arc::new(LocalRepository::new(current_dir.clone()).with_description(description.clone()));

    let (lockfile, mut resolved) = match old_lockfile.as_ref().map(|lockfile| {
        (
            lockfile,
            validate_locked_resolution(&current_dir, &description, &r_version, lockfile),
        )
    }) {
        Some((lockfile, Ok(()))) => (lockfile.clone(), required_packages_from_lockfile(lockfile)?),
        Some((lockfile, Err(LockedResolutionError::Validation { failures }))) => {
            let repositories = if failures.iter().all(|failure| {
                matches!(
                    failure,
                    LockedResolutionFailure::PackageRequirementsChanged
                        | LockedResolutionFailure::RVersionChanged { .. }
                )
            }) {
                lockfile
                    .repos
                    .iter()
                    .map(<dyn PackageRepository>::from_lockfile)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(lock_error_from_repository)?
            } else {
                repositories_from_description(&current_dir, &description).await?
            };
            let preferred_versions = lockfile
                .packages
                .iter()
                .map(|(name, package)| (name.clone(), package.version.clone()))
                .collect();
            let selected = resolve_from_registry(
                repositories.clone(),
                Arc::clone(&root),
                desired_roots.clone(),
                preferred_versions,
            )
            .await
            .map_err(lock_error_from_resolution)?;
            let resolved = hydrate_resolved_packages(selected).await?;
            let sysreq_db = load_sysreq_snapshot_for_lock(Some(lockfile)).await;
            let lockfile = lockfile_from_resolution(
                desired_roots.clone(),
                &resolved
                    .iter()
                    .filter(|(_, (version, _))| !version.repository().equals(root.as_ref()))
                    .map(|(name, package)| (name.clone(), package.clone()))
                    .collect::<RequiredPackages>(),
                &sysreq_db,
                &repositories,
                &r_version,
            )
            .await?;
            (lockfile, resolved)
        }
        Some((_, Err(source))) => return Err(source.into()),
        None => {
            let repositories = repositories_from_description(&current_dir, &description).await?;
            let selected = resolve_from_registry(
                repositories.clone(),
                Arc::clone(&root),
                desired_roots.clone(),
                BTreeMap::new(),
            )
            .await
            .map_err(lock_error_from_resolution)?;
            let resolved = hydrate_resolved_packages(selected).await?;
            let sysreq_db = load_sysreq_snapshot_for_lock(None).await;
            let lockfile = lockfile_from_resolution(
                desired_roots,
                &resolved
                    .iter()
                    .filter(|(_, (version, _))| !version.repository().equals(root.as_ref()))
                    .map(|(name, package)| (name.clone(), package.clone()))
                    .collect::<RequiredPackages>(),
                &sysreq_db,
                &repositories,
                &r_version,
            )
            .await?;
            (lockfile, resolved)
        }
    };

    // The lockfile contains only external packages, but sync also needs the local
    // root. Reinsert it with the in-memory DESCRIPTION used during resolution.
    resolved.insert(
        root_name,
        (
            PackageVersion::new(root_version, root),
            Arc::new(description.clone()),
        ),
    );

    write_description(&current_dir, &description)?;
    write_lockfile(&current_dir, &lockfile)?;
    sync_system_dependencies(&lockfile, false, false)?;

    let project_library = project_library_path(&current_dir);
    let installed = installed_packages(&project_library).await?;
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

    sync_packages(&project_library, resolved, installed, &r_version).await?;

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

#[derive(Debug, Error, Diagnostic)]
enum RunError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error("failed to run {program}")]
    #[diagnostic(code(rpx::run::command_failed))]
    CommandFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

async fn cmd_run(command: &[String]) -> Result<(), RunError> {
    let project_path = find_project_root()?;
    let (program, args) = command
        .split_first()
        .expect("run command requires at least one argument");
    let project_library = project_library_path(&project_path);

    let status = Command::with_venv(program, &project_library)
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

#[derive(Debug, Error, Diagnostic)]
enum LockError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileWrite(#[from] LockfileWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Repositories(#[from] RepositoriesFromDescriptionError),

    #[error("failed to load resolved package metadata: {source}")]
    #[diagnostic(code(rpx::lock::package_metadata_failed))]
    PackageMetadata {
        #[from]
        source: RepositoryError,
    },

    #[error("failed to prepare package requirements: {details}")]
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

    #[error("could not access Git repository {repository}")]
    #[diagnostic(
        code(rpx::lock::git_repository_unavailable),
        help(
            "Check that the repository exists. For private repositories, configure Git credentials."
        )
    )]
    GitRepositoryUnavailable { repository: String },

    #[error("failed to resolve package set")]
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

    #[error("invalid system requirements database commit {commit}: {source}")]
    #[diagnostic(code(rpx::lock::invalid_sysreq_commit))]
    InvalidSystemRequirementsCommit {
        commit: String,
        #[source]
        source: git2::Error,
    },
}

async fn resolve_lockfile_for_description(
    current_dir: &Path,
    description: &RDescription,
    old_lockfile: Option<&Lockfile>,
) -> Result<Lockfile, LockError> {
    let r_version = r_version_async().await?;
    let roots = project_dependencies(current_dir, description)?;
    let repositories = repositories_from_description(current_dir, description).await?;
    let preferred_versions = old_lockfile
        .map(|lockfile| {
            lockfile
                .packages
                .iter()
                .map(|(name, package)| (name.clone(), package.version.clone()))
                .collect()
        })
        .unwrap_or_default();

    let root = Arc::new(
        LocalRepository::new(current_dir.to_path_buf()).with_description(description.clone()),
    );
    let selected = resolve_from_registry(
        repositories.clone(),
        Arc::clone(&root),
        roots.clone(),
        preferred_versions,
    )
    .await
    .map_err(lock_error_from_resolution)?;
    let resolved = hydrate_resolved_packages(selected).await?;
    let sysreq_db = load_sysreq_snapshot_for_lock(old_lockfile).await;
    lockfile_from_resolution(
        roots,
        &resolved
            .iter()
            .filter(|(_, (version, _))| !version.repository().equals(root.as_ref()))
            .map(|(name, package)| (name.clone(), package.clone()))
            .collect::<RequiredPackages>(),
        &sysreq_db,
        &repositories,
        &r_version,
    )
    .await
}

async fn cmd_lock() -> Result<(), LockError> {
    let current_dir = find_project_root()?;
    let description = read_description(&current_dir)?;
    let old_lockfile = match read_lockfile(&current_dir) {
        Ok(lockfile) => Some(lockfile),
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(LockfileReadError::OutdatedLockfile { .. }) => None,
        Err(source) => return Err(source.into()),
    };
    let lockfile =
        resolve_lockfile_for_description(&current_dir, &description, old_lockfile.as_ref()).await?;
    let changed = old_lockfile.as_ref() != Some(&lockfile);
    write_lockfile(&current_dir, &lockfile)?;

    if changed {
        status("Updated rpx.lock");
    } else {
        status("rpx.lock is already up to date");
    }
    Ok(())
}

#[derive(Debug, Error, Diagnostic)]
enum SyncError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedPackages(#[from] LockedPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] r::InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RemovePackages(#[from] r::PackageRemovalError),

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

    #[error("failed to install package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_install_failed))]
    PackageInstall {
        package: String,
        #[source]
        source: r::PackageInstallError,
    },
}

async fn cmd_sync(install_system: bool, install_only_system: bool) -> Result<(), SyncError> {
    let current_dir = find_project_root()?;
    let description = read_description(&current_dir)?;
    let lockfile = read_lockfile(&current_dir)?;
    let r_version = r_version_async().await?;
    validate_locked_resolution(&current_dir, &description, &r_version, &lockfile)?;

    sync_system_dependencies(&lockfile, install_system, install_only_system)?;
    if install_only_system {
        return Ok(());
    }

    let mut required = required_packages_from_lockfile(&lockfile)?;
    let (root_name, root_version) = root_package(&current_dir, &description)?;
    let root =
        Arc::new(LocalRepository::new(current_dir.clone()).with_description(description.clone()));
    required.insert(
        root_name,
        (
            PackageVersion::new(root_version, root),
            Arc::new(description),
        ),
    );

    let project_library = project_library_path(&current_dir);
    let installed = installed_packages(&project_library).await?;
    sync_packages(&project_library, required, installed, &r_version).await?;
    status("Synchronized project library");
    Ok(())
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
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockfileRead(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] r::InstalledPackagesError),

    #[error("project is out of sync\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::status::out_of_sync),
        help("Run `rpx sync` to synchronize the project.")
    )]
    OutOfSync { mismatches: StatusMismatches },
}

async fn cmd_status() -> Result<(), StatusError> {
    let current_dir = find_project_root()?;
    let description = read_description(&current_dir)?;
    let lockfile = read_lockfile(&current_dir)?;
    let r_version = r_version_async().await?;
    validate_locked_resolution(&current_dir, &description, &r_version, &lockfile)?;
    let base_packages = base_packages().await?;

    let mut expected_packages = lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let (root_name, root_version) = root_package(&current_dir, &description)?;
    expected_packages.insert(root_name, root_version);

    let project_library = project_library_path(&current_dir);
    let installed = installed_packages(&project_library).await?;
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
        .filter(|package| !expected_packages.contains_key(*package))
        .cloned()
        .collect();
    let mut mismatches = StatusMismatches {
        missing_packages,
        extra_packages,
        version_mismatches,
        ..StatusMismatches::default()
    };

    let system_plan = if host_supports_system_sync() {
        system_plan_from_lockfile(&lockfile).ok()
    } else {
        None
    };
    if let Some(plan) = system_plan {
        mismatches.missing_system_packages = plan.missing_packages;
        mismatches.unsupported_system_rules = plan.unsupported_rules;
    }

    if !mismatches.is_empty() {
        return Err(StatusError::OutOfSync { mismatches });
    }

    status("Project is in sync");
    Ok(())
}

#[derive(Debug, Error, Diagnostic)]
enum CleanError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error("failed to remove {label} at {path}")]
    #[diagnostic(code(rpx::clean::remove_failed))]
    RemoveFailed {
        label: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn cmd_clean() -> Result<(), CleanError> {
    let current_dir = find_project_root()?;
    let mut removed_any = false;

    removed_any |=
        remove_dir_if_exists(&project_library_root_path(&current_dir), "project library")?;
    removed_any |= remove_dir_if_exists(&cache_dir_path(), "cache directory")?;

    if removed_any {
        status("Removed project library and cache directories");
    } else {
        status("Project library and cache directories are already clean");
    }
    Ok(())
}

fn remove_dir_if_exists(path: &Path, label: &str) -> Result<bool, CleanError> {
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

#[derive(Debug, Error, Diagnostic)]
#[error("invalid package constraint {package}: {details}")]
#[diagnostic(
    code(rpx::add::invalid_constraint),
    help("Use PACKAGE@OPERATORVERSION, for example digest@>=0.6.37.")
)]
struct AddPackageParseError {
    package: String,
    details: String,
}

fn parse_add_package(package: &str) -> Result<Relation, AddPackageParseError> {
    if package.is_empty() || package.chars().any(char::is_whitespace) {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "package specifications cannot contain whitespace".to_string(),
        });
    }

    let Some((name, constraint)) = package.split_once('@') else {
        return Relation::any(package).map_err(|source| AddPackageParseError {
            package: package.to_string(),
            details: source.to_string(),
        });
    };
    if name.is_empty() {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "package name is missing".to_string(),
        });
    }

    let (operator, version) = [">=", "<=", "==", "!=", ">", "<"]
        .into_iter()
        .find_map(|operator| {
            constraint
                .strip_prefix(operator)
                .map(|version| (operator, version))
        })
        .ok_or_else(|| AddPackageParseError {
            package: package.to_string(),
            details: "version constraint operator is missing or invalid".to_string(),
        })?;
    if version.is_empty() {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "version is missing".to_string(),
        });
    }

    let version = version
        .parse::<Version>()
        .map_err(|source| AddPackageParseError {
            package: package.to_string(),
            details: source.to_string(),
        })?;
    let requirement = match operator {
        ">=" => VersionRequirement::GreaterThanEqual(version),
        "<=" => VersionRequirement::LessThanEqual(version),
        "==" => VersionRequirement::Equal(version),
        "!=" => VersionRequirement::NotEqual(version),
        ">" => VersionRequirement::GreaterThan(version),
        "<" => VersionRequirement::LessThan(version),
        _ => unreachable!("constraint operator was selected from a fixed set"),
    };

    Relation::new(name, requirement).map_err(|source| AddPackageParseError {
        package: package.to_string(),
        details: source.to_string(),
    })
}

async fn load_sysreq_snapshot_for_lock(
    existing_lockfile: Option<&Lockfile>,
) -> sysreqs::SysreqDbSnapshot {
    let existing_commit = existing_lockfile
        .and_then(|lockfile| lockfile.sysreqs.db_commit)
        .map(|commit| commit.to_string());

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
    project_library: &Path,
    required: RequiredPackages,
    installed: BTreeMap<String, PackageVersion>,
    r_version: &semver::Version,
) -> Result<(), SyncError> {
    let packages_to_remove = installed
        .iter()
        .filter(|(name, installed_version)| {
            required.get(*name).is_none_or(|(required_version, _)| {
                package_requires_install(required_version, Some(installed_version))
            })
        })
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let retained = installed
        .iter()
        .filter(|(name, installed_version)| {
            required.get(*name).is_some_and(|(required_version, _)| {
                !package_requires_install(required_version, Some(installed_version))
            })
        })
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let packages_to_install = required
        .into_iter()
        .filter(|(name, (required_version, _))| {
            package_requires_install(required_version, installed.get(name))
        })
        .collect();

    remove_packages_from_venv(project_library, &packages_to_remove)?;
    install_required_packages(project_library, packages_to_install, retained, r_version).await?;

    Ok(())
}

fn package_requires_install(required: &PackageVersion, installed: Option<&PackageVersion>) -> bool {
    let repository = required.repository().as_ref();

    // Git and local sources can change without changing their package version.
    repository.downcast_ref::<GitRepository>().is_some()
        || repository.downcast_ref::<LocalRepository>().is_some()
        || installed != Some(required)
}

pub(crate) fn exit_with_status(code: Option<i32>) {
    if code != Some(0) {
        std::process::exit(code.unwrap_or(1));
    }
}

async fn hydrate_resolved_packages(
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

async fn lockfile_from_resolution(
    requirements: BTreeSet<Relation>,
    resolved_packages: &RequiredPackages,
    sysreq_snapshot: &sysreqs::SysreqDbSnapshot,
    repositories: &[Arc<dyn PackageRepository>],
    r_version: &semver::Version,
) -> Result<Lockfile, LockError> {
    let repos = futures_util::future::join_all(
        repositories
            .iter()
            .map(|repository| repository.to_lockfile()),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .map_err(lock_error_from_repository)?;

    let packages = resolved_packages
        .iter()
        .map(|(name, (version, description))| {
            let repository = repositories
                .iter()
                .zip(&repos)
                .find(|(runtime, _)| version.repository().equals(runtime.as_ref()))
                .map(|(_, locked)| locked.url().clone())
                .ok_or_else(|| LockError::UnsupportedRepository {
                    repository: version.repository().to_string(),
                })?;

            let depends = description
                .depends()
                .map_err(|source| LockError::ResolveFailed {
                    details: source.to_string(),
                })?;
            let imports = description
                .imports()
                .map_err(|source| LockError::ResolveFailed {
                    details: source.to_string(),
                })?;
            let linking_to =
                description
                    .linking_to()
                    .map_err(|source| LockError::ResolveFailed {
                        details: source.to_string(),
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
        .collect::<Result<BTreeMap<_, _>, LockError>>()?;

    let mut rules = BTreeMap::<String, BTreeSet<String>>::new();
    for (package, (_, description)) in resolved_packages {
        let package_rules =
            sysreqs::match_rules(description, sysreq_snapshot).map_err(|source| {
                LockError::ResolveFailed {
                    details: source.to_string(),
                }
            })?;
        for rule in package_rules {
            rules.entry(rule).or_default().insert(package.clone());
        }
    }

    let db_commit = (!sysreq_snapshot.commit.is_empty())
        .then(|| sysreq_snapshot.commit.parse())
        .transpose()
        .map_err(|source| LockError::InvalidSystemRequirementsCommit {
            commit: sysreq_snapshot.commit.clone(),
            source,
        })?;

    Ok(Lockfile {
        version: lockfile::LOCKFILE_VERSION,
        revision: lockfile::LOCKFILE_REVISION,
        r: r_version.clone(),
        sysreqs: lockfile::SystemRequirements { db_commit, rules },
        repos,
        requirements,
        packages,
    })
}

fn package_dependency_names(description: &RDescription) -> Result<BTreeSet<String>, String> {
    let depends = description.depends().map_err(|error| error.to_string())?;
    let imports = description.imports().map_err(|error| error.to_string())?;
    let linking_to = description
        .linking_to()
        .map_err(|error| error.to_string())?;

    Ok(depends
        .chain(imports)
        .chain(linking_to)
        .map(|relation| relation.package().to_string())
        .filter(|package| package != "R")
        .collect())
}

fn package_rules_from_lockfile(
    rules: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, Vec<String>> {
    rules
        .iter()
        .flat_map(|(rule, packages)| {
            packages
                .iter()
                .map(move |package| (package.clone(), rule.clone()))
        })
        .fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut package_rules, (package, rule)| {
                package_rules.entry(package).or_default().push(rule);
                package_rules
            },
        )
}

fn system_plan_from_lockfile(lockfile: &Lockfile) -> Result<SystemDependencyPlan, String> {
    let Some(db_commit) = lockfile.sysreqs.db_commit.as_ref() else {
        return Ok(system_plan_without_db(lockfile));
    };

    let snapshot = sysreqs::snapshot_for_commit(&db_commit.to_string())?;
    let package_rules = package_rules_from_lockfile(&lockfile.sysreqs.rules);

    Ok(resolve_system_plan(&snapshot, &package_rules))
}

fn system_plan_without_db(lockfile: &Lockfile) -> SystemDependencyPlan {
    SystemDependencyPlan {
        host: current_host_platform(),
        missing_packages: vec![],
        install_packages: vec![],
        pre_install_commands: vec![],
        post_install_commands: vec![],
        unsupported_rules: lockfile.sysreqs.rules.keys().cloned().collect(),
        package_rules: package_rules_from_lockfile(&lockfile.sysreqs.rules),
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
) -> Result<(), SyncError> {
    if !host_supports_system_sync() {
        if install_system || install_only_system {
            return Err(SyncError::UnsupportedSystemInstall);
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
) -> Result<(), SyncError> {
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
            return Err(SyncError::SystemDependenciesFailed { details: error });
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
                return Err(SyncError::SystemDependenciesFailed { details: error });
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

async fn install_required_packages(
    project_library: &Path,
    packages: RequiredPackages,
    retained: BTreeSet<String>,
    r_version: &semver::Version,
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

    let project_library = project_library.to_path_buf();
    let r_version = Arc::new(r_version.clone());
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
        let dependencies = package_dependency_names(&description)
            .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;
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
            let install_project_library = project_library.clone();
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

                    install_package_directory(
                        &project_path,
                        &install_project_library,
                        "project package",
                    )
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

        if let Some(repository) = package_version
            .repository()
            .as_ref()
            .downcast_ref::<GitRepository>()
        {
            let repository = repository.clone();
            let install_required_names = Arc::clone(&required_names);
            let install_installed_packages = Arc::clone(&installed_packages);
            let install_installed_rx = installed_rx.clone();
            let install_installed_tx = installed_tx.clone();
            let install_shared_pool = Arc::clone(&shared_pool);
            let install_pool = Arc::clone(&install_pool);
            let install_project_library = project_library.clone();
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
                            details: "install pool closed before Git package installation"
                                .to_string(),
                        }
                    })?;
                    let _shared_permit =
                        install_shared_pool.acquire_owned().await.map_err(|_| {
                            SyncError::DownloadArtifactsFailed {
                                details: "sync work pool closed before Git package installation"
                                    .to_string(),
                            }
                        })?;

                    let checkout = repository.checkout().await.map_err(|error| {
                        SyncError::DownloadArtifactsFailed {
                            details: format!("failed to checkout {package_name}: {error}"),
                        }
                    })?;
                    let package_root = repository
                        .subdirectory()
                        .map_or(checkout.clone(), |subdirectory| checkout.join(subdirectory));
                    install_package_directory(
                        &package_root,
                        &install_project_library,
                        &format!("{package_name} from Git"),
                    )
                    .await
                    .map_err(|source| SyncError::PackageInstall {
                        package: package_name.clone(),
                        source,
                    })?;
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
            package_version.version().as_ref(),
            r_version.as_ref(),
        );
        let (prepared_tx, prepared_rx) = oneshot::channel();

        let prepare_package_name = package_name.clone();
        let prepare_package_version = package_version.clone();
        let prepare_cache_key = cache_key.clone();
        let prepare_r_version = Arc::clone(&r_version);
        let prepare_shared_pool = Arc::clone(&shared_pool);
        prepare_tasks.spawn(
            async move {
                let prepared = match prepare_shared_pool.acquire_owned().await {
                    Ok(_permit) => {
                        prepare_locked_package_artifact(
                            prepare_package_name,
                            prepare_package_version,
                            prepare_cache_key,
                            prepare_r_version,
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
        let install_project_library = project_library.clone();
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
                    install_project_library,
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
    r_version: Arc<semver::Version>,
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
        r_version.as_ref(),
        span.clone(),
    )
    .instrument(span)
    .await
}

async fn prepare_locked_package_artifact_inner(
    package: String,
    package_version: PackageVersion,
    cache_key: &CompiledPackageCacheKey,
    r_version: &semver::Version,
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
        ("windows", true) => http::rrepo_windows_binary(base_url, &package, &version, r_version)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("windows", false) => http::cran_windows_binary(base_url, r_version, &package, &version)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("macos", true) => {
            let target = macos_binary_target()?;

            http::rrepo_macos_binary(base_url, &package, &version, &target, r_version)
                .await
                .map_err(|error| error.to_string())
                .and_then(response_for_status)
                .map(|response| (response, "tgz", format!("mac.binary.{target}")))
        }

        ("macos", false) => {
            let target = macos_binary_target()?;

            http::cran_macos_binary(base_url, &target, r_version, &package, &version)
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
    project_library: PathBuf,
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

    install_prepared_package_inner(
        project_library,
        package,
        version,
        cache_key,
        prepared_artifact,
        span.clone(),
    )
    .instrument(span)
    .await
}

async fn install_prepared_package_inner(
    project_library: PathBuf,
    package: String,
    version: String,
    cache_key: CompiledPackageCacheKey,
    prepared_artifact: Option<(PathBuf, String)>,
    span: tracing::Span,
) -> Result<String, String> {
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
        &project_library,
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
        let internal_dependencies = package_dependency_names(description)?
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
        LockError, RequiredPackages, lock_error_from_repository, lock_error_from_resolution,
        lockfile_from_resolution, package_dependency_names, package_requires_install,
        package_rules_from_lockfile, parse_add_package, required_package_install_order,
    };
    use crate::{
        git::GitError,
        r::BasePackagesError,
        repository::{
            GitRepository, LocalRepository, PackageRepository, RepositoryError, built_in_repository,
        },
        resolver::{PackageVersion, ProviderError, RDependencyProvider, ResolutionError},
        sysreqs::{SysreqDbSnapshot, SysreqRule},
    };
    use miette::Diagnostic;
    use pubgrub::{DerivationTree, External, PubGrubError, Ranges};
    use r_description::{RDescription, Relation, Remote, Version};
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
        sync::Arc,
    };

    fn version(value: &str) -> Version {
        value.parse().expect("version fixture should parse")
    }

    fn relation(value: &str) -> Relation {
        value.parse().expect("relation fixture should parse")
    }

    fn required_packages(packages: &[(&str, &str)]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, fields)| {
                let description =
                    RDescription::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}"));
                (
                    (*name).to_string(),
                    (
                        PackageVersion::new(version("1.0.0"), built_in_repository()),
                        Arc::new(description),
                    ),
                )
            })
            .collect()
    }

    fn git_access_error(remote: &str) -> RepositoryError {
        RepositoryError::Git {
            repository: remote.to_string(),
            source: Arc::new(GitError::Access {
                remote: remote.to_string(),
                source: git2::Error::from_str("access denied"),
            }),
        }
    }

    fn ordinary_repository_error() -> RepositoryError {
        RepositoryError::InvalidData {
            resource: "fixture".to_string(),
            details: "invalid".to_string(),
        }
    }

    #[test]
    fn parse_add_package_accepts_supported_forms() {
        for (input, expected) in [
            ("dplyr", "dplyr"),
            ("dplyr@>=1.0.0", "dplyr (>= 1.0.0)"),
            ("dplyr@<=1.0.0", "dplyr (<= 1.0.0)"),
            ("dplyr@==1.0.0", "dplyr (== 1.0.0)"),
            ("dplyr@!=1.0.0", "dplyr (!= 1.0.0)"),
            ("dplyr@>1.0.0", "dplyr (> 1.0.0)"),
            ("dplyr@<1.0.0", "dplyr (< 1.0.0)"),
        ] {
            let parsed = parse_add_package(input).expect("supported form should parse");
            assert_eq!(parsed.package(), "dplyr");
            assert_eq!(parsed.to_string(), expected);
        }
    }

    #[test]
    fn parse_add_package_rejects_invalid_forms() {
        for input in [
            "",
            "dplyr >= 1.0.0",
            "@>=1.0.0",
            "dplyr@=1.0.0",
            "dplyr@>=",
            "dplyr@>= 1.0.0",
        ] {
            assert!(parse_add_package(input).is_err(), "{input:?} should fail");
        }
    }

    #[test]
    fn lock_error_from_resolution_maps_current_categories() {
        let source: PubGrubError<RDependencyProvider> = PubGrubError::NoSolution(
            DerivationTree::External(External::NoVersions("missing".into(), Ranges::empty())),
        );
        let error = lock_error_from_resolution(ResolutionError::PubGrub(source));
        assert!(matches!(
            &error,
            LockError::NoSolution { explanation } if explanation.contains("missing")
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::lock::no_solution")
        );

        for provider in [
            ProviderError::Repository(git_access_error("https://example.test/repo.git")),
            ProviderError::DependencyMetadata {
                repository: "fixture".into(),
                source: git_access_error("ssh://git@example.test/repo.git"),
            },
        ] {
            assert!(matches!(
                lock_error_from_resolution(ResolutionError::Provider(provider)),
                LockError::GitRepositoryUnavailable { repository }
                    if repository.contains("example.test/repo.git")
            ));
        }
        let base = BasePackagesError::InvalidUtf8 {
            source: String::from_utf8(vec![0xff]).expect_err("invalid UTF-8 fixture"),
        };
        assert!(matches!(
            lock_error_from_resolution(ResolutionError::BasePackages(base)),
            LockError::BasePackages(_)
        ));
        assert!(matches!(
            lock_error_from_resolution(ResolutionError::Provider(ProviderError::Repository(
                ordinary_repository_error()
            ))),
            LockError::Resolution { .. }
        ));
    }

    #[test]
    fn lock_error_from_repository_maps_current_categories() {
        assert!(matches!(
            lock_error_from_repository(git_access_error("https://example.test/private.git")),
            LockError::GitRepositoryUnavailable { repository }
                if repository == "https://example.test/private.git"
        ));
        assert!(matches!(
            lock_error_from_repository(ordinary_repository_error()),
            LockError::Repository { .. }
        ));
    }

    #[tokio::test]
    async fn lockfile_from_resolution_assembles_current_metadata() {
        let requirements = BTreeSet::from([relation("selected (>= 1.0.0)")]);
        let resolved = required_packages(&[(
            "selected",
            "Depends: R (>= 4.4), hardDepends\nImports: hardImports\nLinkingTo: hardLinking\nSuggests: optional\nSystemRequirements: libcurl\n",
        )]);
        let commit = "1111111111111111111111111111111111111111";
        let snapshot = SysreqDbSnapshot {
            commit: commit.into(),
            rules: vec![SysreqRule {
                id: "libcurl".into(),
                patterns: vec!["libcurl".into()],
                dependencies: vec![],
            }],
            scripts: BTreeMap::new(),
        };
        let lockfile = lockfile_from_resolution(
            requirements.clone(),
            &resolved,
            &snapshot,
            &[built_in_repository()],
            &semver::Version::new(4, 5, 1),
        )
        .await
        .expect("resolution should lock");
        let package = &lockfile.packages["selected"];
        assert_eq!(lockfile.requirements, requirements);
        assert_eq!(lockfile.r, semver::Version::new(4, 5, 1));
        assert_eq!(lockfile.repos.len(), 1);
        assert_eq!(lockfile.repos[0].url(), &package.repository);
        assert_eq!(package.version, version("1.0.0"));
        assert_eq!(
            package.dependencies,
            BTreeSet::from([
                relation("R (>= 4.4)"),
                relation("hardDepends"),
                relation("hardImports"),
                relation("hardLinking"),
            ])
        );
        assert_eq!(lockfile.sysreqs.db_commit, Some(commit.parse().unwrap()));
        assert_eq!(
            lockfile.sysreqs.rules,
            BTreeMap::from([("libcurl".into(), BTreeSet::from(["selected".into()]))])
        );
    }

    #[tokio::test]
    async fn lockfile_from_resolution_rejects_unprovided_repository() {
        let local: Arc<dyn PackageRepository> =
            Arc::new(LocalRepository::new(PathBuf::from("vendor/selected")));
        let resolved = BTreeMap::from([(
            "selected".into(),
            (
                PackageVersion::new(version("1.0.0"), local),
                Arc::new(RDescription::parse("Package: selected\nVersion: 1.0.0\n")),
            ),
        )]);
        assert!(matches!(
            lockfile_from_resolution(
                BTreeSet::new(), &resolved, &crate::sysreqs::empty_snapshot(),
                &[built_in_repository()], &semver::Version::new(4, 5, 0),
            ).await,
            Err(LockError::UnsupportedRepository { repository })
                if repository == "vendor/selected"
        ));
    }

    #[tokio::test]
    async fn lockfile_from_resolution_rejects_invalid_metadata() {
        for field in ["Depends", "Imports", "LinkingTo"] {
            let metadata = format!("{field}: broken (>= invalid)\n");
            let resolved = required_packages(&[("selected", &metadata)]);
            assert!(matches!(
                lockfile_from_resolution(
                    BTreeSet::new(),
                    &resolved,
                    &crate::sysreqs::empty_snapshot(),
                    &[built_in_repository()],
                    &semver::Version::new(4, 5, 0),
                )
                .await,
                Err(LockError::ResolveFailed { .. })
            ));
        }
        let snapshot = SysreqDbSnapshot {
            commit: "not-an-oid".into(),
            rules: vec![],
            scripts: BTreeMap::new(),
        };
        assert!(matches!(
            lockfile_from_resolution(
                BTreeSet::new(), &RequiredPackages::new(), &snapshot,
                &[built_in_repository()], &semver::Version::new(4, 5, 0),
            ).await,
            Err(LockError::InvalidSystemRequirementsCommit { commit, .. })
                if commit == "not-an-oid"
        ));
    }

    #[test]
    fn package_dependency_names_uses_only_hard_dependencies() {
        let description = RDescription::parse(
            "Package: selected\nVersion: 1.0.0\nDepends: R, depends, duplicate\nImports: imports, duplicate\nLinkingTo: linking\nSuggests: suggested\n",
        );
        assert_eq!(
            package_dependency_names(&description).unwrap(),
            BTreeSet::from([
                "depends".into(),
                "duplicate".into(),
                "imports".into(),
                "linking".into()
            ])
        );
        for field in ["Depends", "Imports", "LinkingTo"] {
            let description = RDescription::parse(&format!(
                "Package: selected\nVersion: 1.0.0\n{field}: broken (>= invalid)\n"
            ));
            assert!(package_dependency_names(&description).is_err());
        }
    }

    #[test]
    fn package_rules_from_lockfile_inverts_ordered_used_rules() {
        let rules = BTreeMap::from([
            (
                "libcurl".into(),
                BTreeSet::from(["curl".into(), "httr2".into()]),
            ),
            ("openssl".into(), BTreeSet::from(["curl".into()])),
            ("unused".into(), BTreeSet::new()),
        ]);
        assert_eq!(
            package_rules_from_lockfile(&rules),
            BTreeMap::from([
                ("curl".into(), vec!["libcurl".into(), "openssl".into()]),
                ("httr2".into(), vec!["libcurl".into()]),
            ])
        );
    }

    #[test]
    fn package_requires_install_respects_source_and_version() {
        let registry = PackageVersion::new(version("1.0.0"), built_in_repository());
        let same = PackageVersion::new(version("1.0.0"), built_in_repository());
        let old = PackageVersion::new(version("0.9.0"), built_in_repository());
        assert!(package_requires_install(&registry, None));
        assert!(!package_requires_install(&registry, Some(&same)));
        assert!(package_requires_install(&registry, Some(&old)));
        let local: Arc<dyn PackageRepository> =
            Arc::new(LocalRepository::new(PathBuf::from("vendor/selected")));
        let git: Arc<dyn PackageRepository> = Arc::new(
            GitRepository::new("github::owner/repository".parse::<Remote>().unwrap()).unwrap(),
        );
        assert!(package_requires_install(
            &PackageVersion::new(version("1.0.0"), local),
            Some(&same)
        ));
        assert!(package_requires_install(
            &PackageVersion::new(version("1.0.0"), git),
            Some(&same)
        ));
    }

    #[test]
    fn required_package_install_order_is_complete_and_dependency_first() {
        let packages = required_packages(&[
            ("dependent", "Imports: dependency, external\n"),
            ("dependency", ""),
            ("unrelated", ""),
        ]);
        let order = required_package_install_order(&packages).unwrap();
        let position = |name| order.iter().position(|package| package == name).unwrap();
        assert!(position("dependency") < position("dependent"));
        assert!(order.contains(&"unrelated".into()));
        assert!(!order.contains(&"external".into()));
    }

    #[test]
    fn required_package_install_order_reports_all_blocked_names() {
        let packages = required_packages(&[
            ("a", "Imports: b\n"),
            ("b", "Imports: a\n"),
            ("c", "Imports: a\n"),
        ]);
        assert_eq!(
            required_package_install_order(&packages).unwrap_err(),
            "cyclic or unresolved package dependencies: a, b, c"
        );
    }
}

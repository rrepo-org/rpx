use crate::{
    LockError, SyncError, configure_project_installation,
    description::{
        DescriptionParseError, DescriptionReadError, DescriptionWriteError,
        RepositoriesFromDescriptionError, project_dependencies, read_description,
        remove_dependencies, repositories_from_description, root_package, write_description,
    },
    hydrate_resolved_packages, load_sysreq_snapshot_for_lock, lock_error_from_repository,
    lock_error_from_resolution,
    lockfile::{LockfileReadError, LockfileWriteError, read_lockfile, write_lockfile},
    lockfile_from_resolution,
    output::status,
    project::{
        LockedPackagesError, LockedResolutionError, LockedResolutionFailure, ProjectDiscoveryError,
        RequiredPackages, find_project_root, project_library_path, required_packages_from_lockfile,
        validate_locked_resolution,
    },
    r::{RVersionError, base_packages, installed_packages, r_version_async},
    repository::{LocalRepository, PackageRepository, RepositoryError},
    resolver::resolve_from_registry,
    sync_packages, sync_system_dependencies,
};
use miette::Diagnostic;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
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
    InstalledPackages(#[from] crate::r::InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

pub(crate) async fn run(packages: &[String], no_install_project: bool) -> Result<(), Error> {
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

    let base_packages = if no_install_project {
        base_packages().await.map_err(LockError::BasePackages)?
    } else {
        BTreeSet::new()
    };
    configure_project_installation(
        &current_dir,
        &description,
        &mut resolved,
        root_name,
        root_version,
        no_install_project,
        &base_packages,
    )?;

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

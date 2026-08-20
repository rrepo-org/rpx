use crate::{
    LockError, SyncError, configure_project_installation,
    description::{
        DependencyField, DescriptionParseError, DescriptionReadError, DescriptionWriteError,
        RepositoriesFromDescriptionError, add_dependencies, project_dependencies, read_description,
        repositories_from_description, root_package, write_description,
    },
    hydrate_resolved_packages, load_sysreq_snapshot_for_lock, lock_error_from_repository,
    lock_error_from_resolution,
    lockfile::{LockfileReadError, LockfileWriteError, read_lockfile, write_lockfile},
    lockfile_from_resolution,
    output::status,
    pin_dependency_to_resolved_major,
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
use r_description::{Relation, Version, VersionRequirement};
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
    InstalledPackages(#[from] crate::r::InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

pub(crate) async fn run(
    packages: &[String],
    dependency_field: DependencyField,
    no_install_project: bool,
) -> Result<(), Error> {
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

    add_dependencies(
        &current_dir,
        &mut description,
        &added_relations,
        dependency_field,
    )?;

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
            pin_dependency_to_resolved_major(&mut relations, package, selected.version());

            relations
        });

    if final_added_relations != added_relations {
        add_dependencies(
            &current_dir,
            &mut description,
            &final_added_relations,
            dependency_field,
        )?;
        lockfile.requirements = project_dependencies(&current_dir, &description)?;
    }

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
#[error("invalid package constraint {package}: {details}")]
#[diagnostic(
    code(rpx::add::invalid_constraint),
    help("Use PACKAGE@OPERATORVERSION, for example digest@>=0.6.37.")
)]
pub(crate) struct AddPackageParseError {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

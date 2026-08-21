use crate::{
    SyncError,
    description::root_package,
    install_required_packages,
    project::{Project, ProjectResolution, RequiredPackages, project_library_path},
    r::{base_packages, installed_packages, remove_packages_from_venv},
    repository::{GitRepository, LocalRepository},
    resolver::PackageVersion,
    sync_system_dependencies,
};
use r_description::RDescription;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectPackageMode {
    Install,
    Omit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemSyncMode {
    Check,
    Install,
    InstallOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyncProjectOptions {
    pub(crate) project_package: ProjectPackageMode,
    pub(crate) system: SystemSyncMode,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SyncReport {
    pub(crate) installed_before: BTreeSet<String>,
    pub(crate) removed: BTreeSet<String>,
    pub(crate) installed: BTreeSet<String>,
    pub(crate) retained: BTreeSet<String>,
}

pub(crate) async fn sync_resolved_project(
    project: &Project,
    resolution: ProjectResolution,
    options: SyncProjectOptions,
) -> Result<SyncReport, SyncError> {
    let (install_system, install_only_system) = match options.system {
        SystemSyncMode::Check => (false, false),
        SystemSyncMode::Install => (true, false),
        SystemSyncMode::InstallOnly => (false, true),
    };
    sync_system_dependencies(&resolution.lockfile, install_system, install_only_system)?;
    if install_only_system {
        return Ok(SyncReport::default());
    }

    let mut required = resolution.packages;
    let (root_name, root_version) = root_package(&project.root, &project.description)?;
    required.remove(&root_name);

    match options.project_package {
        ProjectPackageMode::Install => {
            let root = Arc::new(
                LocalRepository::new(project.root.clone())
                    .with_description(project.description.clone()),
            );
            required.insert(
                root_name,
                (
                    PackageVersion::new(root_version, root),
                    Arc::new(project.description.clone()),
                ),
            );
        }
        ProjectPackageMode::Omit => {
            let base_packages = base_packages().await?;
            validate_project_omission(&root_name, &required, &base_packages)?;
        }
    }

    let project_library = project_library_path(&project.root);
    let installed = installed_packages(&project_library).await?;
    sync_packages(&project_library, required, installed, &resolution.r_version).await
}

async fn sync_packages(
    project_library: &Path,
    required: RequiredPackages,
    installed: BTreeMap<String, PackageVersion>,
    r_version: &semver::Version,
) -> Result<SyncReport, SyncError> {
    let installed_before = installed.keys().cloned().collect();
    let removed = installed
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
        .collect::<RequiredPackages>();
    let installed = packages_to_install.keys().cloned().collect();

    remove_packages_from_venv(project_library, &removed)?;
    install_required_packages(
        project_library,
        packages_to_install,
        retained.clone(),
        r_version,
    )
    .await?;

    Ok(SyncReport {
        installed_before,
        removed,
        installed,
        retained,
    })
}

pub(crate) fn package_requires_install(
    required: &PackageVersion,
    installed: Option<&PackageVersion>,
) -> bool {
    let repository = required.repository().as_ref();

    // Git and local sources can change without changing their package version.
    repository.downcast_ref::<GitRepository>().is_some()
        || repository.downcast_ref::<LocalRepository>().is_some()
        || installed != Some(required)
}

fn validate_project_omission(
    root_name: &str,
    required: &RequiredPackages,
    base_packages: &BTreeSet<String>,
) -> Result<(), SyncError> {
    if base_packages.contains(root_name) {
        return Ok(());
    }

    let dependents = required
        .iter()
        .filter_map(|(name, (_, description))| {
            package_dependency_names(description)
                .map(|dependencies| dependencies.contains(root_name).then(|| name.clone()))
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

    if dependents.is_empty() {
        Ok(())
    } else {
        Err(SyncError::CircularProjectDependency {
            project: root_name.to_string(),
            dependents: dependents.join(", "),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::built_in_repository;

    fn required_packages(packages: &[(&str, &str)]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, fields)| {
                let description =
                    RDescription::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}"));
                (
                    (*name).to_string(),
                    (
                        PackageVersion::new(
                            "1.0.0".parse().expect("version fixture should parse"),
                            built_in_repository(),
                        ),
                        Arc::new(description),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn rejects_omitting_project_required_by_locked_package() {
        let packages = required_packages(&[("helper", "Imports: project\n")]);

        let error = validate_project_omission("project", &packages, &BTreeSet::new())
            .expect_err("reverse project dependency should fail");

        assert!(matches!(
            error,
            SyncError::CircularProjectDependency {
                project,
                dependents
            } if project == "project" && dependents == "helper"
        ));
    }

    #[test]
    fn permits_omitting_project_with_base_package_name() {
        let packages = required_packages(&[("helper", "Imports: stats\n")]);

        validate_project_omission("stats", &packages, &BTreeSet::from(["stats".to_string()]))
            .expect("base package dependency should not reference the project");
    }
}

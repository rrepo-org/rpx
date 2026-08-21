use crate::{
    LockError,
    description::{DescriptionParseError, root_package},
    host_supports_system_sync,
    output::status,
    project::{
        ProjectLoadError, ResolutionPolicy, load_project, project_library_path, resolve_project,
    },
    r::{BasePackagesError, base_packages, installed_packages},
    system_plan_from_lockfile,
};
use miette::Diagnostic;
use r_description::Version;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug)]
struct PackageVersionMismatch {
    package: String,
    installed: Version,
    expected: Version,
}

#[derive(Debug, Default)]
pub(crate) struct StatusMismatches {
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
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] crate::r::InstalledPackagesError),

    #[error("project is out of sync\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::status::out_of_sync),
        help("Run `rpx sync` to synchronize the project.")
    )]
    OutOfSync { mismatches: StatusMismatches },
}

pub(crate) async fn run() -> Result<(), Error> {
    let project = load_project()?;
    let resolution = resolve_project(&project, ResolutionPolicy::RequireValid).await?;
    let lockfile = &resolution.lockfile;
    let base_packages = base_packages().await?;

    let mut expected_packages = lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let (root_name, root_version) = root_package(&project.root, &project.description)?;
    expected_packages.insert(root_name, root_version);

    let project_library = project_library_path(&project.root);
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
        system_plan_from_lockfile(lockfile).ok()
    } else {
        None
    };
    if let Some(plan) = system_plan {
        mismatches.missing_system_packages = plan.missing_packages;
        mismatches.unsupported_system_rules = plan.unsupported_rules;
    }

    if !mismatches.is_empty() {
        return Err(Error::OutOfSync { mismatches });
    }

    status("Project is in sync");
    Ok(())
}

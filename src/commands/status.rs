use crate::{
    description::{DescriptionParseError, root_package},
    output::status,
    project::{
        LoadProjectResolutionError, ProjectLoadError, load_project, load_project_resolution,
        project_library_path,
    },
    r::{BasePackagesError, base_packages, installed_packages},
};
use miette::Diagnostic;
use r_description::Version;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, PartialEq, Eq)]
struct PackageVersionMismatch {
    package: String,
    installed: Version,
    expected: Version,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StatusMismatches {
    missing_packages: Vec<String>,
    extra_packages: Vec<String>,
    version_mismatches: Vec<PackageVersionMismatch>,
}

impl StatusMismatches {
    fn is_empty(&self) -> bool {
        self.missing_packages.is_empty()
            && self.extra_packages.is_empty()
            && self.version_mismatches.is_empty()
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
    LoadResolution(#[from] LoadProjectResolutionError),

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
    let resolution = load_project_resolution(&project).await?;
    let lockfile = &resolution.lockfile;
    let base_packages = base_packages().await?;

    let expected_packages = lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let (root_name, _) = root_package(&project.root, &project.description)?;

    let project_library = project_library_path(&project.root);
    let installed = installed_packages(&project_library).await?;
    let mismatches = status_mismatches(&expected_packages, &installed, &root_name);

    if !mismatches.is_empty() {
        return Err(Error::OutOfSync { mismatches });
    }

    status("Project is in sync");
    Ok(())
}

fn status_mismatches(
    expected_packages: &BTreeMap<String, Version>,
    installed: &BTreeMap<String, crate::resolver::PackageVersion>,
    root_name: &str,
) -> StatusMismatches {
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
            package.as_str() != root_name && !expected_packages.contains_key(*package)
        })
        .cloned()
        .collect();
    StatusMismatches {
        missing_packages,
        extra_packages,
        version_mismatches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repository::built_in_repository, resolver::PackageVersion};

    fn version(value: &str) -> Version {
        value.parse().expect("version should parse")
    }

    fn installed_version(value: &str) -> PackageVersion {
        PackageVersion::new(version(value), built_in_repository())
    }

    #[test]
    fn root_package_is_optional_for_status() {
        let expected = BTreeMap::new();
        let omitted = BTreeMap::new();
        let installed = BTreeMap::from([("project".to_string(), installed_version("1.0.0"))]);

        assert!(status_mismatches(&expected, &omitted, "project").is_empty());
        assert!(status_mismatches(&expected, &installed, "project").is_empty());
    }

    #[test]
    fn missing_locked_dependencies_are_still_reported() {
        let expected = BTreeMap::from([("digest".to_string(), version("0.6.39"))]);

        let mismatches = status_mismatches(&expected, &BTreeMap::new(), "project");

        assert_eq!(mismatches.missing_packages, ["digest"]);
        assert!(mismatches.extra_packages.is_empty());
        assert!(mismatches.version_mismatches.is_empty());
    }
}

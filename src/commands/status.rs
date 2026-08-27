use crate::{
    description::{DescriptionParseError, root_package},
    output::status,
    project::{
        LibraryMismatches, LoadProjectResolutionError, ProjectLoadError, library_mismatches,
        load_project, load_project_resolution, project_library_path,
    },
    r::{BasePackagesError, base_packages, installed_packages},
};
use miette::Diagnostic;
use std::collections::BTreeMap;
use thiserror::Error;

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
    OutOfSync { mismatches: LibraryMismatches },
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
    let mismatches = library_mismatches(&expected_packages, &installed, Some(&root_name));

    if !mismatches.is_exact() {
        return Err(Error::OutOfSync { mismatches });
    }

    status("Project is in sync");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repository::built_in_repository, resolver::PackageVersion};
    use r_description::Version;

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

        assert!(library_mismatches(&expected, &omitted, Some("project")).is_exact());
        assert!(library_mismatches(&expected, &installed, Some("project")).is_exact());
    }

    #[test]
    fn missing_locked_dependencies_are_still_reported() {
        let expected = BTreeMap::from([("digest".to_string(), version("0.6.39"))]);

        let mismatches = library_mismatches(&expected, &BTreeMap::new(), Some("project"));

        assert_eq!(mismatches.missing_packages, ["digest"]);
        assert!(mismatches.extra_packages.is_empty());
        assert!(mismatches.version_mismatches.is_empty());
    }

    #[test]
    fn extra_packages_are_runnable_but_not_exact() {
        let installed = BTreeMap::from([("extra".to_string(), installed_version("1.0.0"))]);

        let mismatches = library_mismatches(&BTreeMap::new(), &installed, None);

        assert!(mismatches.is_runnable());
        assert!(!mismatches.is_exact());
        assert!(mismatches.into_runtime_mismatches().is_exact());
    }
}

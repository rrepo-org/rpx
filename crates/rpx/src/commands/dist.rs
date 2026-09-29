use crate::{
    cli::{DistBuildArgs, DistCommands},
    description::{DescriptionParseError, ProjectType, project_type, root_package},
    output::status,
    project::{
        LibraryMismatches, LoadProjectResolutionError, ProjectLoadError, library_mismatches,
        load_project, load_project_resolution, project_library_path,
    },
    r::{BasePackagesError, InstalledPackagesError, base_packages, installed_packages},
};
use miette::Diagnostic;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error("distribution requires an installable R package")]
    #[diagnostic(code(rpx::dist::not_a_package))]
    NotAPackage,

    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LoadResolution(#[from] LoadProjectResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),

    #[error("project library has not been synced for distribution")]
    #[diagnostic(
        code(rpx::dist::library_out_of_sync),
        help("Run `rpx sync` before `rpx dist build`.")
    )]
    LibraryMissing,

    #[error("project library is not synced for distribution\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::dist::library_out_of_sync),
        help("Run `rpx sync` before `rpx dist build`.")
    )]
    LibraryOutOfSync { mismatches: LibraryMismatches },

    #[error("failed to build source package: {source}")]
    #[diagnostic(code(rpx::dist::build_failed))]
    Build {
        #[source]
        source: rpx_dist::BuildError,
    },
}

pub(crate) async fn run(command: DistCommands) -> Result<(), Error> {
    match command {
        DistCommands::Build(args) => build(args).await,
    }
}

async fn build(args: DistBuildArgs) -> Result<(), Error> {
    let project = load_project()?;
    if project_type(&project.description) != ProjectType::Package {
        return Err(Error::NotAPackage);
    }
    let (package, version) = root_package(&project.root, &project.description)?;
    let resolution = load_project_resolution(&project).await?;
    let base_packages = base_packages().await?;
    let expected = resolution
        .lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let project_library = project_library_path(&project.root);
    if !project_library.is_dir() {
        return Err(Error::LibraryMissing);
    }
    let installed = installed_packages(&project_library).await?;
    let mismatches = library_mismatches(&expected, &installed, Some(&package));
    if !mismatches.is_exact() {
        return Err(Error::LibraryOutOfSync { mismatches });
    }
    let output_dir = args.output_dir.unwrap_or_else(|| project.root.join("dist"));
    let artifact = rpx_dist::build(rpx_dist::BuildRequest {
        package_root: project.root,
        package,
        version,
        output_dir,
        project_library,
    })
    .await
    .map_err(|source| Error::Build { source })?;
    status(format_args!(
        "Built {} {} at {}",
        artifact.package,
        artifact.version,
        artifact.path.display()
    ));
    Ok(())
}

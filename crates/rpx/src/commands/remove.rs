use crate::{
    cli::RemoveArgs,
    description::{
        DependencyMutationError, DescriptionNormalizationError, normalize_description,
        remove_dependencies,
    },
    output::status,
    project::{
        ProjectLoadError, ProjectWriteError, ResolutionPolicy, ResolveProjectError, load_project,
        project_library_path, resolve_project, write_project_files,
    },
    r::{InstalledPackagesError, installed_packages},
    sync::{SyncError, sync_resolved_project},
};
use miette::Diagnostic;
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DependencyMutation(#[from] DependencyMutationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionNormalization(#[from] DescriptionNormalizationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Resolve(#[from] ResolveProjectError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectWrite(#[from] ProjectWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

pub(crate) async fn run(args: RemoveArgs) -> Result<(), Error> {
    let mut project = load_project()?;
    let installed = installed_packages(&project_library_path(&project.root)).await?;
    let removed_packages = args.packages.iter().cloned().collect::<BTreeSet<_>>();
    remove_dependencies(&project.root, &mut project.description, &removed_packages)?;
    project.description = normalize_description(&project.root, &project.description)?;
    let resolution = resolve_project(&project, ResolutionPolicy::ReuseIfValid).await?;
    write_project_files(
        &project.root,
        Some(&project.description),
        &resolution.lockfile,
    )?;
    sync_resolved_project(&project, resolution, args.no_install_project.into()).await?;
    let removed = args
        .packages
        .iter()
        .filter(|package| installed.contains_key(package.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = args
        .packages
        .iter()
        .filter(|package| !installed.contains_key(package.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();

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

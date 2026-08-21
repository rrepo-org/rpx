use crate::{
    cli::RemoveArgs,
    description::{DescriptionParseError, remove_dependencies},
    output::status,
    project::{
        ProjectLoadError, ProjectWriteError, ResolutionPolicy, ResolveProjectError, load_project,
        resolve_project, write_project_files,
    },
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
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Resolve(#[from] ResolveProjectError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectWrite(#[from] ProjectWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

pub(crate) async fn run(args: RemoveArgs) -> Result<(), Error> {
    let mut project = load_project()?;
    let removed_packages = args.packages.iter().cloned().collect::<BTreeSet<_>>();
    remove_dependencies(&project.root, &mut project.description, &removed_packages)?;
    let resolution = resolve_project(&project, ResolutionPolicy::ReuseIfValid).await?;
    write_project_files(
        &project.root,
        Some(&project.description),
        &resolution.lockfile,
    )?;
    let report =
        sync_resolved_project(&project, resolution, args.no_install_project.into()).await?;
    let removed = args
        .packages
        .iter()
        .filter(|package| report.installed_before.contains(package.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = args
        .packages
        .iter()
        .filter(|package| !report.installed_before.contains(package.as_str()))
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

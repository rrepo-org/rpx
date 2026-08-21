use crate::{
    cli::SyncArgs,
    output::status,
    project::{
        LoadProjectResolutionError, ProjectLoadError, load_project, load_project_resolution,
    },
    sync::{SyncError, sync_resolved_project},
};
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LoadResolution(#[from] LoadProjectResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Sync(#[from] SyncError),
}

pub(crate) async fn run(args: SyncArgs) -> Result<(), Error> {
    let project = load_project()?;
    let resolution = load_project_resolution(&project).await?;
    sync_resolved_project(&project, resolution, args.no_install_project.into()).await?;
    status("Synchronized project library");
    Ok(())
}

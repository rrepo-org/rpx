use crate::{
    LockError, SyncError,
    output::status,
    project::{ProjectLoadError, load_project, load_project_resolution},
    sync::{ProjectPackageMode, SyncProjectOptions, SystemSyncMode, sync_resolved_project},
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
    Lock(#[from] LockError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Sync(#[from] SyncError),
}

pub(crate) async fn run(
    no_install_project: bool,
    install_system: bool,
    install_only_system: bool,
) -> Result<(), Error> {
    let project = load_project()?;
    let resolution = load_project_resolution(&project).await?;
    sync_resolved_project(
        &project,
        resolution,
        SyncProjectOptions {
            project_package: if no_install_project {
                ProjectPackageMode::Omit
            } else {
                ProjectPackageMode::Install
            },
            system: if install_only_system {
                SystemSyncMode::InstallOnly
            } else if install_system {
                SystemSyncMode::Install
            } else {
                SystemSyncMode::Check
            },
        },
    )
    .await?;
    status("Synchronized project library");
    Ok(())
}

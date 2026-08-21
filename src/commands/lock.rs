use crate::{
    LockError,
    output::status,
    project::{
        ProjectLoadError, ProjectWriteError, ResolutionPolicy, load_project, resolve_project,
        write_project_lockfile,
    },
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
    ProjectWrite(#[from] ProjectWriteError),
}

pub(crate) async fn run() -> Result<(), Error> {
    let project = load_project()?;
    let resolution = resolve_project(&project, ResolutionPolicy::AlwaysResolve).await?;
    let changed = resolution.lockfile_changed;
    write_project_lockfile(&project, &resolution)?;

    if changed {
        status("Updated rpx.lock");
    } else {
        status("rpx.lock is already up to date");
    }
    Ok(())
}

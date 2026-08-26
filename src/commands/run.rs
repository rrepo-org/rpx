use crate::{
    cli::RunArgs,
    project::{
        ProjectDiscoveryError, ProjectLibraryError, ensure_project_library, find_project_root,
    },
    r::RVirtualEnv,
};
use miette::Diagnostic;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLibrary(#[from] ProjectLibraryError),

    #[error("command not found: {program}")]
    #[diagnostic(code(rpx::run::command_not_found))]
    CommandNotFound { program: String },

    #[error("failed to run {program}")]
    #[diagnostic(code(rpx::run::command_failed))]
    CommandFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) async fn run(args: RunArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let (program, command_args) = args
        .command
        .split_first()
        .expect("run command requires at least one argument");
    let project_library = ensure_project_library(&project_path)?;

    let status = Command::with_venv(program, &project_library)
        .args(command_args)
        .status()
        .await
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::CommandNotFound {
                    program: program.clone(),
                }
            } else {
                Error::CommandFailed {
                    program: program.clone(),
                    source,
                }
            }
        })?;

    exit_with_status(status.code());
    Ok(())
}

fn exit_with_status(code: Option<i32>) {
    if code != Some(0) {
        std::process::exit(code.unwrap_or(1));
    }
}

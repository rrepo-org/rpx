use crate::r::RVirtualEnv;
use miette::Diagnostic;
use std::{ffi::OsString, path::Path};
use thiserror::Error;

use super::RECURSION_DEPTH_ENV;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error("command not found: {program}")]
    #[diagnostic(
        code(rpx::run::command_not_found),
        help(
            "Check that the executable is on PATH and that script interpreters exist. Shell commands must be invoked explicitly, for example `rpx run sh -c 'command'`."
        )
    )]
    CommandNotFound { program: String },

    #[error("failed to start {program}")]
    #[diagnostic(code(rpx::run::command_start_failed))]
    CommandStartFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed while waiting for {program}")]
    #[diagnostic(code(rpx::run::command_wait_failed))]
    CommandWaitFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

pub(super) fn execute_command(
    program: &OsString,
    args: &[OsString],
    project_library: &Path,
    depth: u32,
) -> Result<(), Error> {
    let display_program = program.to_string_lossy().into_owned();
    let mut command = std::process::Command::with_venv(program, project_library);
    command
        .args(args)
        .env(RECURSION_DEPTH_ENV, depth.to_string());
    let mut child = command
        .spawn()
        .map_err(|source| classify_start_error(program, source))?;
    let status = child.wait().map_err(|source| Error::CommandWaitFailed {
        program: display_program,
        source,
    })?;
    if status.code() != Some(0) {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn classify_start_error(program: &OsString, source: std::io::Error) -> Error {
    let program = program.to_string_lossy().into_owned();
    if source.kind() == std::io::ErrorKind::NotFound {
        Error::CommandNotFound { program }
    } else {
        Error::CommandStartFailed { program, source }
    }
}

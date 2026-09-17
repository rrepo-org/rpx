use miette::Diagnostic;
use std::{ffi::OsString, os::unix::process::CommandExt, path::Path};
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
}

pub(super) fn execute_command(
    program: &OsString,
    args: &[OsString],
    project_library: &Path,
    depth: u32,
) -> Result<(), Error> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env("R_LIBS_USER", project_library)
        .env(RECURSION_DEPTH_ENV, depth.to_string());
    let source = command.exec();
    Err(classify_start_error(program, source))
}

fn classify_start_error(program: &OsString, source: std::io::Error) -> Error {
    let program = program.to_string_lossy().into_owned();
    if source.kind() == std::io::ErrorKind::NotFound {
        Error::CommandNotFound { program }
    } else {
        Error::CommandStartFailed { program, source }
    }
}

use crate::{
    cli::RunArgs,
    project::{
        LibraryMismatches, LoadProjectResolutionError, ProjectLibraryError, ProjectLoadError,
        ensure_project_library, library_mismatches, load_project, load_project_resolution,
        project_library_path,
    },
    r::{
        BasePackagesError, InstalledPackagesError, RVirtualEnv, base_packages, installed_packages,
    },
};
use miette::Diagnostic;
use std::{collections::BTreeMap, ffi::OsString};
use thiserror::Error;

const RECURSION_DEPTH_ENV: &str = "RPX_RUN_RECURSION_DEPTH";
const MAX_RECURSION_DEPTH: u32 = 8;

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
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLibrary(#[from] ProjectLibraryError),

    #[error("project library is not ready to run commands\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::run::library_out_of_sync),
        help("Run `rpx sync` to install the locked package versions.")
    )]
    LibraryOutOfSync { mismatches: LibraryMismatches },

    #[error("invalid {RECURSION_DEPTH_ENV} value: {value}")]
    #[diagnostic(code(rpx::run::recursion_depth_invalid))]
    InvalidRecursionDepth { value: String },

    #[error("refusing to recursively invoke `rpx run` more than {MAX_RECURSION_DEPTH} times")]
    #[diagnostic(
        code(rpx::run::recursion_limit),
        help(
            "Check whether the command or its interpreter uses a shebang that invokes `rpx run`."
        )
    )]
    RecursionLimit,

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

    #[cfg(windows)]
    #[error("failed while waiting for {program}")]
    #[diagnostic(code(rpx::run::command_wait_failed))]
    CommandWaitFailed {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) async fn run(args: RunArgs) -> Result<(), Error> {
    let depth = recursion_depth()?;
    let project = load_project()?;
    let project_library = project_library_path(&project.root);

    let (resolution, base_packages, installed) = tokio::try_join!(
        async {
            load_project_resolution(&project)
                .await
                .map_err(Error::LoadResolution)
        },
        async { base_packages().await.map_err(Error::BasePackages) },
        async {
            installed_packages(&project_library)
                .await
                .map_err(Error::InstalledPackages)
        },
    )?;

    let expected = resolution
        .lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let mismatches = library_mismatches(&expected, &installed, None);
    if !mismatches.is_runnable() {
        return Err(Error::LibraryOutOfSync {
            mismatches: mismatches.into_runtime_mismatches(),
        });
    }

    let project_library = ensure_project_library(&project.root)?;
    let (program, command_args) = args
        .command
        .split_first()
        .expect("run command requires at least one argument");
    #[cfg(unix)]
    return execute_command(program, command_args, &project_library, depth + 1);

    #[cfg(windows)]
    execute_command(program, command_args, &project_library, depth + 1).await
}

fn recursion_depth() -> Result<u32, Error> {
    let value = match std::env::var(RECURSION_DEPTH_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(0),
        Err(std::env::VarError::NotUnicode(value)) => {
            return Err(Error::InvalidRecursionDepth {
                value: value.to_string_lossy().into_owned(),
            });
        }
    };
    let depth = value
        .parse::<u32>()
        .map_err(|_| Error::InvalidRecursionDepth { value })?;
    if depth > MAX_RECURSION_DEPTH {
        return Err(Error::RecursionLimit);
    }
    Ok(depth)
}

#[cfg(unix)]
fn execute_command(
    program: &OsString,
    args: &[OsString],
    project_library: &std::path::Path,
    depth: u32,
) -> Result<(), Error> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::with_venv(program, project_library);
    command
        .args(args)
        .env(RECURSION_DEPTH_ENV, depth.to_string());
    let source = command.exec();
    Err(classify_start_error(&program.to_string_lossy(), source))
}

#[cfg(windows)]
async fn execute_command(
    program: &OsString,
    args: &[OsString],
    project_library: &std::path::Path,
    depth: u32,
) -> Result<(), Error> {
    let display_program = program.to_string_lossy().into_owned();
    let mut command = tokio::process::Command::with_venv(program, project_library);
    command
        .args(args)
        .env(RECURSION_DEPTH_ENV, depth.to_string());
    let mut child = command
        .spawn()
        .map_err(|source| classify_start_error(&display_program, source))?;
    let status = child
        .wait()
        .await
        .map_err(|source| Error::CommandWaitFailed {
            program: display_program,
            source,
        })?;
    if status.code() != Some(0) {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn classify_start_error(program: &str, source: std::io::Error) -> Error {
    if source.kind() == std::io::ErrorKind::NotFound {
        Error::CommandNotFound {
            program: program.to_string(),
        }
    } else {
        Error::CommandStartFailed {
            program: program.to_string(),
            source,
        }
    }
}

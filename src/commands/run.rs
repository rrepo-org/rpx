use crate::{
    cli::RunArgs,
    lockfile::{LOCKFILE_NAME, LockfileReadError, read_lockfile},
    project::{
        LibraryMismatches, LockedResolutionError, ProjectLibraryError, ProjectLoadError,
        ensure_project_library, library_mismatches, load_project, project_library_path,
        validate_runtime_resolution,
    },
    r::{
        BasePackagesError, InstalledPackagesError, RVersionError, RVirtualEnv, base_packages,
        installed_packages, r_version_async,
    },
};
use miette::Diagnostic;
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};
use thiserror::Error;

const RECURSION_DEPTH_ENV: &str = "RPX_RUN_RECURSION_DEPTH";
const MAX_RECURSION_DEPTH: u32 = 8;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error("{} is required to run commands in this project", path.display())]
    #[diagnostic(
        code(rpx::run::lockfile_required),
        help("Run `rpx lock`, then `rpx sync`, before running project commands.")
    )]
    LockfileRequired { path: PathBuf },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lockfile(#[from] LockfileReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LockedResolution(#[from] LockedResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    RVersion(#[from] RVersionError),

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

pub(crate) enum RunOutcome {
    #[cfg(unix)]
    Exec(PreparedCommand),
    #[cfg(windows)]
    Exit(Option<i32>),
}

#[cfg(unix)]
pub(crate) struct PreparedCommand {
    command: std::process::Command,
    program: String,
}

pub(crate) async fn run(args: RunArgs) -> Result<RunOutcome, Error> {
    let depth = recursion_depth()?;
    let project = load_project()?;
    let lockfile = required_lockfile(&project.root)?;
    let project_library = project_library_path(&project.root);

    let (r_version, base_packages, installed) = tokio::try_join!(
        async { r_version_async().await.map_err(Error::RVersion) },
        async { base_packages().await.map_err(Error::BasePackages) },
        async {
            installed_packages(&project_library)
                .await
                .map_err(Error::InstalledPackages)
        },
    )?;
    validate_runtime_resolution(&project.root, &project.description, &r_version, &lockfile)?;

    let expected = lockfile
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
    return prepare_command(program, command_args, &project_library, depth + 1);

    #[cfg(windows)]
    prepare_command(program, command_args, &project_library, depth + 1).await
}

fn required_lockfile(project_path: &PathBuf) -> Result<crate::lockfile::Lockfile, Error> {
    match read_lockfile(project_path) {
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(Error::LockfileRequired {
                path: project_path.join(LOCKFILE_NAME),
            })
        }
        result => result.map_err(Error::Lockfile),
    }
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
fn prepare_command(
    program: &OsString,
    args: &[OsString],
    project_library: &std::path::Path,
    depth: u32,
) -> Result<RunOutcome, Error> {
    let mut command = std::process::Command::with_venv(program, project_library);
    command
        .args(args)
        .env(RECURSION_DEPTH_ENV, depth.to_string());
    Ok(RunOutcome::Exec(PreparedCommand {
        command,
        program: program.to_string_lossy().into_owned(),
    }))
}

#[cfg(windows)]
async fn prepare_command(
    program: &OsString,
    args: &[OsString],
    project_library: &std::path::Path,
    depth: u32,
) -> Result<RunOutcome, Error> {
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
    Ok(RunOutcome::Exit(status.code()))
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

#[cfg(unix)]
pub(crate) fn exec(prepared: PreparedCommand) -> Result<(), Error> {
    use std::os::unix::process::CommandExt;

    let mut prepared = prepared;
    let source = prepared.command.exec();
    Err(classify_start_error(&prepared.program, source))
}

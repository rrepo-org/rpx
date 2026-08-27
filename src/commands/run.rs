use crate::{
    cli::RunArgs,
    project::{
        LibraryMismatches, LoadProjectResolutionError, ProjectLibraryError, ProjectLoadError,
        ensure_project_library, library_mismatches, load_project, load_project_resolution,
        project_library_path,
    },
    r::{BasePackagesError, InstalledPackagesError, base_packages, installed_packages},
};
use miette::Diagnostic;
use std::collections::BTreeMap;
use thiserror::Error;

#[cfg_attr(unix, path = "run/unix.rs")]
#[cfg_attr(windows, path = "run/windows.rs")]
mod platform;

const RECURSION_DEPTH_ENV: &str = "RPX_RUN_RECURSION_DEPTH";
const MAX_RECURSION_DEPTH: u32 = 100;

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

    #[error(transparent)]
    #[diagnostic(transparent)]
    Command(#[from] platform::Error),
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
    platform::execute_command(program, command_args, &project_library, depth + 1)?;
    Ok(())
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

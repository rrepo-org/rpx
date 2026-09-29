//! Build R source-package archives for distribution.

use r_metadata::Version;
use std::{io, path::PathBuf};
use tempfile::TempPath;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug)]
pub struct BuildRequest {
    pub package_root: PathBuf,
    pub package: String,
    pub version: Version,
    pub output_dir: PathBuf,
    pub project_library: PathBuf,
}

#[derive(Debug)]
pub struct BuiltArtifact {
    pub package: String,
    pub version: Version,
    pub path: PathBuf,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("failed to create output directory at {}: {source}", path.display())]
    OutputDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect package directory at {}: {source}", path.display())]
    PackageDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("output directory {} is inside the package being built", path.display())]
    OutputInsidePackage { path: PathBuf },
    #[error("failed to read {}: {source}", path.display())]
    BuildIgnore {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("output directory {} requires a `^dist$` rule in {}", path.display(), ignore.display())]
    OutputNotIgnored { path: PathBuf, ignore: PathBuf },
    #[error("failed to create temporary build directory in {}: {source}", path.display())]
    TemporaryDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to run R CMD build: {source}")]
    Start {
        #[source]
        source: io::Error,
    },
    #[error("R CMD build failed (exit code {exit_code:?})\nstdout:\n{stdout}\nstderr:\n{stderr}")]
    Command {
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    #[error("R CMD build did not produce the expected archive at {}", path.display())]
    ArchiveMissing { path: PathBuf },
    #[error("failed to inspect built archive at {}: {source}", path.display())]
    InspectArchive {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to publish built archive at {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Build a source package and publish the completed archive to `output_dir`.
pub async fn build(request: BuildRequest) -> Result<BuiltArtifact, BuildError> {
    tokio::fs::create_dir_all(&request.output_dir)
        .await
        .map_err(|source| BuildError::OutputDirectory {
            path: request.output_dir.clone(),
            source,
        })?;
    let package_root = tokio::fs::canonicalize(&request.package_root)
        .await
        .map_err(|source| BuildError::PackageDirectory {
            path: request.package_root.clone(),
            source,
        })?;
    let output_dir = tokio::fs::canonicalize(&request.output_dir)
        .await
        .map_err(|source| BuildError::OutputDirectory {
            path: request.output_dir.clone(),
            source,
        })?;
    let staging_parent = if let Ok(relative) = output_dir.strip_prefix(&package_root) {
        if !relative.starts_with("dist") {
            return Err(BuildError::OutputInsidePackage { path: output_dir });
        }
        let ignore = package_root.join(".Rbuildignore");
        let rules = match tokio::fs::read_to_string(&ignore).await {
            Ok(rules) => rules,
            Err(source) if source.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(BuildError::BuildIgnore {
                    path: ignore,
                    source,
                });
            }
        };
        if !rules.lines().any(|line| line.trim() == "^dist$") {
            return Err(BuildError::OutputNotIgnored {
                path: output_dir,
                ignore,
            });
        }
        // A sibling workspace cannot be swept up by R CMD build, even when
        // dist/ already holds an archive from an earlier invocation.
        package_root
            .parent()
            .ok_or_else(|| BuildError::OutputInsidePackage {
                path: output_dir.clone(),
            })?
    } else {
        &output_dir
    };

    let workspace = tempfile::Builder::new()
        .prefix(".rpx-dist-")
        .tempdir_in(staging_parent)
        .map_err(|source| BuildError::TemporaryDirectory {
            path: staging_parent.to_path_buf(),
            source,
        })?;
    let filename = format!("{}_{}.tar.gz", request.package, request.version);
    let staged = workspace.path().join(&filename);
    let destination = output_dir.join(filename);

    let mut command = Command::new("R");
    command
        .arg("CMD")
        .arg("build")
        .arg(&package_root)
        .current_dir(workspace.path())
        .env("R_LIBS", &request.project_library)
        .env("R_LIBS_USER", &request.project_library)
        .env("R_LIBS_SITE", &request.project_library)
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .map_err(|source| BuildError::Start { source })?;
    if !output.status.success() {
        return Err(BuildError::Command {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    match tokio::fs::metadata(&staged).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(BuildError::ArchiveMissing { path: staged }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(BuildError::ArchiveMissing { path: staged });
        }
        Err(source) => {
            return Err(BuildError::InspectArchive {
                path: staged,
                source,
            });
        }
    }

    TempPath::try_from_path(staged)
        .and_then(|temporary| temporary.persist(&destination).map_err(|error| error.error))
        .map_err(|source| BuildError::Publish {
            path: destination.clone(),
            source,
        })?;

    Ok(BuiltArtifact {
        package: request.package,
        version: request.version,
        path: destination,
    })
}

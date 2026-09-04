use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Output,
};

use miette::Diagnostic;
use r_metadata::Version;
use r_package_installer::{LibraryEntry, scan_library};
use thiserror::Error;
use tokio::{process::Command, sync::OnceCell};

use crate::{repository::built_in_repository, resolver::PackageVersion};

#[derive(Debug, Error, Diagnostic)]
pub enum RSubprocessError {
    #[error("failed to start {program}: {source}")]
    #[diagnostic(code(rpx::r::start_failed))]
    Start {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "{program} exited unsuccessfully with code {exit_code:?}: {summary}\n\nstdout:\n{stdout}\n\nstderr:\n{stderr}"
    )]
    #[diagnostic(code(rpx::r::command_failed))]
    Failed {
        program: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        summary: String,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum PackageBuildError {
    #[error("failed to prepare package artifact directory at {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::artifact_directory_failed))]
    ArtifactDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create temporary package build directory in {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::temporary_directory_failed))]
    TemporaryDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to build package at {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::command_failed))]
    Command {
        path: PathBuf,
        #[source]
        source: Box<RSubprocessError>,
    },

    #[error("R CMD build did not create the expected source archive at {}", path.display())]
    #[diagnostic(code(rpx::build::archive_missing))]
    ArchiveMissing { path: PathBuf },

    #[error("failed to inspect source archive at {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::archive_inspection_failed))]
    InspectArchive {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to publish source archive at {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::archive_publication_failed))]
    PublishArchive {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to clean temporary package build directory at {}: {source}", path.display())]
    #[diagnostic(code(rpx::build::temporary_directory_cleanup_failed))]
    Cleanup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum InstalledPackagesError {
    #[error("failed to inspect project library: {source}")]
    #[diagnostic(code(rpx::runtime::project_library_inspection_failed))]
    Scan {
        #[source]
        source: r_package_installer::Error,
    },

    #[error("failed to join project library inspection: {source}")]
    #[diagnostic(code(rpx::runtime::project_library_inspection_join_failed))]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("invalid installed version {version} for {package}: {details}")]
    #[diagnostic(code(rpx::runtime::installed_package_invalid_version))]
    InvalidVersion {
        package: String,
        version: String,
        details: String,
    },

    #[error("project library contains an active or interrupted package transaction at {}", path.display())]
    #[diagnostic(
        code(rpx::runtime::project_library_locked),
        help(
            "Finish the other package operation or remove the stale lock after confirming its owner is no longer running."
        )
    )]
    Locked { path: PathBuf },
}

#[derive(Debug, Error, Diagnostic)]
pub enum BasePackagesError {
    #[error("failed to inspect base R packages: {source}")]
    #[diagnostic(code(rpx::runtime::base_packages_failed))]
    Command {
        #[source]
        source: RSubprocessError,
    },

    #[error("base package output is not valid UTF-8: {source}")]
    #[diagnostic(code(rpx::runtime::base_packages_invalid_utf8))]
    InvalidUtf8 {
        #[source]
        source: std::string::FromUtf8Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum RVersionError {
    #[error("failed to inspect the R version: {source}")]
    #[diagnostic(code(rpx::runtime::version_failed))]
    Command {
        #[source]
        source: RSubprocessError,
    },

    #[error("R version output is not valid UTF-8: {source}")]
    #[diagnostic(code(rpx::runtime::version_invalid_utf8))]
    InvalidUtf8 {
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("R reported invalid version {version}: {source}")]
    InvalidVersion {
        version: String,
        #[source]
        source: semver::Error,
    },
}

static BASE_PACKAGES: OnceCell<BTreeSet<String>> = OnceCell::const_new();
static R_VERSION: OnceCell<semver::Version> = OnceCell::const_new();

pub async fn build_package_archive(
    package_root: &Path,
    package: &str,
    version: &str,
    archive: &Path,
) -> Result<(), PackageBuildError> {
    let artifact_directory = archive
        .parent()
        .expect("source artifact path should have a parent");
    tokio::fs::create_dir_all(artifact_directory)
        .await
        .map_err(|source| PackageBuildError::ArtifactDirectory {
            path: artifact_directory.to_path_buf(),
            source,
        })?;
    let workspace = tempfile::Builder::new()
        .prefix(".rpx-build-")
        .tempdir_in(artifact_directory)
        .map_err(|source| PackageBuildError::TemporaryDirectory {
            path: artifact_directory.to_path_buf(),
            source,
        })?;
    let workspace_path = workspace.path().to_path_buf();
    let staged_archive = workspace.path().join(format!("{package}_{version}.tar.gz"));

    let result = async {
        let mut build = Command::new("R");
        build
            .arg("CMD")
            .arg("build")
            .arg("--no-build-vignettes")
            .arg("--no-manual")
            .arg("--no-resave-data")
            .arg(package_root)
            .current_dir(&workspace_path);
        build.kill_on_drop(true);
        run_subprocess(build)
            .await
            .map_err(|source| PackageBuildError::Command {
                path: package_root.to_path_buf(),
                source: Box::new(source),
            })?;

        match tokio::fs::metadata(&staged_archive).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(PackageBuildError::ArchiveMissing {
                    path: staged_archive.clone(),
                });
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(PackageBuildError::ArchiveMissing {
                    path: staged_archive.clone(),
                });
            }
            Err(source) => {
                return Err(PackageBuildError::InspectArchive {
                    path: staged_archive.clone(),
                    source,
                });
            }
        }

        tempfile::TempPath::try_from_path(staged_archive.clone())
            .and_then(|temporary| temporary.persist(archive).map_err(|error| error.error))
            .map_err(|source| PackageBuildError::PublishArchive {
                path: archive.to_path_buf(),
                source,
            })?;

        Ok(())
    }
    .await;

    let cleanup = tokio::task::spawn_blocking(move || workspace.close())
        .await
        .map_err(std::io::Error::other)
        .and_then(|result| result);
    if let Err(error) = &cleanup
        && result.is_err()
    {
        tracing::warn!(%error, path = %workspace_path.display(), "failed to clean temporary package build directory");
    }
    result?;
    cleanup.map_err(|source| PackageBuildError::Cleanup {
        path: workspace_path,
        source,
    })
}

pub async fn base_packages() -> Result<BTreeSet<String>, BasePackagesError> {
    #[cfg(test)]
    let packages = BASE_PACKAGES
        .get_or_init(|| async {
            [
                "base",
                "compiler",
                "datasets",
                "graphics",
                "grDevices",
                "grid",
                "methods",
                "parallel",
                "splines",
                "stats",
                "stats4",
                "tcltk",
                "tools",
                "utils",
                "testBasePackage",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect()
        })
        .await;

    #[cfg(not(test))]
    let packages = BASE_PACKAGES.get_or_try_init(fetch_base_packages).await?;

    Ok(packages.clone())
}

pub async fn installed_packages(
    project_library: &Path,
) -> Result<BTreeMap<String, PackageVersion>, InstalledPackagesError> {
    let project_library = project_library.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || scan_library(&project_library))
        .await
        .map_err(|source| InstalledPackagesError::Join { source })?
        .map_err(|source| InstalledPackagesError::Scan { source })?;
    let repository = built_in_repository();
    let mut installed = BTreeMap::new();
    for entry in entries {
        match entry {
            LibraryEntry::Installed(package) => {
                let version = package
                    .metadata
                    .version
                    .parse::<Version>()
                    .map_err(|source| InstalledPackagesError::InvalidVersion {
                        package: package.metadata.name.clone(),
                        version: package.metadata.version.clone(),
                        details: source.to_string(),
                    })?;
                installed.insert(
                    package.metadata.name,
                    PackageVersion::new(version, repository.clone()),
                );
            }
            LibraryEntry::Locked(lock) => {
                return Err(InstalledPackagesError::Locked { path: lock.path });
            }
            LibraryEntry::Recoverable(plan) => {
                return Err(InstalledPackagesError::Locked {
                    path: plan.lock.path,
                });
            }
            LibraryEntry::Incomplete { path, reason } => {
                tracing::debug!(path = %path.display(), %reason, "found incomplete project library entry");
            }
            LibraryEntry::Foreign(path) => {
                tracing::debug!(path = %path.display(), "ignoring foreign project library entry");
            }
        }
    }
    Ok(installed)
}

pub async fn r_version_async() -> Result<semver::Version, RVersionError> {
    R_VERSION.get_or_try_init(fetch_r_version).await.cloned()
}

async fn fetch_r_version() -> Result<semver::Version, RVersionError> {
    let mut command = Command::new("Rscript");
    command.arg("--version");
    let output = run_subprocess(command)
        .await
        .map_err(|source| RVersionError::Command { source })?;

    let output =
        String::from_utf8(output.stdout).map_err(|source| RVersionError::InvalidUtf8 { source })?;
    let output = output.trim();
    let version = output
        .strip_prefix("Rscript (R) version ")
        .and_then(|remainder| remainder.split_whitespace().next())
        .unwrap_or(output);

    version
        .parse()
        .map_err(|source| RVersionError::InvalidVersion {
            version: version.to_string(),
            source,
        })
}

async fn run_subprocess(mut command: Command) -> Result<Output, RSubprocessError> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let output = command
        .output()
        .await
        .map_err(|source| RSubprocessError::Start {
            program: program.clone(),
            source,
        })?;
    if output.status.success() {
        return Ok(output);
    }

    Err(RSubprocessError::Failed {
        program,
        exit_code: output.status.code(),
        summary: summarize_subprocess_output(&output.stdout, &output.stderr),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn fetch_base_packages() -> Result<BTreeSet<String>, BasePackagesError> {
    let mut command = Command::new("Rscript");
    command
        .arg("--vanilla")
        .arg("-e")
        .arg("writeLines(rownames(utils::installed.packages(priority = 'base')))");
    let output = run_subprocess(command)
        .await
        .map_err(|source| BasePackagesError::Command { source })?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|source| BasePackagesError::InvalidUtf8 { source })?;

    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn summarize_subprocess_output(stdout: &[u8], stderr: &[u8]) -> String {
    let combined = [
        String::from_utf8_lossy(stderr),
        String::from_utf8_lossy(stdout),
    ]
    .join("\n");
    let lines = combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    lines
        .iter()
        .rev()
        .find(|line| {
            line.contains("ERROR")
                || line.contains("error:")
                || line.contains("installation of package")
                || line.contains("failed")
        })
        .copied()
        .or_else(|| lines.last().copied())
        .unwrap_or("process exited without output")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[tokio::test]
    async fn scans_installed_packages_with_builtin_repository() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("digest");
        fs::create_dir_all(package.join("Meta")).unwrap();
        fs::write(
            package.join("DESCRIPTION"),
            "Package: digest\nVersion: 0.6.37\nBuilt: R 4.5.1; x86_64-pc-linux-gnu; 2026-01-01; unix\n",
        )
        .unwrap();
        fs::write(package.join("Meta/package.rds"), "metadata").unwrap();

        let packages = installed_packages(directory.path()).await.unwrap();
        let digest = packages.get("digest").unwrap();

        assert_eq!(digest.version().to_string(), "0.6.37");
        assert!(digest.repository().equals(built_in_repository().as_ref()));
    }

    #[tokio::test]
    async fn missing_project_library_has_no_installed_packages() {
        let directory = tempfile::tempdir().expect("temporary directory should exist");
        let path = directory.path().join("missing-library");

        let packages = installed_packages(&path)
            .await
            .expect("missing library should be empty");

        assert!(packages.is_empty());
    }

    #[tokio::test]
    async fn project_library_must_be_a_directory() {
        let directory = tempfile::tempdir().expect("temporary directory should exist");
        let path = directory.path().join("library");
        fs::write(&path, "not a directory").expect("test file should be written");

        let error = installed_packages(&path)
            .await
            .expect_err("file should not be accepted as a library");

        assert!(matches!(error, InstalledPackagesError::Scan { .. }));
    }
}

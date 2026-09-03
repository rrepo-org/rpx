use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Output,
};

use miette::Diagnostic;
use r_metadata::Version;
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
pub enum PackageInstallError {
    #[error("failed to {action} {target}: {source}")]
    #[diagnostic(code(rpx::install::command_failed))]
    Command {
        action: &'static str,
        target: String,
        #[source]
        source: Box<RSubprocessError>,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum InstalledPackagesError {
    #[error("failed to inspect project library at {}: {source}", path.display())]
    #[diagnostic(code(rpx::runtime::project_library_inspection_failed))]
    LibraryMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("project library path is not a directory: {}", path.display())]
    #[diagnostic(code(rpx::runtime::project_library_not_directory))]
    LibraryNotDirectory { path: PathBuf },

    #[error("failed to inspect installed R packages: {source}")]
    #[diagnostic(code(rpx::runtime::installed_packages_failed))]
    Command {
        #[source]
        source: RSubprocessError,
    },

    #[error("installed package output is not valid UTF-8: {source}")]
    #[diagnostic(code(rpx::runtime::installed_packages_invalid_utf8))]
    InvalidUtf8 {
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("invalid installed package output on line {line}: {contents}")]
    #[diagnostic(code(rpx::runtime::installed_packages_invalid_row))]
    InvalidRow { line: usize, contents: String },

    #[error("invalid installed version {version} for {package} on line {line}: {details}")]
    #[diagnostic(code(rpx::runtime::installed_package_invalid_version))]
    InvalidVersion {
        line: usize,
        package: String,
        version: String,
        details: String,
    },
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

fn project_r_command(program: impl AsRef<OsStr>, project_library: &Path) -> Command {
    let mut command = Command::new(program.as_ref());
    command.env("R_LIBS_USER", project_library);
    command
}

static BASE_PACKAGES: OnceCell<BTreeSet<String>> = OnceCell::const_new();
static R_VERSION: OnceCell<semver::Version> = OnceCell::const_new();

pub async fn install_package_artifact(
    project_library: &Path,
    artifact_path: &Path,
    package: &str,
    version: &str,
    pkg_type: &str,
    r_version: &semver::Version,
    target_library: &Path,
) -> Result<Output, PackageInstallError> {
    let mut command = project_r_command("Rscript", project_library);
    command
        .arg("--vanilla")
        .arg("-e")
        .arg(concat!(
            "args <- commandArgs(trailingOnly = TRUE);",
            "artifact <- args[[1L]];",
            "package_type <- args[[2L]];",
            "target_library <- args[[3L]];",
            "package_name <- args[[4L]];",
            "expected_version <- args[[5L]];",
            "utils::install.packages(artifact, repos = NULL, type = package_type, lib = target_library);",
            "package_dir <- file.path(target_library, package_name);",
            "description <- file.path(package_dir, 'DESCRIPTION');",
            "if (!dir.exists(package_dir) || !file.exists(description)) ",
            "stop(sprintf('Expected package %s at %s after installation. Library entries: %s', package_name, package_dir, paste(list.files(target_library, all.files = TRUE), collapse = ', ')));",
            "metadata <- read.dcf(description, fields = c('Package', 'Version'));",
            "installed_name <- unname(metadata[1L, 'Package']);",
            "installed_version <- unname(metadata[1L, 'Version']);",
            "if (!identical(installed_name, package_name)) ",
            "stop(sprintf('Installed package name is %s, expected %s', installed_name, package_name));",
            "if (!identical(installed_version, expected_version)) ",
            "stop(sprintf('Installed %s version %s, expected %s', package_name, installed_version, expected_version))"
        ))
        .arg(artifact_path)
        .arg(pkg_type)
        .arg(target_library)
        .arg(package)
        .arg(version);
    command.kill_on_drop(true);
    run_subprocess(command)
        .await
        .map_err(|source| PackageInstallError::Command {
            action: "install",
            target: format!(
                "{package}@{version} from {} as {pkg_type} with R {r_version}",
                artifact_path.display()
            ),
            source: Box::new(source),
        })
}

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
    let metadata = match tokio::fs::metadata(project_library).await {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(source) => {
            return Err(InstalledPackagesError::LibraryMetadata {
                path: project_library.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.is_dir() {
        return Err(InstalledPackagesError::LibraryNotDirectory {
            path: project_library.to_path_buf(),
        });
    }

    let expression = concat!(
        "args <- commandArgs(trailingOnly = TRUE);",
        "packages <- utils::installed.packages(lib.loc = args[[1L]]);",
        "if (nrow(packages) == 0) quit(save = 'no', status = 0);",
        "utils::write.table(packages[, c('Package', 'Version'), drop = FALSE], ",
        "sep = '\t', row.names = FALSE, col.names = TRUE, quote = FALSE)"
    );

    let mut command = Command::new("Rscript");
    command
        .arg("--vanilla")
        .arg("-e")
        .arg(expression)
        .arg(project_library);
    let output = run_subprocess(command)
        .await
        .map_err(|source| InstalledPackagesError::Command { source })?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|source| InstalledPackagesError::InvalidUtf8 { source })?;

    parse_installed_packages(&stdout)
}

#[derive(Debug, Error, Diagnostic)]
pub enum PackageRemovalError {
    #[error("failed to remove package {package} at {}: {source}", path.display())]
    #[diagnostic(code(rpx::library::package_remove_failed))]
    Remove {
        package: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn remove_packages_from_venv(
    project_library: &Path,
    packages: &BTreeSet<String>,
) -> Result<(), PackageRemovalError> {
    packages
        .iter()
        .try_for_each(|package| remove_package_from_venv(project_library, package))
}

pub fn remove_package_from_venv(
    project_library: &Path,
    package: &str,
) -> Result<(), PackageRemovalError> {
    let package_dir = project_library.join(package);

    if !package_dir.exists() {
        return Ok(());
    }

    std::fs::remove_dir_all(&package_dir).map_err(|source| PackageRemovalError::Remove {
        package: package.to_string(),
        path: package_dir,
        source,
    })
}

fn parse_installed_packages(
    output: &str,
) -> Result<BTreeMap<String, PackageVersion>, InstalledPackagesError> {
    let mut lines = output.lines().enumerate();
    let Some((_, header)) = lines.next() else {
        return Ok(BTreeMap::new());
    };
    if header != "Package\tVersion" {
        return Err(InstalledPackagesError::InvalidRow {
            line: 1,
            contents: header.to_string(),
        });
    }

    let repository = built_in_repository();
    lines
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let mut parts = line.split('\t');
            let (Some(package), Some(version), None) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(InstalledPackagesError::InvalidRow {
                    line: index + 1,
                    contents: line.to_string(),
                });
            };
            let package = package.trim();
            let version = version.trim();
            if package.is_empty() || version.is_empty() {
                return Err(InstalledPackagesError::InvalidRow {
                    line: index + 1,
                    contents: line.to_string(),
                });
            }

            let parsed_version = version.parse::<Version>().map_err(|source| {
                InstalledPackagesError::InvalidVersion {
                    line: index + 1,
                    package: package.to_string(),
                    version: version.to_string(),
                    details: source.to_string(),
                }
            })?;

            Ok((
                package.to_string(),
                PackageVersion::new(parsed_version, repository.clone()),
            ))
        })
        .collect()
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
    use std::{fs, sync::Arc};

    use super::*;

    #[test]
    fn parses_installed_packages_with_builtin_repository() {
        let packages = parse_installed_packages("Package\tVersion\ndigest\t0.6.37\n").unwrap();
        let digest = packages.get("digest").unwrap();

        assert_eq!(digest.version().to_string(), "0.6.37");
        assert!(Arc::ptr_eq(digest.repository(), &built_in_repository()));
    }

    #[test]
    fn rejects_invalid_installed_package_version() {
        let error =
            parse_installed_packages("Package\tVersion\ndigest\tnot-a-version\n").unwrap_err();

        assert!(matches!(
            error,
            InstalledPackagesError::InvalidVersion { package, version, .. }
                if package == "digest" && version == "not-a-version"
        ));
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

        assert!(matches!(
            error,
            InstalledPackagesError::LibraryNotDirectory { path: actual } if actual == path
        ));
    }
}

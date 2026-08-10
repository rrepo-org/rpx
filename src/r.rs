use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Output,
    time::{SystemTime, UNIX_EPOCH},
};

use miette::Diagnostic;
use r_description::lossless::Version;
use thiserror::Error;
use tokio::{process::Command, sync::OnceCell};

use crate::{
    project::project_library_path, repository::built_in_repository, resolver::PackageVersion,
};

#[derive(Debug, Error, Diagnostic)]
pub enum RSubprocessError {
    #[error("failed to start {program}: {source}")]
    #[diagnostic(code(rpx::r::start_failed))]
    Start {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{program} exited unsuccessfully with code {exit_code:?}: {summary}")]
    #[diagnostic(code(rpx::r::command_failed))]
    Failed {
        program: &'static str,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        summary: String,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum PackageInstallError {
    #[error("package installation path is not valid UTF-8: {}", path.display())]
    #[diagnostic(code(rpx::install::invalid_path))]
    InvalidPath { path: PathBuf },

    #[error("failed to install {target}: {source}")]
    #[diagnostic(code(rpx::install::command_failed))]
    Command {
        target: String,
        #[source]
        source: RSubprocessError,
    },

    #[error(
        "failed to install {target}: {source} (log: {})",
        log_path.display()
    )]
    #[diagnostic(code(rpx::install::command_failed))]
    Failed {
        target: String,
        log_path: PathBuf,
        #[source]
        source: RSubprocessError,
    },

    #[error("failed to write installation log at {}: {source}", path.display())]
    #[diagnostic(code(rpx::install::log_write_failed))]
    LogWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum InstalledPackagesError {
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

#[derive(Debug, Error, Diagnostic)]
pub enum RError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] PackageInstallError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Version(#[from] RVersionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Remove(#[from] PackageRemovalError),
}

pub(crate) trait RVirtualEnv {
    fn with_venv(program: impl AsRef<str>) -> Self;
}

impl RVirtualEnv for tokio::process::Command {
    fn with_venv(program: impl AsRef<str>) -> Self {
        let mut command = tokio::process::Command::new(program.as_ref());
        command.env("R_LIBS_USER", project_library_path());
        command
    }
}

static BASE_PACKAGES: OnceCell<Vec<String>> = OnceCell::const_new();
static R_VERSION: OnceCell<semver::Version> = OnceCell::const_new();

pub async fn install_local_package(
    artifact_path: &Path,
    package: &str,
    version: &str,
    pkg_type: &str,
    target_library: &Path,
) -> Result<(), PackageInstallError> {
    let artifact_path = artifact_path
        .to_str()
        .ok_or_else(|| PackageInstallError::InvalidPath {
            path: artifact_path.to_path_buf(),
        })?;
    let target_library =
        target_library
            .to_str()
            .ok_or_else(|| PackageInstallError::InvalidPath {
                path: target_library.to_path_buf(),
            })?;

    let expression = concat!(
        "install.packages('%ARTIFACT%', repos = NULL, type = '%TYPE%', lib = '%LIB%');",
        "packages <- installed.packages(lib.loc = '%LIB%');",
        "if (!('%PACKAGE%' %in% rownames(packages))) stop('Expected package %PACKAGE% to be installed');",
        "installed_version <- packages['%PACKAGE%', 'Version'];",
        "if (installed_version != '%VERSION%') warning(sprintf('Installed %s version %s, expected %s', '%PACKAGE%', installed_version, '%VERSION%'))"
    )
    .replace("%ARTIFACT%", &escape_r_string(artifact_path))
    .replace("%TYPE%", &escape_r_string(pkg_type))
    .replace("%LIB%", &escape_r_string(target_library))
    .replace("%PACKAGE%", &escape_r_string(package))
    .replace("%VERSION%", &escape_r_string(version));

    let mut command = Command::with_venv("Rscript");
    command.arg("-e").arg(expression);
    let output = run_subprocess(command, "Rscript").await;

    install_command_result(output, format!("{package}@{version}"))
}

pub async fn install_package_directory(
    package_root: &Path,
    target_library: &Path,
    target: &str,
) -> Result<(), PackageInstallError> {
    let mut command = Command::with_venv("R");
    command
        .arg("CMD")
        .arg("INSTALL")
        .arg(format!("--library={}", target_library.display()))
        .arg(package_root);
    let output = run_subprocess(command, "R").await;

    install_command_result(output, target.to_string())
}

fn install_command_result(
    result: Result<Output, RSubprocessError>,
    target: String,
) -> Result<(), PackageInstallError> {
    let Err(source) = result else {
        return Ok(());
    };
    match &source {
        RSubprocessError::Start { .. } => Err(PackageInstallError::Command { target, source }),
        RSubprocessError::Failed { stdout, stderr, .. } => {
            let log_path = install_log_path();
            write_install_log(&log_path, stdout, stderr)?;
            Err(PackageInstallError::Failed {
                target,
                log_path,
                source,
            })
        }
    }
}

pub async fn base_packages() -> Result<Vec<String>, BasePackagesError> {
    BASE_PACKAGES
        .get_or_try_init(fetch_base_packages)
        .await
        .cloned()
}

pub async fn installed_packages() -> Result<BTreeMap<String, PackageVersion>, InstalledPackagesError>
{
    let expression = concat!(
        "packages <- installed.packages(lib.loc = .libPaths()[1]);",
        "if (nrow(packages) == 0) quit(save = 'no', status = 0);",
        "write.table(packages[, c('Package', 'Version'), drop = FALSE], ",
        "sep = '\t', row.names = FALSE, col.names = TRUE, quote = FALSE)"
    );

    let mut command = Command::with_venv("Rscript");
    command.arg("-e").arg(expression);
    let output = run_subprocess(command, "Rscript")
        .await
        .map_err(|source| InstalledPackagesError::Command { source })?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|source| InstalledPackagesError::InvalidUtf8 { source })?;

    parse_installed_packages(&stdout)
}

pub fn remove_packages_from_venv(packages: &BTreeSet<String>) -> Result<(), PackageRemovalError> {
    packages
        .iter()
        .try_for_each(|package| remove_package_from_venv(package))
}

pub fn remove_package_from_venv(package: &str) -> Result<(), PackageRemovalError> {
    let package_dir = project_library_path().join(package);

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

            let parsed_version = version.parse::<Version>().map_err(|details| {
                InstalledPackagesError::InvalidVersion {
                    line: index + 1,
                    package: package.to_string(),
                    version: version.to_string(),
                    details,
                }
            })?;

            Ok((
                package.to_string(),
                PackageVersion::new(parsed_version, repository.clone()),
            ))
        })
        .collect()
}

fn escape_r_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub async fn r_version_async() -> Result<semver::Version, RVersionError> {
    R_VERSION.get_or_try_init(fetch_r_version).await.cloned()
}

async fn fetch_r_version() -> Result<semver::Version, RVersionError> {
    let mut command = tokio::process::Command::new("Rscript");
    command.arg("-e").arg("cat(as.character(getRversion()))");
    let output = run_subprocess(command, "Rscript")
        .await
        .map_err(|source| RVersionError::Command { source })?;

    let version =
        String::from_utf8(output.stdout).map_err(|source| RVersionError::InvalidUtf8 { source })?;
    let version = version.trim();

    version
        .parse()
        .map_err(|source| RVersionError::InvalidVersion {
            version: version.to_string(),
            source,
        })
}

async fn run_subprocess(
    mut command: Command,
    program: &'static str,
) -> Result<Output, RSubprocessError> {
    let output = command
        .output()
        .await
        .map_err(|source| RSubprocessError::Start { program, source })?;
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

fn write_install_log(
    log_path: &Path,
    stdout: &str,
    stderr: &str,
) -> Result<(), PackageInstallError> {
    let mut contents = String::new();
    contents.push_str("# stdout\n");
    contents.push_str(stdout);
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("# stderr\n");
    contents.push_str(stderr);

    fs::write(log_path, contents).map_err(|source| PackageInstallError::LogWrite {
        path: log_path.to_path_buf(),
        source,
    })
}

async fn fetch_base_packages() -> Result<Vec<String>, BasePackagesError> {
    let mut command = Command::with_venv("Rscript");
    command
        .arg("-e")
        .arg("writeLines(rownames(installed.packages(priority = 'base')))");
    let output = run_subprocess(command, "Rscript")
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

fn install_log_path() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("rpx-install-{}-{unique}.log", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
}

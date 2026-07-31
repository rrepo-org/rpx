use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::{process::Command, sync::OnceCell};

use crate::project::project_library_path;

#[derive(Debug)]
pub struct InstalledPackage {
    pub package: String,
    pub version: String,
}

#[derive(Debug)]
pub struct InstallFailure {
    pub exit_code: Option<i32>,
    pub log_path: PathBuf,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub version: String,
    pub platform: String,
    pub pkg_type: String,
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

pub async fn install_local_package(
    artifact_path: &Path,
    package: &str,
    version: &str,
    pkg_type: &str,
    target_library: &Path,
) -> Result<(), InstallFailure> {
    let artifact_path = artifact_path
        .to_str()
        .expect("artifact path should be valid utf-8");
    let target_library = target_library
        .to_str()
        .expect("target library path should be valid utf-8");

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

    let output = Command::with_venv("Rscript")
        .arg("-e")
        .arg(expression)
        .output()
        .await
        .expect("failed to run Rscript");

    install_command_result(output)
}

pub async fn install_project_package(
    project_root: &Path,
    target_library: &Path,
) -> Result<(), InstallFailure> {
    let output = Command::with_venv("R")
        .arg("CMD")
        .arg("INSTALL")
        .arg(format!("--library={}", target_library.display()))
        .arg(project_root)
        .output()
        .await
        .expect("failed to run R CMD INSTALL");

    install_command_result(output)
}

fn install_command_result(output: std::process::Output) -> Result<(), InstallFailure> {
    if output.status.success() {
        return Ok(());
    }

    let log_path = install_log_path();
    let mut contents = String::new();

    contents.push_str("# stdout\n");
    contents.push_str(&String::from_utf8_lossy(&output.stdout));
    if !contents.ends_with('\n') {
        contents.push('\n');
    }

    contents.push_str("# stderr\n");
    contents.push_str(&String::from_utf8_lossy(&output.stderr));

    fs::write(&log_path, contents).expect("failed to write install log");

    let summary = summarize_install_output(&output.stdout, &output.stderr);

    Err(InstallFailure {
        exit_code: output.status.code(),
        log_path,
        summary,
    })
}

pub async fn base_packages() -> Vec<String> {
    BASE_PACKAGES.get_or_init(fetch_base_packages).await.clone()
}

pub async fn installed_packages_async() -> Vec<InstalledPackage> {
    let expression = concat!(
        "packages <- installed.packages(lib.loc = .libPaths()[1]);",
        "if (nrow(packages) == 0) quit(save = 'no', status = 0);",
        "write.table(packages[, c('Package', 'Version'), drop = FALSE], ",
        "sep = '\t', row.names = FALSE, col.names = TRUE, quote = FALSE)"
    );

    let output = Command::with_venv("Rscript")
        .arg("-e")
        .arg(expression)
        .output()
        .await
        .expect("failed to run Rscript");

    crate::exit_with_status(output.status.code());

    parse_installed_packages(&String::from_utf8_lossy(&output.stdout))
}

pub async fn installed_packages() -> Vec<InstalledPackage> {
    let expression = concat!(
        "packages <- installed.packages(lib.loc = .libPaths()[1]);",
        "if (nrow(packages) == 0) quit(save = 'no', status = 0);",
        "write.table(packages[, c('Package', 'Version'), drop = FALSE], ",
        "sep = '\t', row.names = FALSE, col.names = TRUE, quote = FALSE)"
    );

    let output = Command::with_venv("Rscript")
        .arg("-e")
        .arg(expression)
        .output()
        .await
        .expect("failed to run Rscript");

    crate::exit_with_status(output.status.code());

    parse_installed_packages(&String::from_utf8_lossy(&output.stdout))
}

pub fn remove_packages_from_venv(packages: &[String]) -> Result<(), String> {
    let _ = packages
        .iter()
        .try_for_each(|p| remove_package_from_venv(&p));

    Ok(())
}

pub fn remove_package_from_venv(package: &str) -> Result<(), String> {
    let package_dir = project_library_path().join(package);

    if !package_dir.exists() {
        return Ok(());
    }

    std::fs::remove_dir_all(&package_dir).map_err(|error| {
        format!(
            "failed to remove package directory {}: {error}",
            package_dir.display()
        )
    })
}

fn parse_installed_packages(output: &str) -> Vec<InstalledPackage> {
    output
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let package = parts.next()?.trim().to_string();
            let version = parts.next()?.trim().to_string();

            Some(InstalledPackage { package, version })
        })
        .collect()
}

fn escape_r_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub async fn r_version_async() -> Result<String, String> {
    let output = tokio::process::Command::new("Rscript")
        .arg("-e")
        .arg("cat(as.character(getRversion()))")
        .output()
        .await
        .map_err(|error| format!("failed to run Rscript: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to inspect R version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if version.is_empty() {
        return Err("failed to inspect R version: Rscript returned empty output".to_string());
    }

    Ok(version)
}
pub async fn fetch_runtime_info() -> RuntimeInfo {
    let output = Command::with_venv("Rscript")
        .arg("-e")
        .arg("cat(as.character(getRversion()), '\t', R.version$platform, '\t', .Platform$pkgType, sep = '')")
        .output()
        .await
        .expect("failed to run Rscript");

    crate::exit_with_status(output.status.code());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.trim().splitn(3, '\t');

    RuntimeInfo {
        version: parts
            .next()
            .expect("R version should be present")
            .to_string(),
        platform: parts
            .next()
            .expect("R platform should be present")
            .to_string(),
        pkg_type: parts
            .next()
            .expect("R package type should be present")
            .to_string(),
    }
}

async fn fetch_base_packages() -> Vec<String> {
    let output = Command::with_venv("Rscript")
        .arg("-e")
        .arg("writeLines(rownames(installed.packages(priority = 'base')))")
        .output()
        .await
        .expect("failed to run Rscript");

    crate::exit_with_status(output.status.code());

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn summarize_install_output(stdout: &[u8], stderr: &[u8]) -> String {
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
        .unwrap_or("package installation failed")
        .to_string()
}

fn install_log_path() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("rpx-install-{}-{unique}.log", std::process::id()))
}

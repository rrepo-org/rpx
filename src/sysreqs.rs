use flate2::read::GzDecoder;
use r_description::lossless::RDescription;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tar::Archive;

use crate::project::cache_dir_path;

const SYSREQS_API_BASE: &str = "https://api.github.com/repos/rstudio/r-system-requirements";
const SYSREQS_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SysreqDbSnapshot {
    pub commit: String,
    pub rules: Vec<SysreqRule>,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SysreqRule {
    pub id: String,
    pub patterns: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<SysreqDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SysreqDependency {
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub apt_satisfy: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<SysreqConstraint>,
    #[serde(default)]
    pub pre_install: Vec<SysreqAction>,
    #[serde(default)]
    pub post_install: Vec<SysreqAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SysreqConstraint {
    pub os: String,
    #[serde(default)]
    pub distribution: String,
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SysreqAction {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub script: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostPlatform {
    Linux {
        distribution: String,
        version: String,
    },
    Macos,
    Windows,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemDependencyPlan {
    pub host: HostPlatform,
    pub missing_packages: Vec<String>,
    pub install_packages: Vec<String>,
    pub pre_install_commands: Vec<String>,
    pub post_install_commands: Vec<String>,
    pub unsupported_rules: Vec<String>,
    pub package_rules: BTreeMap<String, Vec<String>>,
    pub install_supported: bool,
    pub can_auto_install: bool,
    pub installed_query_error: Option<String>,
    pub needs_metadata_refresh: bool,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LatestSnapshotCache {
    commit: String,
    fetched_at_unix: u64,
}

#[derive(Debug, Deserialize)]
struct RawSysreqRule {
    patterns: Vec<String>,
    #[serde(default)]
    dependencies: Vec<SysreqDependency>,
}

pub(crate) fn latest_snapshot() -> Result<SysreqDbSnapshot, String> {
    let cache_path = latest_snapshot_cache_path();
    if let Some(cached) =
        read_json_cache_fresh::<LatestSnapshotCache>(&cache_path, SYSREQS_CACHE_TTL)
    {
        return snapshot_for_commit(&cached.commit);
    }

    let commit = latest_commit_hash()?;
    let snapshot = snapshot_for_commit(&commit)?;
    write_json(
        &cache_path,
        &LatestSnapshotCache {
            commit,
            fetched_at_unix: now_unix(),
        },
    )?;
    Ok(snapshot)
}

pub(crate) fn cached_latest_snapshot() -> Result<Option<SysreqDbSnapshot>, String> {
    let cache_path = latest_snapshot_cache_path();
    let Some(cached) = read_json_cache::<LatestSnapshotCache>(&cache_path)? else {
        return Ok(None);
    };

    snapshot_for_commit(&cached.commit).map(Some)
}

pub(crate) fn snapshot_for_commit(commit: &str) -> Result<SysreqDbSnapshot, String> {
    let cache_path = db_snapshot_cache_path(commit);
    if cache_path.exists() {
        return read_json(&cache_path);
    }

    let snapshot = download_snapshot(commit)?;
    write_json(&cache_path, &snapshot)?;
    Ok(snapshot)
}

pub(crate) fn empty_snapshot() -> SysreqDbSnapshot {
    SysreqDbSnapshot {
        commit: String::new(),
        rules: vec![],
        scripts: BTreeMap::new(),
    }
}

pub(crate) fn match_rules(description: &RDescription, db: &SysreqDbSnapshot) -> Vec<String> {
    let system_requirements = description.system_requirements();

    let Some(spec) = system_requirements
        .as_ref()
        .map(|values| values.join(" "))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return vec![];
    };

    let mut matches = BTreeSet::new();

    for rule in &db.rules {
        if rule.patterns.iter().any(|pattern| {
            Regex::new(&format!("(?i){pattern}"))
                .map(|regex| regex.is_match(&spec))
                .unwrap_or(false)
        }) {
            matches.insert(rule.id.clone());
        }
    }

    matches.into_iter().collect()
}

pub(crate) fn current_host_platform() -> HostPlatform {
    match env::consts::OS {
        "macos" => HostPlatform::Macos,
        "windows" => HostPlatform::Windows,
        "linux" => detect_linux_platform(),
        other => HostPlatform::Unknown(other.to_string()),
    }
}

pub(crate) fn resolve_plan(
    db: &SysreqDbSnapshot,
    package_rules: &BTreeMap<String, Vec<String>>,
) -> SystemDependencyPlan {
    let host = current_host_platform();
    let mut install_packages = BTreeSet::new();
    let mut pre_install = Vec::new();
    let mut post_install = Vec::new();
    let mut unsupported_rules = BTreeSet::new();

    for rules in package_rules.values() {
        for rule_id in rules {
            let Some(rule) = db.rules.iter().find(|rule| rule.id == *rule_id) else {
                unsupported_rules.insert(rule_id.clone());
                continue;
            };

            let matching_dependencies = rule
                .dependencies
                .iter()
                .filter(|dependency| dependency_matches_host(dependency, &host))
                .collect::<Vec<_>>();
            if matching_dependencies.is_empty() {
                unsupported_rules.insert(rule_id.clone());
                continue;
            }

            for dependency in matching_dependencies {
                for package in &dependency.packages {
                    install_packages.insert(package.clone());
                }
                for action in &dependency.pre_install {
                    if let Ok(command) = action_command(action, db) {
                        pre_install.push(command);
                    }
                }
                for action in &dependency.post_install {
                    if let Ok(command) = action_command(action, db) {
                        post_install.push(command);
                    }
                }
            }
        }
    }

    pre_install = dedupe_keep_order(pre_install);
    post_install = dedupe_keep_order(post_install);
    let install_packages = install_packages.into_iter().collect::<Vec<_>>();
    let install_supported = package_manager_for_host(&host).is_some();
    let can_auto_install =
        install_supported && install_prefix(std::io::stderr().is_terminal()).is_some();

    let (missing_packages, installed_query_error, needs_metadata_refresh) =
        match missing_packages_for_host(&host, &install_packages) {
            Ok(missing_packages) => (missing_packages, None, false),
            Err(error) => (
                install_packages.clone(),
                Some(error.to_string()),
                error.needs_metadata_refresh(),
            ),
        };

    SystemDependencyPlan {
        host,
        missing_packages,
        install_packages,
        pre_install_commands: pre_install,
        post_install_commands: post_install,
        unsupported_rules: unsupported_rules.into_iter().collect(),
        package_rules: package_rules.clone(),
        install_supported,
        can_auto_install,
        installed_query_error,
        needs_metadata_refresh,
    }
}

pub(crate) fn preview_commands(plan: &SystemDependencyPlan) -> Vec<String> {
    let mut commands = Vec::new();
    commands.extend(
        plan.pre_install_commands
            .iter()
            .cloned()
            .map(prefix_command),
    );
    if !plan.missing_packages.is_empty()
        && let Some(command) = package_install_command(&plan.host, &plan.missing_packages)
    {
        commands.push(prefix_command(command));
    }
    commands.extend(
        plan.post_install_commands
            .iter()
            .cloned()
            .map(prefix_command),
    );
    commands
}

pub(crate) fn install(plan: &SystemDependencyPlan) -> Result<(), String> {
    if plan.missing_packages.is_empty()
        && plan.pre_install_commands.is_empty()
        && plan.post_install_commands.is_empty()
    {
        return Ok(());
    }

    if !plan.install_supported {
        return Err(format!(
            "automatic system dependency installation is not supported on {}",
            plan.host.label()
        ));
    }

    let Some(prefix) = install_prefix(std::io::stderr().is_terminal()) else {
        return Err(system_install_privilege_error(
            std::io::stderr().is_terminal(),
        ));
    };

    for command in &plan.pre_install_commands {
        run_shell_command(&prefix, command)?;
    }
    if !plan.missing_packages.is_empty() {
        let command =
            package_install_command(&plan.host, &plan.missing_packages).ok_or_else(|| {
                format!(
                    "automatic system dependency installation is not supported on {}",
                    plan.host.label()
                )
            })?;
        run_shell_command(&prefix, &command)?;
    }
    for command in &plan.post_install_commands {
        run_shell_command(&prefix, command)?;
    }

    Ok(())
}

pub(crate) fn refresh_metadata(plan: &SystemDependencyPlan) -> Result<(), String> {
    let Some(command) = package_update_command(&plan.host) else {
        return Err(format!(
            "automatic system dependency installation is not supported on {}",
            plan.host.label()
        ));
    };
    let Some(prefix) = install_prefix(std::io::stderr().is_terminal()) else {
        return Err(system_install_privilege_error(
            std::io::stderr().is_terminal(),
        ));
    };

    run_shell_command(&prefix, &command)
}

pub(crate) fn refresh_preview_command(plan: &SystemDependencyPlan) -> Option<String> {
    package_update_command(&plan.host).map(prefix_command)
}

pub(crate) fn recheck_missing_packages(plan: &SystemDependencyPlan) -> Result<Vec<String>, String> {
    missing_packages_for_host(&plan.host, &plan.install_packages).map_err(|error| error.to_string())
}

impl HostPlatform {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Linux {
                distribution,
                version,
            } => format!("linux/{distribution}-{version}"),
            Self::Macos => "macos".to_string(),
            Self::Windows => "windows".to_string(),
            Self::Unknown(value) => value.clone(),
        }
    }
}

fn latest_commit_hash() -> Result<String, String> {
    let client = github_client()?;
    let response = client
        .get(format!("{SYSREQS_API_BASE}/commits/main"))
        .send()
        .map_err(|error| format!("failed to fetch sysreq database commit: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to fetch sysreq database commit ({})",
            response.status()
        ));
    }

    response
        .json::<CommitResponse>()
        .map(|response| response.sha)
        .map_err(|error| format!("failed to decode sysreq database commit: {error}"))
}

fn download_snapshot(commit: &str) -> Result<SysreqDbSnapshot, String> {
    let client = github_client()?;
    let mut response = client
        .get(format!("{SYSREQS_API_BASE}/tarball/{commit}"))
        .send()
        .map_err(|error| format!("failed to download sysreq database snapshot: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to download sysreq database snapshot ({})",
            response.status()
        ));
    }

    let mut bytes = Vec::new();
    response
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read sysreq database snapshot: {error}"))?;

    let decoder = GzDecoder::new(bytes.as_slice());
    let mut archive = Archive::new(decoder);
    let mut rules = Vec::new();
    let mut scripts = BTreeMap::new();

    for entry in archive
        .entries()
        .map_err(|error| format!("failed to read sysreq database archive: {error}"))?
    {
        let mut entry =
            entry.map_err(|error| format!("failed to read sysreq archive entry: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("failed to read sysreq archive path: {error}"))?;
        let components = path.components().collect::<Vec<_>>();
        if components.len() < 3 {
            continue;
        }

        let top = components[1].as_os_str().to_string_lossy();
        let name = components[2].as_os_str().to_string_lossy().to_string();
        if top == "rules"
            && std::path::Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            let mut contents = String::new();
            entry
                .read_to_string(&mut contents)
                .map_err(|error| format!("failed to read sysreq rule {name}: {error}"))?;
            let raw = serde_json::from_str::<RawSysreqRule>(&contents)
                .map_err(|error| format!("failed to parse sysreq rule {name}: {error}"))?;
            rules.push(SysreqRule {
                id: name.trim_end_matches(".json").to_string(),
                patterns: raw.patterns,
                dependencies: raw.dependencies,
            });
            continue;
        }

        if top == "scripts" {
            let mut contents = String::new();
            entry
                .read_to_string(&mut contents)
                .map_err(|error| format!("failed to read sysreq script {name}: {error}"))?;
            scripts.insert(name, contents);
        }
    }

    rules.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(SysreqDbSnapshot {
        commit: commit.to_string(),
        rules,
        scripts,
    })
}

fn dependency_matches_host(dependency: &SysreqDependency, host: &HostPlatform) -> bool {
    if dependency.constraints.is_empty() {
        return true;
    }

    dependency
        .constraints
        .iter()
        .any(|constraint| constraint_matches_host(constraint, host))
}

fn constraint_matches_host(constraint: &SysreqConstraint, host: &HostPlatform) -> bool {
    match host {
        HostPlatform::Linux {
            distribution,
            version,
        } => {
            if constraint.os != "linux" || constraint.distribution != *distribution {
                return false;
            }
            constraint.versions.is_empty()
                || constraint.versions.iter().any(|value| value == version)
        }
        HostPlatform::Windows => constraint.os == "windows",
        HostPlatform::Macos => constraint.os == "macos",
        HostPlatform::Unknown(_) => false,
    }
}

fn detect_linux_platform() -> HostPlatform {
    let Ok(contents) = fs::read_to_string("/etc/os-release") else {
        return HostPlatform::Unknown("linux".to_string());
    };

    let values = contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            (
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let id = values.get("ID").cloned().unwrap_or_default();
    let version = values
        .get("VERSION_ID")
        .cloned()
        .or_else(|| values.get("VERSION_CODENAME").cloned())
        .or_else(|| values.get("DEBIAN_CODENAME").cloned())
        .or_else(|| values.get("VERSION").cloned())
        .unwrap_or_default();
    let distribution = match id.as_str() {
        "ubuntu" => Some("ubuntu"),
        "debian" => Some("debian"),
        "centos" => Some("centos"),
        "rhel" => Some("redhat"),
        "rocky" | "rockylinux" => Some("rockylinux"),
        "opensuse-leap" | "opensuse" => Some("opensuse"),
        "sles" | "sle_hpc" => Some("sle"),
        "fedora" => Some("fedora"),
        "alpine" => Some("alpine"),
        _ => None,
    };

    match distribution {
        Some(distribution) => HostPlatform::Linux {
            distribution: distribution.to_string(),
            version,
        },
        None => HostPlatform::Unknown(format!("linux/{id}-{version}")),
    }
}

fn installed_packages(host: &HostPlatform) -> Result<BTreeSet<String>, String> {
    let output = match host {
        HostPlatform::Linux { distribution, .. }
            if distribution == "ubuntu" || distribution == "debian" =>
        {
            Command::new("dpkg-query")
                .args(["-W", "-f=${Package}\n"])
                .output()
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "centos"
                || distribution == "rockylinux"
                || distribution == "redhat"
                || distribution == "fedora"
                || distribution == "opensuse"
                || distribution == "sle" =>
        {
            Command::new("rpm")
                .args(["-qa", "--qf", "%{NAME}\n"])
                .output()
        }
        HostPlatform::Linux { distribution, .. } if distribution == "alpine" => {
            Command::new("apk").args(["info"]).output()
        }
        _ => {
            return Err(format!(
                "installed package detection is not supported on {}",
                host.label()
            ));
        }
    }
    .map_err(|error| format!("failed to inspect installed system packages: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "failed to inspect installed system packages ({})",
            output.status
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn missing_packages_for_host(
    host: &HostPlatform,
    packages: &[String],
) -> Result<Vec<String>, AvailabilityCheckError> {
    if matches!(host, HostPlatform::Linux { distribution, .. } if distribution == "ubuntu" || distribution == "debian")
    {
        return apt_missing_packages(packages);
    }

    let installed = installed_packages(host).map_err(AvailabilityCheckError::other)?;
    Ok(packages
        .iter()
        .filter(|package| !installed.contains(*package))
        .cloned()
        .collect())
}

fn apt_missing_packages(packages: &[String]) -> Result<Vec<String>, AvailabilityCheckError> {
    let output = apt_simulation_output(packages)?;
    Ok(apt_simulation_missing_packages(packages, &output))
}

fn apt_simulation_output(packages: &[String]) -> Result<String, AvailabilityCheckError> {
    let output = Command::new("apt-get")
        .arg("-s")
        .arg("install")
        .arg("-y")
        .arg("--no-install-recommends")
        .args(packages)
        .output()
        .map_err(|error| {
            AvailabilityCheckError::other(format!("failed to inspect apt packages: {error}"))
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");

    if !output.status.success() {
        return Err(classify_apt_failure(output.status.code(), &combined));
    }
    Ok(format!("{stdout}\n{stderr}"))
}

fn apt_simulation_missing_packages(packages: &[String], output: &str) -> Vec<String> {
    let selected_aliases = output
        .lines()
        .filter_map(parse_apt_alias_selection)
        .collect::<BTreeMap<_, _>>();
    let packages_to_install = output
        .lines()
        .filter_map(parse_apt_installed_package)
        .collect::<BTreeSet<_>>();

    packages
        .iter()
        .filter(|package| {
            let candidate = selected_aliases
                .get(package.as_str())
                .map_or(package.as_str(), String::as_str);
            packages_to_install.contains(candidate)
        })
        .cloned()
        .collect()
}

fn parse_apt_alias_selection(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let prefix = "Note, selecting '";
    let middle = "' instead of '";
    if !line.starts_with(prefix) {
        return None;
    }

    let rest = &line[prefix.len()..];
    let (selected, rest) = rest.split_once(middle)?;
    let requested = rest.strip_suffix('"').or_else(|| rest.strip_suffix('\''))?;
    Some((requested.to_string(), selected.to_string()))
}

fn parse_apt_installed_package(line: &str) -> Option<String> {
    let line = line.trim_start();
    let rest = line.strip_prefix("Inst ")?;
    let package = rest.split_whitespace().next()?;
    Some(package.to_string())
}

#[derive(Debug, Clone)]
struct AvailabilityCheckError {
    message: String,
    needs_metadata_refresh: bool,
}

impl AvailabilityCheckError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_metadata_refresh: false,
        }
    }

    fn metadata_refresh(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_metadata_refresh: true,
        }
    }

    fn needs_metadata_refresh(&self) -> bool {
        self.needs_metadata_refresh
    }
}

impl std::fmt::Display for AvailabilityCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn classify_apt_failure(exit_code: Option<i32>, output: &str) -> AvailabilityCheckError {
    let trimmed = output.trim();
    if apt_failure_likely_needs_metadata_refresh(trimmed) {
        return AvailabilityCheckError::metadata_refresh(format!(
            "failed to inspect apt packages (exit status: {})",
            exit_code.unwrap_or(1)
        ));
    }

    AvailabilityCheckError::other(format!(
        "failed to inspect apt packages (exit status: {}): {trimmed}",
        exit_code.unwrap_or(1)
    ))
}

fn apt_failure_likely_needs_metadata_refresh(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    lowered.contains("unable to locate package")
        || lowered.contains("has no installation candidate")
        || lowered.contains("package information")
}

fn package_manager_for_host(host: &HostPlatform) -> Option<&'static str> {
    match host {
        HostPlatform::Linux { distribution, .. }
            if distribution == "ubuntu" || distribution == "debian" =>
        {
            Some("apt-get")
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "centos" || distribution == "redhat" =>
        {
            Some("yum")
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "rockylinux" || distribution == "fedora" =>
        {
            Some("dnf")
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "opensuse" || distribution == "sle" =>
        {
            Some("zypper")
        }
        HostPlatform::Linux { distribution, .. } if distribution == "alpine" => Some("apk"),
        _ => None,
    }
}

fn package_update_command(host: &HostPlatform) -> Option<String> {
    match host {
        HostPlatform::Linux { distribution, .. }
            if distribution == "ubuntu" || distribution == "debian" =>
        {
            Some("apt-get update".to_string())
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "rockylinux" || distribution == "fedora" =>
        {
            Some("dnf makecache".to_string())
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "centos" || distribution == "redhat" =>
        {
            Some("yum makecache".to_string())
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "opensuse" || distribution == "sle" =>
        {
            Some("zypper --non-interactive refresh".to_string())
        }
        HostPlatform::Linux { distribution, .. } if distribution == "alpine" => {
            Some("apk update".to_string())
        }
        _ => None,
    }
}

fn package_install_command(host: &HostPlatform, packages: &[String]) -> Option<String> {
    if packages.is_empty() {
        return None;
    }

    let joined = packages.join(" ");
    match host {
        HostPlatform::Linux { distribution, .. }
            if distribution == "ubuntu" || distribution == "debian" =>
        {
            Some(format!("apt-get update && apt-get install -y {joined}"))
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "centos" || distribution == "redhat" =>
        {
            Some(format!("yum install -y {joined}"))
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "rockylinux" || distribution == "fedora" =>
        {
            Some(format!("dnf install -y {joined}"))
        }
        HostPlatform::Linux { distribution, .. }
            if distribution == "opensuse" || distribution == "sle" =>
        {
            Some(format!("zypper --non-interactive install {joined}"))
        }
        HostPlatform::Linux { distribution, .. } if distribution == "alpine" => {
            Some(format!("apk add --no-cache {joined}"))
        }
        _ => None,
    }
}

fn action_command(action: &SysreqAction, db: &SysreqDbSnapshot) -> Result<String, String> {
    if !action.command.trim().is_empty() {
        return Ok(action.command.trim().to_string());
    }
    if !action.script.trim().is_empty() {
        let script = db
            .scripts
            .get(action.script.trim())
            .ok_or_else(|| format!("missing sysreq helper script: {}", action.script.trim()))?;
        return Ok(script.trim().to_string());
    }
    Err("invalid sysreq action: missing command or script".to_string())
}

fn install_prefix(interactive: bool) -> Option<Vec<String>> {
    if current_uid().as_deref() == Some("0") {
        return Some(vec![]);
    }

    let sudo_ok = Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if sudo_ok {
        return Some(vec!["sudo".to_string()]);
    }

    if interactive && sudo_available() {
        return Some(vec!["sudo".to_string()]);
    }

    None
}

fn sudo_available() -> bool {
    Command::new("sudo")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn current_uid() -> Option<String> {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_shell_command(prefix: &[String], command: &str) -> Result<(), String> {
    let mut process = if prefix.is_empty() {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    } else {
        let mut process = Command::new(&prefix[0]);
        process.args(&prefix[1..]);
        process.args(["sh", "-c", command]);
        process
    };

    let output = process
        .output()
        .map_err(|error| format!("failed to run system command `{command}`: {error}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = [stdout.as_ref().trim(), stderr.as_ref().trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let tail = command_output_tail(&combined, 40);

    if tail.is_empty() {
        return Err(format!(
            "system command failed ({}): {command}",
            output.status
        ));
    }

    Err(format!(
        "system command failed ({}): {command}\n{tail}",
        output.status
    ))
}

fn command_output_tail(output: &str, max_lines: usize) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn prefix_command(command: String) -> String {
    install_prefix(std::io::stderr().is_terminal())
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| format!("{} {command}", prefix.join(" ")))
        .unwrap_or(command)
}

fn system_install_privilege_error(interactive: bool) -> String {
    if interactive {
        "need root privileges or sudo to install system dependencies".to_string()
    } else {
        "need root privileges or passwordless sudo to install system dependencies".to_string()
    }
}

fn github_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("rpx")
        .build()
        .map_err(|error| format!("failed to create sysreq database client: {error}"))
}

fn latest_snapshot_cache_path() -> PathBuf {
    let path = cache_dir_path().join("sysreqs").join("latest.json");
    ensure_parent_dir(&path);
    path
}

fn db_snapshot_cache_path(commit: &str) -> PathBuf {
    let path = cache_dir_path()
        .join("sysreqs")
        .join("snapshots")
        .join(format!("{commit}.json"));
    ensure_parent_dir(&path);
    path
}

fn ensure_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create sysreq cache directory");
    }
}

fn read_json_cache_fresh<T>(path: &PathBuf, ttl: Duration) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > ttl {
        return None;
    }
    read_json(path).ok()
}

fn read_json<T>(path: &PathBuf) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read sysreq cache {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse sysreq cache {}: {error}", path.display()))
}

fn read_json_cache<T>(path: &PathBuf) -> Result<Option<T>, String>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(None);
    }

    read_json(path).map(Some)
}

fn write_json<T>(path: &PathBuf, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    let contents = serde_json::to_string_pretty(value)
        .map_err(|error| format!("failed to serialize sysreq cache: {error}"))?;
    fs::write(path, format!("{contents}\n"))
        .map_err(|error| format!("failed to write sysreq cache {}: {error}", path.display()))
}

fn dedupe_keep_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{apt_failure_likely_needs_metadata_refresh, apt_simulation_missing_packages};

    #[test]
    fn apt_simulation_recognizes_alias_as_already_satisfied() {
        let output = "Reading package lists... Done\n\
Building dependency tree... Done\n\
Reading state information... Done\n\
Note, selecting 'libfreetype-dev' instead of 'libfreetype6-dev'\n\
libfreetype-dev is already the newest version (2.14.3+dfsg-1).\n\
0 upgraded, 0 newly installed, 0 to remove and 75 not upgraded.\n";

        assert!(
            apt_simulation_missing_packages(&["libfreetype6-dev".to_string()], output).is_empty()
        );
    }

    #[test]
    fn apt_simulation_recognizes_pending_install() {
        let output = "Reading package lists... Done\n\
Building dependency tree... Done\n\
Reading state information... Done\n\
The following NEW packages will be installed:\n\
  libfreetype6-dev\n\
Inst libfreetype6-dev (2.14.3+dfsg-1 Debian:testing [amd64])\n\
Conf libfreetype6-dev (2.14.3+dfsg-1 Debian:testing [amd64])\n";

        assert_eq!(
            apt_simulation_missing_packages(&["libfreetype6-dev".to_string()], output),
            vec!["libfreetype6-dev".to_string()]
        );
    }

    #[test]
    fn apt_simulation_batches_missing_and_satisfied_packages() {
        let output = "Reading package lists... Done\n\
Building dependency tree... Done\n\
Reading state information... Done\n\
Note, selecting 'libfreetype-dev' instead of 'libfreetype6-dev'\n\
The following NEW packages will be installed:\n\
  libxml2-dev\n\
Inst libxml2-dev (2.14.3+dfsg-1 Debian:testing [amd64])\n\
Conf libxml2-dev (2.14.3+dfsg-1 Debian:testing [amd64])\n";

        assert_eq!(
            apt_simulation_missing_packages(
                &["libfreetype6-dev".to_string(), "libxml2-dev".to_string()],
                output,
            ),
            vec!["libxml2-dev".to_string()]
        );
    }

    #[test]
    fn detects_metadata_refresh_needed_for_apt_resolution_failure() {
        let output = "Reading package lists... Done\n\
Building dependency tree... Done\n\
Reading state information... Done\n\
E: Unable to locate package libxml2-dev\n";

        assert!(apt_failure_likely_needs_metadata_refresh(output));
    }
}

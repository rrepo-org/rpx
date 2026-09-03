use crate::project::cache_dir_path;
use r_metadata::{HostedGitRemote, Remote, RemoteSource};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest, Sha256};
use std::{
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    str::FromStr,
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing_indicatif::span_ext::IndicatifSpanExt;

const FETCHED_REF: &str = "refs/rpx/source";
const FETCHED_COMMIT_REF: &str = "refs/rpx/commit";

static GIT_SEMAPHORE: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

const GIT_ENVIRONMENT_VARIABLES: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
];

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct GitOid([u8; 40]);

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(std::str::from_utf8(&self.0).expect("Git OID is ASCII"))
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for GitOid {
    type Err = GitOidParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitOidParseError);
        }
        let mut bytes = [0; 40];
        for (destination, source) in bytes.iter_mut().zip(value.bytes()) {
            *destination = source.to_ascii_lowercase();
        }
        Ok(Self(bytes))
    }
}

impl Serialize for GitOid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Error)]
#[error("invalid Git object ID; expected exactly 40 hexadecimal characters")]
pub(crate) struct GitOidParseError;

#[derive(Debug, Default)]
pub(crate) struct Identity {
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
}

pub(crate) fn configured_identity(path: &Path) -> Result<Identity, GitError> {
    Ok(Identity {
        name: config_value(path, "user.name")?,
        email: config_value(path, "user.email")?,
    })
}

pub(crate) fn is_inside_worktree(path: &Path) -> Result<bool, GitError> {
    let mut existing_ancestor = None;
    for ancestor in path.ancestors() {
        match ancestor.try_exists() {
            Ok(true) => {
                existing_ancestor = Some(ancestor);
                break;
            }
            Ok(false) => {}
            Err(source) => {
                return Err(GitError::FileSystem {
                    operation: "inspect Git discovery path",
                    path: ancestor.to_path_buf(),
                    source,
                });
            }
        }
    }
    let Some(existing_ancestor) = existing_ancestor else {
        return Ok(false);
    };

    let mut command = git_command();
    command
        .arg("-C")
        .arg(existing_ancestor)
        .args(["rev-parse", "--is-inside-work-tree"]);
    let output = command_output(command)?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

pub(crate) fn initialize_repository(path: &Path) -> Result<(), GitError> {
    let mut command = git_command();
    command.arg("init").arg(path);
    run(command, "initialize Git repository").map(|_| ())
}

fn config_value(path: &Path, key: &str) -> Result<Option<String>, GitError> {
    let existing_ancestor = path.ancestors().find(|path| path.is_dir());
    let mut command = git_command();
    if let Some(path) = existing_ancestor {
        command.arg("-C").arg(path);
    }
    command.args(["config", "--get", key]);
    let output = command_output(command)?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(Box::new(process_error("read Git identity configuration", &output)).into());
    }
    Ok(
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty()),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GitUrl(String);

impl GitUrl {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn from_hosted(
        host: Option<&str>,
        default_host: &str,
        remote: &HostedGitRemote,
    ) -> Result<Self, GitError> {
        let host = host.unwrap_or(default_host);
        if invalid_text(host) || invalid_text(&remote.owner) || invalid_text(&remote.repository) {
            return Err(GitError::InvalidUrl);
        }

        let mut url =
            reqwest::Url::parse(&format!("https://{host}")).map_err(|_| GitError::InvalidUrl)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(GitError::InvalidUrl);
        }

        let repository = remote.repository.trim_end_matches(".git");
        let path = std::iter::once(remote.owner.as_str())
            .chain(repository.split('/'))
            .collect::<Vec<_>>();
        if path.iter().any(|part| part.is_empty()) {
            return Err(GitError::InvalidUrl);
        }
        let last = path.len() - 1;
        let mut segments = url.path_segments_mut().map_err(|()| GitError::InvalidUrl)?;
        for (index, part) in path.into_iter().enumerate() {
            if index == last {
                segments.push(&format!("{part}.git"));
            } else {
                segments.push(part);
            }
        }
        drop(segments);

        Ok(Self(url.to_string()))
    }

    fn from_generic(value: &str) -> Result<Self, GitError> {
        let value = value.trim();
        if value.is_empty() || invalid_text(value) {
            return Err(GitError::InvalidUrl);
        }

        if value.contains("://") {
            let url = reqwest::Url::parse(value).map_err(|_| GitError::InvalidUrl)?;
            if !matches!(url.scheme(), "https" | "ssh") {
                return Err(GitError::UnsupportedScheme {
                    scheme: url.scheme().to_string(),
                });
            }
            if url.password().is_some() || (url.scheme() == "https" && !url.username().is_empty()) {
                return Err(GitError::CredentialsInUrl);
            }
            if url.scheme() == "ssh" && url.username().is_empty() {
                return Err(GitError::SshUsernameRequired);
            }
            if url.host_str().is_none()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path().trim_matches('/').is_empty()
            {
                return Err(GitError::InvalidUrl);
            }

            return Ok(Self(url.to_string()));
        }

        parse_scp_url(value)?;
        let delimiter = scp_delimiter(value).expect("validated SCP URL should contain a delimiter");
        let authority = &value[..delimiter];
        let path = &value[delimiter + 1..];
        let url = reqwest::Url::parse(&format!("ssh://{authority}/{path}"))
            .map_err(|_| GitError::InvalidUrl)?;
        Ok(Self(url.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn from_local_path(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned())
    }
}

impl TryFrom<Remote> for GitUrl {
    type Error = GitError;

    fn try_from(remote: Remote) -> Result<Self, Self::Error> {
        match remote.source {
            RemoteSource::GitHub(source) => {
                Self::from_hosted(remote.host.as_deref(), "github.com", &source)
            }
            RemoteSource::GitLab(source) => {
                Self::from_hosted(remote.host.as_deref(), "gitlab.com", &source)
            }
            RemoteSource::Bitbucket(source) => {
                Self::from_hosted(remote.host.as_deref(), "bitbucket.org", &source)
            }
            RemoteSource::Git(source) => Self::from_generic(&source.url),
            _ => Err(GitError::UnsupportedRemote),
        }
    }
}

impl TryFrom<&reqwest::Url> for GitUrl {
    type Error = GitError;

    fn try_from(url: &reqwest::Url) -> Result<Self, Self::Error> {
        Self::from_generic(url.as_str())
    }
}

impl TryFrom<&GitUrl> for reqwest::Url {
    type Error = GitError;

    fn try_from(url: &GitUrl) -> Result<Self, Self::Error> {
        Self::parse(url.as_str()).map_err(|_| GitError::InvalidUrl)
    }
}

impl fmt::Display for GitUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
pub(crate) enum GitError {
    #[error("unsupported Remotes source; expected GitHub, GitLab, Bitbucket, or generic git")]
    UnsupportedRemote,

    #[error("invalid Git remote URL")]
    InvalidUrl,

    #[error("unsupported Git URL scheme {scheme}; expected https or ssh")]
    UnsupportedScheme { scheme: String },

    #[error("credentials must not be embedded in a Git remote URL")]
    CredentialsInUrl,

    #[error("SSH Git remote URLs must include a username")]
    SshUsernameRequired,

    #[error("invalid Git reference {reference}")]
    InvalidReference { reference: String },

    #[error("Git reference {reference} was not found")]
    ReferenceNotFound { reference: String },

    #[error("Git reference {reference} is ambiguous between a branch and tag")]
    AmbiguousReference { reference: String },

    #[error("SHA-256 Git object IDs are not supported")]
    UnsupportedObjectFormat,

    #[error("Git remote has no default branch")]
    MissingDefaultBranch,

    #[error("Git commit {commit} is not available from {remote}")]
    CommitUnavailable { remote: String, commit: GitOid },

    #[error("could not access Git repository {remote}")]
    Access {
        remote: String,
        #[source]
        source: Box<GitProcessError>,
    },

    #[error("Git executable not found; ensure Git is installed and available on PATH")]
    GitNotFound,

    #[error("failed to invoke Git: {0}")]
    Invocation(#[source] std::io::Error),

    #[error(transparent)]
    Process(#[from] Box<GitProcessError>),

    #[error("failed to {operation} {}: {source}", path.display())]
    FileSystem {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Git service is unavailable")]
    Unavailable,

    #[error("failed to join Git task: {0}")]
    Join(#[source] tokio::task::JoinError),
}

#[derive(Debug, Error)]
#[error("failed to {operation} (exit code {exit_code:?})\nstdout:\n{stdout}\nstderr:\n{stderr}")]
pub(crate) struct GitProcessError {
    pub(crate) operation: &'static str,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) async fn resolve(remote: &GitUrl, reference: Option<&str>) -> Result<GitOid, GitError> {
    if let Some(oid) = requested_oid(reference)? {
        return Ok(oid);
    }

    let remote = remote.clone();
    let reference = reference.map(str::to_owned);
    let permit = Arc::clone(&GIT_SEMAPHORE)
        .acquire_owned()
        .await
        .map_err(|_| GitError::Unavailable)?;

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let advertised = advertised_references(&remote)?;
        select_reference(reference.as_deref(), &advertised).map(|selected| selected.commit)
    })
    .await
    .map_err(GitError::Join)?
}

pub(crate) async fn checkout(
    remote: &GitUrl,
    reference: Option<&str>,
    commit: GitOid,
) -> Result<PathBuf, GitError> {
    let remote = remote.clone();
    let reference = reference.map(str::to_owned);
    let permit = Arc::clone(&GIT_SEMAPHORE)
        .acquire_owned()
        .await
        .map_err(|_| GitError::Unavailable)?;
    let span = tracing::info_span!(
        "git_checkout",
        remote = %remote,
        commit = %commit,
        indicatif.pb_show = true,
    );
    span.pb_set_message("fetch Git source");
    span.pb_start();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _entered = span.enter();
        checkout_blocking(&remote, reference.as_deref(), commit, &span)
    })
    .await
    .map_err(GitError::Join)?
}

pub(crate) fn checkout_path(remote: &GitUrl, commit: GitOid) -> PathBuf {
    GitCachePaths::new(remote, commit).checkout
}

fn checkout_blocking(
    remote: &GitUrl,
    reference: Option<&str>,
    commit: GitOid,
    span: &tracing::Span,
) -> Result<PathBuf, GitError> {
    let paths = GitCachePaths::new(remote, commit);
    if valid_checkout(&paths.checkout, commit) {
        return Ok(paths.checkout);
    }

    create_dir_all(&paths.db_parent)?;
    create_dir_all(&paths.checkout_parent)?;
    open_or_initialize_bare(&paths.database)?;
    ensure_commit(&paths.database, remote, reference, commit, span)?;

    if valid_checkout(&paths.checkout, commit) {
        return Ok(paths.checkout);
    }
    if paths.checkout.exists() {
        remove_dir_all_if_exists(&paths.checkout, "remove invalid Git checkout")?;
    }

    let staging = paths.staging_path();
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|source| GitError::FileSystem {
            operation: "remove stale Git staging directory",
            path: staging.clone(),
            source,
        })?;
    }
    let result = populate_checkout(&paths.database, commit, &staging);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    match fs::rename(&staging, &paths.checkout) {
        Ok(()) => Ok(paths.checkout),
        Err(_source) if valid_checkout(&paths.checkout, commit) => {
            let _ = fs::remove_dir_all(staging);
            Ok(paths.checkout)
        }
        Err(source) => {
            let _ = fs::remove_dir_all(staging);
            Err(GitError::FileSystem {
                operation: "publish Git checkout",
                path: paths.checkout,
                source,
            })
        }
    }
}

fn open_or_initialize_bare(path: &Path) -> Result<(), GitError> {
    if path.exists() {
        return validate_bare_repository(path);
    }

    let mut command = git_command();
    command.args(["init", "--bare"]).arg(path);
    match run(command, "initialize Git object database") {
        Ok(_) => Ok(()),
        Err(error) if path.exists() => validate_bare_repository(path).map_err(|_| error),
        Err(error) => Err(error),
    }
}

fn validate_bare_repository(path: &Path) -> Result<(), GitError> {
    let mut command = git_in(path);
    command.args(["rev-parse", "--is-bare-repository"]);
    let output = run(command, "open Git object database")?;
    if String::from_utf8_lossy(&output.stdout).trim() == "true" {
        Ok(())
    } else {
        Err(Box::new(process_error("open Git object database", &output)).into())
    }
}

fn ensure_commit(
    database: &Path,
    remote: &GitUrl,
    reference: Option<&str>,
    commit: GitOid,
    span: &tracing::Span,
) -> Result<(), GitError> {
    if commit_exists(database, commit)? {
        return Ok(());
    }

    let selected_source = if requested_oid(reference)?.is_some() {
        None
    } else {
        advertised_references(remote)
            .and_then(|advertised| select_reference(reference, &advertised))
            .ok()
            .and_then(|selected| selected.source)
    };

    if let Some(source) = selected_source {
        let refspec = format!("+{source}:{FETCHED_REF}");
        let _ = fetch(database, remote, &[refspec], span);
        if commit_exists(database, commit)? {
            return Ok(());
        }
    }

    let direct = format!("+{commit}:{FETCHED_COMMIT_REF}");
    let _ = fetch(database, remote, &[direct], span);
    if commit_exists(database, commit)? {
        return Ok(());
    }

    fetch(
        database,
        remote,
        &[
            "+refs/heads/*:refs/rpx/heads/*".to_string(),
            "+refs/tags/*:refs/rpx/tags/*".to_string(),
        ],
        span,
    )?;
    if commit_exists(database, commit)? {
        Ok(())
    } else {
        Err(GitError::CommitUnavailable {
            remote: remote.to_string(),
            commit,
        })
    }
}

fn fetch(
    database: &Path,
    remote: &GitUrl,
    refspecs: &[String],
    _span: &tracing::Span,
) -> Result<(), GitError> {
    let mut command = remote_git_in(database);
    command
        .args(["fetch", "--no-tags", "--no-write-fetch-head"])
        .arg(remote.as_str())
        .args(refspecs);
    remote_run(command, "fetch Git source", remote).map(|_| ())
}

fn populate_checkout(database: &Path, commit: GitOid, destination: &Path) -> Result<(), GitError> {
    let clone = |no_hardlinks: bool| {
        let mut command = git_command();
        command.env("GIT_LFS_SKIP_SMUDGE", "1");
        command.args(["clone", "--local", "--no-checkout"]);
        if no_hardlinks {
            command.arg("--no-hardlinks");
        }
        command.arg(database).arg(destination);
        run(command, "populate Git checkout")
    };
    if clone(false).is_err() {
        remove_dir_all_if_exists(destination, "clean failed Git checkout")?;
        clone(true)?;
    }

    let mut detach = git_in(destination);
    detach.env("GIT_LFS_SKIP_SMUDGE", "1").args([
        "update-ref",
        "--no-deref",
        "HEAD",
        &commit.to_string(),
    ]);
    run(detach, "detach Git checkout")?;
    let mut reset = git_in(destination);
    reset
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .args(["reset", "--hard", &commit.to_string()]);
    run(reset, "check out Git source").map(|_| ())
}

fn valid_checkout(path: &Path, commit: GitOid) -> bool {
    let mut head = git_in(path);
    head.args(["rev-parse", "HEAD"]);
    let Ok(output) = run(head, "validate Git checkout") else {
        return false;
    };
    if String::from_utf8_lossy(&output.stdout).trim() != commit.to_string() {
        return false;
    }

    let mut status = git_in(path);
    status.args(["status", "--porcelain", "--untracked-files=all"]);
    run(status, "validate Git checkout").is_ok_and(|output| output.stdout.is_empty())
}

#[derive(Debug)]
struct GitCachePaths {
    db_parent: PathBuf,
    checkout_parent: PathBuf,
    database: PathBuf,
    checkout: PathBuf,
}

impl GitCachePaths {
    fn new(remote: &GitUrl, commit: GitOid) -> Self {
        let root = cache_dir_path().join("git");
        let key = remote_key(remote);
        let db_parent = root.join("db");
        let checkout_parent = root.join("checkouts").join(&key);
        Self {
            database: db_parent.join(format!("{key}.git")),
            checkout: checkout_parent.join(commit.to_string()),
            db_parent,
            checkout_parent,
        }
    }

    fn staging_path(&self) -> PathBuf {
        self.checkout_parent.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }
}

fn remote_key(remote: &GitUrl) -> String {
    let canonical = canonical_cache_url(remote);
    let digest = Sha256::digest(canonical.as_bytes());
    let mut key = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut key, "{byte:02x}").expect("writing to a String should succeed");
    }
    key
}

fn canonical_cache_url(remote: &GitUrl) -> String {
    let value = remote.as_str();
    let normalized = if value.contains("://") {
        value.to_string()
    } else if let Some(delimiter) = scp_delimiter(value) {
        format!("ssh://{}/{}", &value[..delimiter], &value[delimiter + 1..])
    } else {
        return value.to_string();
    };
    let Ok(mut url) = reqwest::Url::parse(&normalized) else {
        return value.to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if matches!(
        (url.scheme(), url.port()),
        ("https", Some(443)) | ("ssh", Some(22))
    ) {
        let _ = url.set_port(None);
    }
    if url.path().len() > 1 {
        let path = url.path().trim_end_matches('/').to_string();
        url.set_path(&path);
    }
    url.to_string()
}

fn create_dir_all(path: &Path) -> Result<(), GitError> {
    fs::create_dir_all(path).map_err(|source| GitError::FileSystem {
        operation: "create Git cache directory",
        path: path.to_path_buf(),
        source,
    })
}

fn remove_dir_all_if_exists(path: &Path, operation: &'static str) -> Result<(), GitError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GitError::FileSystem {
            operation,
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug)]
struct AdvertisedReferences {
    refs: Vec<(String, GitOid)>,
    default_branch: Option<String>,
}

#[derive(Debug)]
struct SelectedReference {
    source: Option<String>,
    commit: GitOid,
}

fn advertised_references(remote: &GitUrl) -> Result<AdvertisedReferences, GitError> {
    let mut command = remote_git_command();
    command.args(["ls-remote", "--symref", remote.as_str()]);
    let output = remote_run(command, "list Git references", remote)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut refs = Vec::new();
    let mut default_branch = None;
    for line in stdout.lines() {
        let Some((value, name)) = line.split_once('\t') else {
            continue;
        };
        if name == "HEAD"
            && let Some(branch) = value.strip_prefix("ref: ")
        {
            default_branch = Some(branch.to_string());
            continue;
        }
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitError::UnsupportedObjectFormat);
        }
        if let Ok(oid) = value.parse() {
            refs.push((name.to_string(), oid));
        }
    }
    Ok(AdvertisedReferences {
        refs,
        default_branch,
    })
}

fn select_reference(
    requested: Option<&str>,
    advertised: &AdvertisedReferences,
) -> Result<SelectedReference, GitError> {
    if let Some(oid) = requested_oid(requested)? {
        return Ok(SelectedReference {
            source: None,
            commit: oid,
        });
    }

    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let source = match requested {
        None => advertised
            .default_branch
            .clone()
            .ok_or(GitError::MissingDefaultBranch)?,
        Some(value) if value.starts_with("refs/") => {
            if !valid_reference_name(value)? {
                return Err(GitError::InvalidReference {
                    reference: value.to_string(),
                });
            }
            value.to_string()
        }
        Some(value) => {
            let branch = format!("refs/heads/{value}");
            let tag = format!("refs/tags/{value}");
            if !valid_reference_name(&branch)? || !valid_reference_name(&tag)? {
                return Err(GitError::InvalidReference {
                    reference: value.to_string(),
                });
            }
            let has_branch = advertised.refs.iter().any(|(name, _)| name == &branch);
            let has_tag = advertised.refs.iter().any(|(name, _)| name == &tag);
            match (has_branch, has_tag) {
                (true, false) => branch,
                (false, true) => tag,
                (true, true) => {
                    return Err(GitError::AmbiguousReference {
                        reference: value.to_string(),
                    });
                }
                (false, false) => {
                    return Err(GitError::ReferenceNotFound {
                        reference: value.to_string(),
                    });
                }
            }
        }
    };
    let direct = advertised
        .refs
        .iter()
        .find_map(|(name, oid)| (name == &source).then_some(*oid))
        .ok_or_else(|| GitError::ReferenceNotFound {
            reference: source.clone(),
        })?;
    let peeled_name = format!("{source}^{{}}");
    let commit = advertised
        .refs
        .iter()
        .find_map(|(name, oid)| (name == &peeled_name).then_some(*oid))
        .unwrap_or(direct);

    Ok(SelectedReference {
        source: Some(source),
        commit,
    })
}

fn requested_oid(reference: Option<&str>) -> Result<Option<GitOid>, GitError> {
    let Some(value) = reference.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::UnsupportedObjectFormat);
    }
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return GitOid::from_str(value)
            .map(Some)
            .map_err(|_| GitError::InvalidReference {
                reference: value.to_string(),
            });
    }
    Ok(None)
}

fn valid_reference_name(reference: &str) -> Result<bool, GitError> {
    let mut command = git_command();
    command.args(["check-ref-format", reference]);
    Ok(command_output(command)?.status.success())
}

fn parse_scp_url(value: &str) -> Result<(), GitError> {
    let delimiter = scp_delimiter(value).ok_or(GitError::InvalidUrl)?;
    let authority = &value[..delimiter];
    let path = &value[delimiter + 1..];
    if authority.is_empty()
        || path.is_empty()
        || authority.contains(['/', '\\'])
        || path.starts_with('/')
        || value.contains(['?', '#'])
    {
        return Err(GitError::InvalidUrl);
    }
    let authority_url =
        reqwest::Url::parse(&format!("ssh://{authority}")).map_err(|_| GitError::InvalidUrl)?;
    if authority_url.host_str().is_none() || authority_url.password().is_some() {
        return Err(GitError::CredentialsInUrl);
    }
    if authority_url.username().is_empty() {
        return Err(GitError::SshUsernameRequired);
    }

    Ok(())
}

fn scp_delimiter(value: &str) -> Option<usize> {
    let host_start = value.find('@').map_or(0, |index| index + 1);
    value[host_start..]
        .find(':')
        .map(|index| host_start + index)
}

fn invalid_text(value: &str) -> bool {
    value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    #[cfg(windows)]
    command.args(["-c", "core.longpaths=true"]);
    for variable in GIT_ENVIRONMENT_VARIABLES {
        command.env_remove(variable);
    }
    command
}

fn git_in(path: &Path) -> Command {
    let mut command = git_command();
    command.arg("-C").arg(path);
    command
}

fn remote_git_command() -> Command {
    let mut command = git_command();
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn remote_git_in(path: &Path) -> Command {
    let mut command = remote_git_command();
    command.arg("-C").arg(path);
    command
}

fn command_output(mut command: Command) -> Result<Output, GitError> {
    command.output().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            GitError::GitNotFound
        } else {
            GitError::Invocation(source)
        }
    })
}

fn process_error(operation: &'static str, output: &Output) -> GitProcessError {
    GitProcessError {
        operation,
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run(command: Command, operation: &'static str) -> Result<Output, GitError> {
    let output = command_output(command)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Box::new(process_error(operation, &output)).into())
    }
}

fn remote_run(
    command: Command,
    operation: &'static str,
    remote: &GitUrl,
) -> Result<Output, GitError> {
    let output = command_output(command)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitError::Access {
            remote: remote.to_string(),
            source: Box::new(process_error(operation, &output)),
        })
    }
}

fn commit_exists(database: &Path, commit: GitOid) -> Result<bool, GitError> {
    let mut command = git_in(database);
    command.args(["cat-file", "-e", &format!("{commit}^{{commit}}")]);
    Ok(command_output(command)?.status.success())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("rpx-git-{name}-{}-{unique}", std::process::id()))
    }

    fn test_git(repository: &Path, args: &[&str]) -> Output {
        let mut command = git_in(repository);
        command.args(args);
        run(command, "run test Git command").expect("Git command should succeed")
    }

    pub(crate) fn commit_file(repository: &Path, contents: &str, message: &str) -> GitOid {
        fs::write(repository.join("DESCRIPTION"), contents).expect("file should be written");
        test_git(repository, &["add", "DESCRIPTION"]);
        test_git(repository, &["commit", "-m", message]);
        String::from_utf8(test_git(repository, &["rev-parse", "HEAD"]).stdout)
            .expect("commit should be UTF-8")
            .trim()
            .parse()
            .expect("commit should parse")
    }

    pub(crate) fn source_repository(name: &str) -> (PathBuf, PathBuf, GitOid) {
        let path = temporary_path(name);
        let mut command = git_command();
        command.args(["init", "--initial-branch=main"]).arg(&path);
        run(command, "initialize test repository").expect("repository should initialize");
        test_git(&path, &["config", "user.name", "rpx"]);
        test_git(&path, &["config", "user.email", "rpx@example.com"]);
        let commit = commit_file(&path, "Package: example\nVersion: 1.0.0\n", "initial");
        let repository = path.clone();
        (path, repository, commit)
    }

    fn remove_git_cache(remote: &GitUrl, commit: GitOid) {
        let paths = GitCachePaths::new(remote, commit);
        let _ = fs::remove_dir_all(paths.database);
        let _ = fs::remove_dir_all(paths.checkout_parent);
    }

    #[test]
    fn reads_optional_identity_config_values() {
        let path = temporary_path("identity-config");
        let mut command = git_command();
        command.arg("init").arg(&path);
        run(command, "initialize test repository").expect("repository should initialize");
        test_git(&path, &["config", "user.name", ""]);
        assert_eq!(config_value(&path, "user.name").unwrap(), None);
        test_git(&path, &["config", "user.name", "  Package Author  "]);
        assert_eq!(
            config_value(&path, "user.name").unwrap().as_deref(),
            Some("Package Author")
        );
        fs::remove_dir_all(path).expect("repository should be removed");
    }

    #[test]
    fn git_oid_is_exact_hex_and_canonical_lowercase() {
        let uppercase = "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD";
        let oid = uppercase.parse::<GitOid>().expect("OID should parse");
        assert_eq!(oid.to_string(), uppercase.to_ascii_lowercase());
        assert_eq!(uppercase.parse::<GitOid>().unwrap(), oid);
        assert!("abc".parse::<GitOid>().is_err());
        assert!(
            "gggggggggggggggggggggggggggggggggggggggg"
                .parse::<GitOid>()
                .is_err()
        );
    }

    #[test]
    fn git_command_sanitizes_repository_environment() {
        let command = git_command();
        for variable in GIT_ENVIRONMENT_VARIABLES {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == variable && value.is_none())
            );
        }

        assert!(remote_git_command().get_envs().any(|(name, value)| {
            name == "GIT_TERMINAL_PROMPT" && value.is_some_and(|value| value == "0")
        }));
    }

    #[test]
    fn canonicalizes_credential_free_cache_keys() {
        let scp = GitUrl::from_generic("git@example.com:team/repository.git")
            .expect("SCP URL should parse");
        let ssh = GitUrl::from_generic("ssh://other@example.com:22/team/repository.git/")
            .expect("SSH URL should parse");

        assert_eq!(remote_key(&scp), remote_key(&ssh));
    }

    #[test]
    fn canonicalizes_scp_urls_to_ssh() {
        let remote = GitUrl::from_generic("git@example.com:team/repository.git")
            .expect("SCP URL should parse");

        assert_eq!(remote.as_str(), "ssh://git@example.com/team/repository.git");
    }

    #[test]
    fn builds_hosted_https_urls() {
        let github = "github::owner/repository@main"
            .parse::<Remote>()
            .expect("remote should parse");
        let gitlab = "gitlab@code.example::group/subgroup/repository"
            .parse::<Remote>()
            .expect("remote should parse");
        let bitbucket = "bitbucket::owner/repository/subdirectory"
            .parse::<Remote>()
            .expect("remote should parse");

        assert_eq!(
            GitUrl::try_from(github).expect("URL should build").as_str(),
            "https://github.com/owner/repository.git"
        );
        assert_eq!(
            GitUrl::try_from(gitlab).expect("URL should build").as_str(),
            "https://code.example/group/subgroup/repository.git"
        );
        assert_eq!(
            GitUrl::try_from(bitbucket)
                .expect("URL should build")
                .as_str(),
            "https://bitbucket.org/owner/repository.git"
        );
    }

    #[test]
    fn validates_generic_remote_urls() {
        for value in [
            "git::https://example.com/team/repository.git",
            "git::ssh://git@example.com/team/repository.git",
            "git::git@example.com:team/repository.git",
        ] {
            let remote = value.parse::<Remote>().expect("remote should parse");
            GitUrl::try_from(remote).expect("Git URL should be accepted");
        }

        for value in [
            "git::file:///tmp/repository",
            "git::../repository",
            "git::http://example.com/repository.git",
            "git::https://user@example.com/repository.git",
            "git::ssh://example.com/team/repository.git",
            "git::example.com:team/repository.git",
        ] {
            let remote = value.parse::<Remote>().expect("remote should parse");
            assert!(GitUrl::try_from(remote).is_err(), "{value} should fail");
        }
    }

    #[tokio::test]
    async fn resolves_without_populating_the_persistent_cache() {
        let (source_path, _source, expected) = source_repository("resolve");
        let remote = GitUrl::from_local_path(&source_path);
        let paths = GitCachePaths::new(&remote, expected);

        let actual = resolve(&remote, None).await.expect("ref should resolve");

        assert_eq!(actual, expected);
        assert!(!paths.database.exists());
        assert!(!paths.checkout.exists());
        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn creates_and_reuses_commit_keyed_checkouts() {
        let (source_path, source, initial) = source_repository("persistent-checkout");
        let remote = GitUrl::from_local_path(&source_path);
        remove_git_cache(&remote, initial);

        let first = checkout(&remote, None, initial)
            .await
            .expect("checkout should succeed");
        let second_commit = commit_file(&source, "Package: example\nVersion: 2.0.0\n", "second");
        let second = checkout(&remote, None, second_commit)
            .await
            .expect("second checkout should succeed");
        let reused = checkout(&remote, None, initial)
            .await
            .expect("checkout should be reused");

        assert_eq!(first, reused);
        assert_ne!(first, second);
        assert_eq!(
            fs::read_to_string(first.join("DESCRIPTION")).expect("first file should exist"),
            "Package: example\nVersion: 1.0.0\n"
        );
        assert_eq!(
            fs::read_to_string(second.join("DESCRIPTION")).expect("second file should exist"),
            "Package: example\nVersion: 2.0.0\n"
        );

        remove_git_cache(&remote, initial);
        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn checkout_uses_the_supplied_commit_after_branch_moves() {
        let (source_path, source, initial) = source_repository("branch-moves");
        let remote = GitUrl::from_local_path(&source_path);
        remove_git_cache(&remote, initial);
        let resolved = resolve(&remote, None).await.expect("ref should resolve");
        let _new = commit_file(&source, "Package: example\nVersion: 2.0.0\n", "move branch");

        let path = checkout(&remote, None, resolved)
            .await
            .expect("old commit should checkout");

        assert_eq!(
            fs::read_to_string(path.join("DESCRIPTION")).expect("file should exist"),
            "Package: example\nVersion: 1.0.0\n"
        );
        remove_git_cache(&remote, initial);
        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn repairs_modified_commit_checkout() {
        let (source_path, _source, initial) = source_repository("repair-checkout");
        let remote = GitUrl::from_local_path(&source_path);
        remove_git_cache(&remote, initial);
        let path = checkout(&remote, None, initial)
            .await
            .expect("checkout should succeed");
        fs::write(path.join("DESCRIPTION"), "modified").expect("checkout should be modified");

        let repaired = checkout(&remote, None, initial)
            .await
            .expect("checkout should be repaired");

        assert_eq!(repaired, path);
        assert_eq!(
            fs::read_to_string(repaired.join("DESCRIPTION")).expect("file should exist"),
            "Package: example\nVersion: 1.0.0\n"
        );
        remove_git_cache(&remote, initial);
        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn resolves_branches_tags_and_direct_commits() {
        let (source_path, source, initial) = source_repository("references");
        let remote = GitUrl::from_local_path(&source_path);
        test_git(&source, &["branch", "feature", &initial.to_string()]);
        test_git(&source, &["tag", "v1", &initial.to_string()]);
        test_git(
            &source,
            &[
                "tag",
                "-a",
                "v1-annotated",
                "-m",
                "annotated release",
                &initial.to_string(),
            ],
        );

        for reference in [
            "feature".to_string(),
            "refs/heads/feature".to_string(),
            "v1".to_string(),
            "v1-annotated".to_string(),
            initial.to_string(),
        ] {
            assert_eq!(
                resolve(&remote, Some(&reference))
                    .await
                    .expect("reference should resolve"),
                initial
            );
        }

        fs::remove_dir_all(source_path).expect("source should be removed");
    }
}

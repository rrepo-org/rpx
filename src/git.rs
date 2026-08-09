use crate::project::cache_dir_path;
use git2::{
    AutotagOption, Config, Cred, CredentialType, Direction, FetchOptions, Odb, Oid, ProxyOptions,
    Reference, RemoteCallbacks, RemoteRedirect, Repository, build::CheckoutBuilder,
};
use r_description::lossy::{HostedGitRemote, Remote, RemoteSource};
use sha2::{Digest, Sha256};
use std::{
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing_indicatif::span_ext::IndicatifSpanExt;

const FETCHED_REF: &str = "refs/rpx/source";
const FETCHED_COMMIT_REF: &str = "refs/rpx/commit";

static GIT_SEMAPHORE: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GitUrl(String);

impl GitUrl {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn is_ssh(&self) -> bool {
        self.0.starts_with("ssh://") || !self.0.contains("://")
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
        Ok(Self(value.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn from_local_path(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned())
    }
}

impl TryFrom<&Remote> for GitUrl {
    type Error = GitError;

    fn try_from(remote: &Remote) -> Result<Self, Self::Error> {
        match &remote.source {
            RemoteSource::GitHub(source) => {
                Self::from_hosted(remote.host.as_deref(), "github.com", source)
            }
            RemoteSource::GitLab(source) => {
                Self::from_hosted(remote.host.as_deref(), "gitlab.com", source)
            }
            RemoteSource::Bitbucket(source) => {
                Self::from_hosted(remote.host.as_deref(), "bitbucket.org", source)
            }
            RemoteSource::Git(source) => Self::from_generic(&source.url),
            _ => Err(GitError::UnsupportedRemote),
        }
    }
}

impl fmt::Display for GitUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
pub enum GitError {
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
    CommitUnavailable { remote: String, commit: Oid },

    #[error("failed to {operation}: {source}")]
    Operation {
        operation: &'static str,
        #[source]
        source: git2::Error,
    },

    #[error("failed to {operation} {}: {source}", path.display())]
    FileSystem {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Git cache path is not valid UTF-8: {}", path.display())]
    InvalidCachePath { path: PathBuf },

    #[error("Git service is unavailable")]
    Unavailable,

    #[error("failed to join Git task: {0}")]
    Join(#[source] tokio::task::JoinError),
}

pub(crate) async fn resolve(remote: &GitUrl, reference: Option<&str>) -> Result<Oid, GitError> {
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
        let odb =
            Odb::new().map_err(|source| operation("create in-memory Git database", source))?;
        let repository = Repository::from_odb(odb)
            .map_err(|source| operation("create in-memory Git repository", source))?;
        let mut git_remote = repository
            .remote_anonymous(remote.as_str())
            .map_err(|source| operation("open Git remote", source))?;
        let advertised = advertised_references(&mut git_remote, &remote)?;
        select_reference(reference.as_deref(), &advertised).map(|selected| selected.commit)
    })
    .await
    .map_err(GitError::Join)?
}

pub(crate) async fn checkout(
    remote: &GitUrl,
    reference: Option<&str>,
    commit: Oid,
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

fn checkout_blocking(
    remote: &GitUrl,
    reference: Option<&str>,
    commit: Oid,
    span: &tracing::Span,
) -> Result<PathBuf, GitError> {
    let paths = GitCachePaths::new(remote, commit);
    if valid_checkout(&paths.checkout, commit) {
        return Ok(paths.checkout);
    }

    create_dir_all(&paths.db_parent)?;
    create_dir_all(&paths.checkout_parent)?;
    let database = open_or_initialize_bare(&paths.database)?;
    ensure_commit(&database, remote, reference, commit, span)?;

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
    fs::create_dir(&staging).map_err(|source| GitError::FileSystem {
        operation: "create Git staging directory",
        path: staging.clone(),
        source,
    })?;

    let result = populate_checkout(&database, commit, &staging);
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

fn open_or_initialize_bare(path: &Path) -> Result<Repository, GitError> {
    if path.exists() {
        return Repository::open_bare(path)
            .map_err(|source| operation("open Git object database", source));
    }

    Repository::init_bare(path).or_else(|initialize_error| {
        Repository::open_bare(path).map_err(|open_error| GitError::Operation {
            operation: "initialize Git object database",
            source: if path.exists() {
                open_error
            } else {
                initialize_error
            },
        })
    })
}

fn ensure_commit(
    database: &Repository,
    remote: &GitUrl,
    reference: Option<&str>,
    commit: Oid,
    span: &tracing::Span,
) -> Result<(), GitError> {
    if database.find_commit(commit).is_ok() {
        return Ok(());
    }

    let selected_source = if requested_oid(reference)?.is_some() {
        None
    } else {
        let mut git_remote = database
            .remote_anonymous(remote.as_str())
            .map_err(|source| operation("open Git remote", source))?;
        advertised_references(&mut git_remote, remote)
            .and_then(|advertised| select_reference(reference, &advertised))
            .ok()
            .and_then(|selected| selected.source)
    };

    if let Some(source) = selected_source {
        let refspec = format!("+{source}:{FETCHED_REF}");
        let _ = fetch(database, remote, &[refspec], span);
        if database.find_commit(commit).is_ok() {
            return Ok(());
        }
    }

    let direct = format!("+{commit}:{FETCHED_COMMIT_REF}");
    let _ = fetch(database, remote, &[direct], span);
    if database.find_commit(commit).is_ok() {
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
    database
        .find_commit(commit)
        .map(|_| ())
        .map_err(|_| GitError::CommitUnavailable {
            remote: remote.to_string(),
            commit,
        })
}

fn fetch(
    database: &Repository,
    remote: &GitUrl,
    refspecs: &[String],
    span: &tracing::Span,
) -> Result<(), GitError> {
    let mut git_remote = database
        .remote_anonymous(remote.as_str())
        .map_err(|source| operation("open Git remote", source))?;
    let config = Config::open_default()
        .map_err(|source| operation("read Git credential configuration", source))?;
    let callbacks = remote_callbacks(&config, remote, Some(span.clone()));
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(callbacks)
        .proxy_options(proxy)
        .follow_redirects(RemoteRedirect::None)
        .download_tags(AutotagOption::None)
        .update_fetchhead(false);
    git_remote
        .fetch(refspecs, Some(&mut options), Some("rpx source fetch"))
        .map_err(|source| operation("fetch Git source", source))
}

fn populate_checkout(
    database: &Repository,
    commit: Oid,
    destination: &Path,
) -> Result<(), GitError> {
    let repository = Repository::init(destination)
        .map_err(|source| operation("initialize Git checkout", source))?;
    let database_path = database
        .path()
        .to_str()
        .ok_or_else(|| GitError::InvalidCachePath {
            path: database.path().to_path_buf(),
        })?;
    let mut local = repository
        .remote_anonymous(database_path)
        .map_err(|source| operation("open local Git object database", source))?;
    let refspec = format!("+{commit}:{FETCHED_REF}");
    let mut options = FetchOptions::new();
    options
        .download_tags(AutotagOption::None)
        .update_fetchhead(false);
    local
        .fetch(&[refspec], Some(&mut options), Some("rpx local checkout"))
        .map_err(|source| operation("populate Git checkout", source))?;
    drop(local);

    let commit = repository
        .find_commit(commit)
        .map_err(|source| operation("read checked out Git commit", source))?;
    let commit_id = commit.id();
    let mut checkout = CheckoutBuilder::new();
    checkout.safe().disable_filters(true);
    repository
        .checkout_tree(commit.as_object(), Some(&mut checkout))
        .map_err(|source| operation("check out Git source", source))?;
    repository
        .set_head_detached(commit_id)
        .map_err(|source| operation("detach Git checkout", source))
}

fn valid_checkout(path: &Path, commit: Oid) -> bool {
    let Ok(repository) = Repository::open(path) else {
        return false;
    };
    let correct_head = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .is_ok_and(|head| head.id() == commit);
    if !correct_head {
        return false;
    }

    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    repository
        .statuses(Some(&mut options))
        .is_ok_and(|statuses| statuses.is_empty())
}

#[derive(Debug)]
struct GitCachePaths {
    db_parent: PathBuf,
    checkout_parent: PathBuf,
    database: PathBuf,
    checkout: PathBuf,
}

impl GitCachePaths {
    fn new(remote: &GitUrl, commit: Oid) -> Self {
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
    refs: Vec<(String, Oid)>,
    default_branch: Option<String>,
}

#[derive(Debug)]
struct SelectedReference {
    source: Option<String>,
    commit: Oid,
}

fn advertised_references(
    remote: &mut git2::Remote<'_>,
    remote_url: &GitUrl,
) -> Result<AdvertisedReferences, GitError> {
    let config = Config::open_default()
        .map_err(|source| operation("read Git credential configuration", source))?;
    let callbacks = remote_callbacks(&config, remote_url, None);
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    let connection = remote
        .connect_auth(Direction::Fetch, Some(callbacks), Some(proxy))
        .map_err(|source| operation("connect to Git remote", source))?;
    let refs = connection
        .list()
        .map_err(|source| operation("list Git references", source))?
        .iter()
        .map(|head| (head.name().to_string(), head.oid()))
        .collect();
    let default_branch = connection
        .default_branch()
        .ok()
        .and_then(|branch| std::str::from_utf8(branch.as_ref()).ok().map(str::to_owned));

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
            if !Reference::is_valid_name(value) {
                return Err(GitError::InvalidReference {
                    reference: value.to_string(),
                });
            }
            value.to_string()
        }
        Some(value) => {
            let branch = format!("refs/heads/{value}");
            let tag = format!("refs/tags/{value}");
            if !Reference::is_valid_name(&branch) || !Reference::is_valid_name(&tag) {
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

fn requested_oid(reference: Option<&str>) -> Result<Option<Oid>, GitError> {
    let Some(value) = reference.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::UnsupportedObjectFormat);
    }
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Oid::from_str(value)
            .map(Some)
            .map_err(|_| GitError::InvalidReference {
                reference: value.to_string(),
            });
    }
    Ok(None)
}

fn remote_callbacks<'a>(
    config: &'a Config,
    remote: &'a GitUrl,
    progress_span: Option<tracing::Span>,
) -> RemoteCallbacks<'a> {
    let mut callbacks = RemoteCallbacks::new();
    let expected_url = remote.as_str();
    let mut helper_attempted = false;
    let mut agent_attempted = false;
    let mut username_attempted = false;
    callbacks.credentials(move |callback_url, username, allowed| {
        if !same_git_authority(expected_url, callback_url) {
            return Err(git2::Error::from_str(
                "Git remote requested credentials for an unexpected host",
            ));
        }
        if !remote.is_ssh()
            && allowed.contains(CredentialType::USER_PASS_PLAINTEXT)
            && !helper_attempted
        {
            helper_attempted = true;
            return Cred::credential_helper(config, callback_url, username);
        }
        if remote.is_ssh()
            && allowed.contains(CredentialType::USERNAME)
            && !username_attempted
            && let Some(username) = username
        {
            username_attempted = true;
            return Cred::username(username);
        }
        if remote.is_ssh()
            && allowed.contains(CredentialType::SSH_KEY)
            && !agent_attempted
            && let Some(username) = username
        {
            agent_attempted = true;
            return Cred::ssh_key_from_agent(username);
        }

        Err(git2::Error::from_str(
            "no supported Git credentials are available",
        ))
    });
    if let Some(span) = progress_span {
        callbacks.transfer_progress(move |progress| {
            let total = progress.total_objects() as u64;
            let received = progress.received_objects() as u64;
            if total > 0 {
                span.pb_set_length(total);
                span.pb_set_position(received);
            }
            true
        });
    }

    callbacks
}

fn same_git_authority(expected: &str, actual: &str) -> bool {
    git_authority(expected).is_some_and(|expected| {
        git_authority(actual).is_some_and(|actual| expected.eq_ignore_ascii_case(&actual))
    })
}

fn git_authority(value: &str) -> Option<String> {
    if value.contains("://") {
        let url = reqwest::Url::parse(value).ok()?;
        let host = url.host_str()?;
        let port = url.port().or_else(|| match url.scheme() {
            "https" => Some(443),
            "ssh" => Some(22),
            _ => None,
        });
        return Some(match port {
            Some(port) => format!("{}://{host}:{port}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        });
    }

    let delimiter = scp_delimiter(value)?;
    let authority = &value[..delimiter];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    Some(format!("ssh://{host}:22"))
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

fn operation(operation: &'static str, source: git2::Error) -> GitError {
    GitError::Operation { operation, source }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use git2::{ObjectType, Signature};

    fn temporary_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("rpx-git-{name}-{}-{unique}", std::process::id()))
    }

    pub(crate) fn commit_file(repository: &Repository, contents: &str, message: &str) -> Oid {
        let workdir = repository
            .workdir()
            .expect("repository should have a worktree");
        fs::write(workdir.join("DESCRIPTION"), contents).expect("file should be written");
        let mut index = repository.index().expect("index should open");
        index
            .add_path(Path::new("DESCRIPTION"))
            .expect("file should be added");
        index.write().expect("index should be written");
        let tree_id = index.write_tree().expect("tree should be written");
        let tree = repository.find_tree(tree_id).expect("tree should exist");
        let signature = Signature::now("rpx", "rpx@example.com").expect("signature should build");
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();

        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parent_refs,
            )
            .expect("commit should be created")
    }

    pub(crate) fn source_repository(name: &str) -> (PathBuf, Repository, Oid) {
        let path = temporary_path(name);
        let repository = Repository::init(&path).expect("repository should initialize");
        repository
            .set_head("refs/heads/main")
            .expect("default branch should be main");
        let commit = commit_file(&repository, "Package: example\nVersion: 1.0.0\n", "initial");
        (path, repository, commit)
    }

    fn remove_git_cache(remote: &GitUrl, commit: Oid) {
        let paths = GitCachePaths::new(remote, commit);
        let _ = fs::remove_dir_all(paths.database);
        let _ = fs::remove_dir_all(paths.checkout_parent);
    }

    #[test]
    fn compares_scp_and_ssh_authorities() {
        assert!(same_git_authority(
            "git@example.com:team/repository.git",
            "ssh://git@example.com/team/repository.git"
        ));
        assert!(!same_git_authority(
            "git@example.com:team/repository.git",
            "ssh://git@other.example.com/team/repository.git"
        ));
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
            GitUrl::try_from(&github)
                .expect("URL should build")
                .as_str(),
            "https://github.com/owner/repository.git"
        );
        assert_eq!(
            GitUrl::try_from(&gitlab)
                .expect("URL should build")
                .as_str(),
            "https://code.example/group/subgroup/repository.git"
        );
        assert_eq!(
            GitUrl::try_from(&bitbucket)
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
            GitUrl::try_from(&remote).expect("Git URL should be accepted");
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
            assert!(GitUrl::try_from(&remote).is_err(), "{value} should fail");
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
        let signature = Signature::now("rpx", "rpx@example.com").expect("signature should build");
        source
            .branch(
                "feature",
                &source.find_commit(initial).expect("commit should exist"),
                false,
            )
            .expect("branch should be created");
        source
            .tag_lightweight(
                "v1",
                &source
                    .find_object(initial, Some(ObjectType::Commit))
                    .expect("object should exist"),
                false,
            )
            .expect("tag should be created");
        source
            .tag(
                "v1-annotated",
                &source
                    .find_object(initial, Some(ObjectType::Commit))
                    .expect("object should exist"),
                &signature,
                "annotated release",
                false,
            )
            .expect("annotated tag should be created");

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

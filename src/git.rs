use git2::{
    AutotagOption, Config, Cred, CredentialType, Direction, FetchOptions, Oid, ProxyOptions,
    Reference, RemoteCallbacks, RemoteRedirect, Repository, build::CheckoutBuilder,
};
use r_description::lossy::{HostedGitRemote, Remote, RemoteSource};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing_indicatif::span_ext::IndicatifSpanExt;

const FETCHED_REF: &str = "refs/rpx/source";

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

    #[error("Git checkout destination already exists: {}", path.display())]
    DestinationExists { path: PathBuf },

    #[error("failed to create Git checkout destination at {}: {source}", path.display())]
    DestinationCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "Git checkout failed ({checkout}) and its partial destination at {} could not be removed: {source}",
        path.display()
    )]
    Cleanup {
        path: PathBuf,
        checkout: Box<GitError>,
        #[source]
        source: std::io::Error,
    },

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

    #[error("Git reference {reference} changed while it was being fetched")]
    ReferenceChanged { reference: String },

    #[error("Git reference {reference} does not resolve to a commit")]
    NotACommit { reference: String },

    #[error("failed to {operation}: {source}")]
    Operation {
        operation: &'static str,
        #[source]
        source: git2::Error,
    },

    #[error("Git checkout service is unavailable")]
    Unavailable,

    #[error("failed to join Git checkout task: {0}")]
    Join(#[source] tokio::task::JoinError),
}

pub(crate) async fn checkout(
    remote: &GitUrl,
    reference: Option<&str>,
    destination: &Path,
) -> Result<Oid, GitError> {
    let remote = remote.clone();
    let reference = reference.map(str::to_owned);
    let destination = destination.to_path_buf();
    let permit = Arc::clone(&GIT_SEMAPHORE)
        .acquire_owned()
        .await
        .map_err(|_| GitError::Unavailable)?;
    let span = tracing::info_span!(
        "git_checkout",
        remote = %remote,
        reference = reference.as_deref().unwrap_or("HEAD"),
        indicatif.pb_show = true,
    );
    span.pb_set_message("fetch Git source");
    span.pb_start();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _entered = span.enter();
        checkout_blocking(&remote, reference.as_deref(), &destination, &span)
    })
    .await
    .map_err(GitError::Join)?
}

fn checkout_blocking(
    remote_url: &GitUrl,
    requested: Option<&str>,
    destination: &Path,
    span: &tracing::Span,
) -> Result<Oid, GitError> {
    match fs::create_dir(destination) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(GitError::DestinationExists {
                path: destination.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(GitError::DestinationCreate {
                path: destination.to_path_buf(),
                source,
            });
        }
    }

    match checkout_new_repository(remote_url, requested, destination, span) {
        Ok(commit) => Ok(commit),
        Err(checkout) => match fs::remove_dir_all(destination) {
            Ok(()) => Err(checkout),
            Err(source) => Err(GitError::Cleanup {
                path: destination.to_path_buf(),
                checkout: Box::new(checkout),
                source,
            }),
        },
    }
}

fn checkout_new_repository(
    remote_url: &GitUrl,
    requested: Option<&str>,
    destination: &Path,
    span: &tracing::Span,
) -> Result<Oid, GitError> {
    let repository = Repository::init(destination)
        .map_err(|source| operation("initialize Git checkout", source))?;
    let mut remote = repository
        .remote_anonymous(remote_url.as_str())
        .map_err(|source| operation("open Git remote", source))?;
    let advertised = advertised_references(&mut remote, remote_url)?;
    let resolved = resolve_reference(requested, advertised)?;
    let refspec = format!("+{}:{FETCHED_REF}", resolved.source);
    let config = Config::open_default()
        .map_err(|source| operation("read Git credential configuration", source))?;
    let callbacks = remote_callbacks(&config, remote_url, Some(span.clone()));
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(callbacks)
        .proxy_options(proxy)
        .follow_redirects(RemoteRedirect::None)
        .download_tags(AutotagOption::None)
        .update_fetchhead(false);
    remote
        .fetch(&[&refspec], Some(&mut options), Some("rpx source fetch"))
        .map_err(|source| operation("fetch Git source", source))?;
    drop(remote);

    let fetched = repository
        .find_reference(FETCHED_REF)
        .map_err(|source| operation("read fetched Git reference", source))?;
    let fetched_oid = fetched.target().ok_or_else(|| GitError::NotACommit {
        reference: resolved.label.clone(),
    })?;
    if fetched_oid != resolved.expected {
        return Err(GitError::ReferenceChanged {
            reference: resolved.label,
        });
    }
    let commit = fetched.peel_to_commit().map_err(|_| GitError::NotACommit {
        reference: resolved.label,
    })?;
    let commit_id = commit.id();
    let mut checkout = CheckoutBuilder::new();
    checkout.safe().disable_filters(true);
    repository
        .checkout_tree(commit.as_object(), Some(&mut checkout))
        .map_err(|source| operation("check out Git source", source))?;
    repository
        .set_head_detached(commit_id)
        .map_err(|source| operation("detach Git checkout", source))?;

    Ok(commit_id)
}

#[derive(Debug)]
struct AdvertisedReferences {
    refs: Vec<(String, Oid)>,
    default_branch: Option<String>,
}

#[derive(Debug)]
struct ResolvedReference {
    source: String,
    expected: Oid,
    label: String,
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

fn resolve_reference(
    requested: Option<&str>,
    advertised: AdvertisedReferences,
) -> Result<ResolvedReference, GitError> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    let source = match requested {
        None => advertised
            .default_branch
            .ok_or(GitError::MissingDefaultBranch)?,
        Some(value) if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            return Err(GitError::UnsupportedObjectFormat);
        }
        Some(value) if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            let oid = Oid::from_str(value).map_err(|_| GitError::InvalidReference {
                reference: value.to_string(),
            })?;
            return Ok(ResolvedReference {
                source: value.to_string(),
                expected: oid,
                label: value.to_string(),
            });
        }
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
    let expected = advertised
        .refs
        .iter()
        .find_map(|(name, oid)| (name == &source).then_some(*oid))
        .ok_or_else(|| GitError::ReferenceNotFound {
            reference: source.clone(),
        })?;

    Ok(ResolvedReference {
        label: requested.unwrap_or("HEAD").to_string(),
        source,
        expected,
    })
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
mod tests {
    use super::*;
    use git2::{ObjectType, Signature};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("rpx-git-{name}-{}-{unique}", std::process::id()))
    }

    fn commit_file(repository: &Repository, contents: &str, message: &str) -> Oid {
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

    fn source_repository(name: &str) -> (PathBuf, Repository, Oid) {
        let path = temporary_path(name);
        let repository = Repository::init(&path).expect("repository should initialize");
        repository
            .set_head("refs/heads/main")
            .expect("default branch should be main");
        let commit = commit_file(&repository, "Package: example\nVersion: 1.0.0\n", "initial");
        (path, repository, commit)
    }

    fn local_url(path: &Path) -> GitUrl {
        GitUrl(path.to_string_lossy().into_owned())
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

        let embedded_password = "git::https://user:password@example.com/repository.git"
            .parse::<Remote>()
            .expect("remote should parse");
        assert!(matches!(
            GitUrl::try_from(&embedded_password),
            Err(GitError::CredentialsInUrl)
        ));
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

    #[tokio::test]
    async fn checks_out_the_remote_default_branch() {
        let (source_path, _source, expected) = source_repository("default-branch");
        let destination = temporary_path("default-branch-checkout");

        let actual = checkout(&local_url(&source_path), None, &destination)
            .await
            .expect("checkout should succeed");

        assert_eq!(actual, expected);
        assert_eq!(
            fs::read_to_string(destination.join("DESCRIPTION"))
                .expect("checked out file should exist"),
            "Package: example\nVersion: 1.0.0\n"
        );
        let checkout_repository = Repository::open(&destination).expect("checkout should be Git");
        assert!(
            checkout_repository
                .head_detached()
                .expect("HEAD should load")
        );

        fs::remove_dir_all(source_path).expect("source should be removed");
        fs::remove_dir_all(destination).expect("checkout should be removed");
    }

    #[tokio::test]
    async fn resolves_branches_tags_and_commits() {
        let (source_path, source, initial) = source_repository("references");
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

        for (reference, suffix) in [
            ("feature".to_string(), "branch"),
            ("refs/heads/feature".to_string(), "full-ref"),
            ("v1".to_string(), "tag"),
            ("v1-annotated".to_string(), "annotated-tag"),
            (initial.to_string(), "commit"),
        ] {
            let destination = temporary_path(suffix);
            let actual = checkout(&local_url(&source_path), Some(&reference), &destination)
                .await
                .expect("checkout should succeed");
            assert_eq!(actual, initial);
            fs::remove_dir_all(destination).expect("checkout should be removed");
        }

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn removes_partial_destination_when_reference_is_missing() {
        let (source_path, _source, _) = source_repository("missing-reference");
        let destination = temporary_path("missing-reference-checkout");

        assert!(matches!(
            checkout(
                &local_url(&source_path),
                Some("does-not-exist"),
                &destination
            )
            .await,
            Err(GitError::ReferenceNotFound { .. })
        ));
        assert!(!destination.exists());

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn rejects_ambiguous_short_references() {
        let (source_path, source, initial) = source_repository("ambiguous");
        source
            .branch(
                "release",
                &source.find_commit(initial).expect("commit should exist"),
                false,
            )
            .expect("branch should be created");
        source
            .tag_lightweight(
                "release",
                &source
                    .find_object(initial, Some(ObjectType::Commit))
                    .expect("object should exist"),
                false,
            )
            .expect("tag should be created");
        let destination = temporary_path("ambiguous-checkout");

        assert!(matches!(
            checkout(&local_url(&source_path), Some("release"), &destination).await,
            Err(GitError::AmbiguousReference { .. })
        ));
        assert!(!destination.exists());

        fs::remove_dir_all(source_path).expect("source should be removed");
    }

    #[tokio::test]
    async fn does_not_replace_an_existing_destination() {
        let (source_path, _source, _) = source_repository("existing-destination");
        let destination = temporary_path("existing-destination-checkout");
        fs::create_dir(&destination).expect("destination should be created");

        assert!(matches!(
            checkout(&local_url(&source_path), None, &destination).await,
            Err(GitError::DestinationExists { .. })
        ));

        fs::remove_dir_all(source_path).expect("source should be removed");
        fs::remove_dir_all(destination).expect("destination should be removed");
    }
}

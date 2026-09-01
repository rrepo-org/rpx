use async_trait::async_trait;
use http::Extensions;
use keyring::Entry;
use miette::{Diagnostic, NamedSource, SourceSpan};
use moka::future::Cache;
use r_metadata::{Relation, Version};
use r_packages::{Finding, Packages};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use reqwest_tracing::{
    ReqwestOtelSpanBackend, TracingMiddleware, default_on_request_end, reqwest_otel_span,
};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::IsTerminal;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use target_lexicon::{Aarch64Architecture, Architecture, OperatingSystem, Triple};
use thiserror::Error;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::output::try_prompt;

pub type HttpClient = reqwest_middleware::ClientWithMiddleware;
const KEYRING_SERVICE: &str = "rpx";

static HTTP_CLIENT: LazyLock<HttpClient> = LazyLock::new(|| {
    ClientBuilder::new(reqwest::Client::new())
        .with(AuthMiddleware::new(AuthManager::new()))
        .with(TracingMiddleware::<RpxHttpProgressTrace>::new())
        .build()
});

pub fn client() -> HttpClient {
    HTTP_CLIENT.clone()
}

#[derive(Debug, Clone)]
pub struct AuthScope {
    origin: String,
}

impl AuthScope {
    fn from_url(url: &reqwest::Url) -> Option<Self> {
        let host = url.host_str()?;
        let mut origin = format!("{}://{}", url.scheme(), host);
        if let Some(port) = url.port() {
            origin.push_str(&format!(":{port}"));
        }
        Some(Self { origin })
    }

    fn key(&self) -> String {
        self.origin.clone()
    }
}

#[derive(Debug, Clone)]
pub struct AuthManager {
    tokens: Cache<String, Arc<str>>,
    challenges: Cache<String, Arc<str>>,
    credentials: Arc<dyn CredentialStore>,
    prompter: Arc<dyn ApiKeyPrompter>,
}

impl AuthManager {
    pub fn new() -> Self {
        Self {
            tokens: Cache::new(64),
            challenges: Cache::new(64),
            credentials: Arc::new(KeyringCredentialStore),
            prompter: Arc::new(TerminalApiKeyPrompter),
        }
    }

    async fn token_for_scope(&self, scope: &AuthScope) -> Result<Option<Arc<str>>, AuthError> {
        let key = scope.key();
        if let Some(token) = self.tokens.get(&key).await {
            return Ok(Some(token));
        }

        let Some(token) = self.credentials.get(scope)? else {
            return Ok(None);
        };
        let token = Arc::<str>::from(token);
        self.tokens.insert(key, Arc::clone(&token)).await;
        Ok(Some(token))
    }

    async fn challenge_token(&self, scope: AuthScope) -> Result<Arc<str>, AuthError> {
        let key = scope.key();
        let manager = self.clone();
        let result = self
            .challenges
            .try_get_with(key.clone(), async move {
                manager.prompt_and_store_token(scope).await
            })
            .await
            .map_err(|error| AuthError::Message(error.to_string()));
        self.challenges.invalidate(&key).await;
        result
    }

    async fn prompt_and_store_token(&self, scope: AuthScope) -> Result<Arc<str>, AuthError> {
        let had_stored_token = self.token_for_scope(&scope).await?.is_some();
        let token = self.prompter.prompt(&scope, had_stored_token)?;
        self.credentials.set(&scope, &token)?;
        let token = Arc::<str>::from(token);
        self.tokens.insert(scope.key(), Arc::clone(&token)).await;
        Ok(token)
    }
}

pub trait CredentialStore: Send + Sync + std::fmt::Debug {
    fn get(&self, scope: &AuthScope) -> Result<Option<String>, AuthError>;
    fn set(&self, scope: &AuthScope, token: &str) -> Result<(), AuthError>;
    fn delete(&self, scope: &AuthScope) -> Result<(), AuthError>;
}

pub trait ApiKeyPrompter: Send + Sync + std::fmt::Debug {
    fn prompt(&self, scope: &AuthScope, had_stored_token: bool) -> Result<String, AuthError>;
}

#[derive(Debug, Clone)]
pub struct KeyringCredentialStore;

#[derive(Debug, Clone)]
pub struct TerminalApiKeyPrompter;

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, scope: &AuthScope) -> Result<Option<String>, AuthError> {
        let Ok(entry) = keyring_entry(scope) else {
            return Ok(None);
        };

        match entry.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) | Err(_) => Ok(None),
        }
    }

    fn set(&self, scope: &AuthScope, token: &str) -> Result<(), AuthError> {
        keyring_entry(scope)?.set_password(token).map_err(|error| {
            AuthError::Message(format!(
                "failed to store API key for {}: {error}",
                scope.origin
            ))
        })
    }

    fn delete(&self, scope: &AuthScope) -> Result<(), AuthError> {
        match keyring_entry(scope)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AuthError::Message(format!(
                "failed to remove stored API key for {}: {error}",
                scope.origin
            ))),
        }
    }
}

impl ApiKeyPrompter for TerminalApiKeyPrompter {
    fn prompt(&self, scope: &AuthScope, had_stored_token: bool) -> Result<String, AuthError> {
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Err(AuthError::Message(format!(
                "{} requires an API key, but no interactive terminal is available",
                scope.origin
            )));
        }

        let prompt = if had_stored_token {
            format!(
                "Stored API key rejected for {}. Enter a new API key: ",
                scope.origin
            )
        } else {
            format!("API key required for {}: ", scope.origin)
        };

        try_prompt(prompt).map_err(|error| {
            AuthError::Message(format!("failed to prompt for API key: {error}"))
        })?;

        let token = rpassword::read_password()
            .map_err(|error| AuthError::Message(format!("failed to read API key: {error}")))?;
        let token = token.trim().to_string();

        if token.is_empty() {
            return Err(AuthError::Message("API key cannot be empty".to_string()));
        }

        Ok(token)
    }
}

#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct AuthMiddlewareError(String);

#[derive(Debug, Clone, Error)]
pub enum AuthError {
    #[error("{0}")]
    Message(String),
}

impl From<AuthError> for AuthMiddlewareError {
    fn from(error: AuthError) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone)]
struct AuthMiddleware {
    auth: AuthManager,
}

impl AuthMiddleware {
    fn new(auth: AuthManager) -> Self {
        Self { auth }
    }
}

#[async_trait]
impl Middleware for AuthMiddleware {
    async fn handle(
        &self,
        mut req: reqwest::Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        let Some(scope) = AuthScope::from_url(req.url()) else {
            return next.run(req, extensions).await;
        };

        let retry_request = req.try_clone();
        if let Some(token) = self
            .auth
            .token_for_scope(&scope)
            .await
            .map_err(AuthMiddlewareError::from)
            .map_err(reqwest_middleware::Error::middleware)?
        {
            set_bearer_token(&mut req, &token)?;
        }

        let response = next.clone().run(req, extensions).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let Some(mut retry_request) = retry_request else {
            return Ok(response);
        };

        let token = self
            .auth
            .challenge_token(scope)
            .await
            .map_err(AuthMiddlewareError::from)
            .map_err(reqwest_middleware::Error::middleware)?;
        set_bearer_token(&mut retry_request, &token)?;
        next.run(retry_request, extensions).await
    }
}

fn set_bearer_token(request: &mut reqwest::Request, token: &str) -> reqwest_middleware::Result<()> {
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
        reqwest_middleware::Error::middleware(AuthMiddlewareError(error.to_string()))
    })?;
    request.headers_mut().insert(AUTHORIZATION, value);
    Ok(())
}

pub fn remove_stored_credential(base_url: &reqwest::Url) -> Result<(), AuthError> {
    let Some(scope) = AuthScope::from_url(base_url) else {
        return Ok(());
    };
    KeyringCredentialStore.delete(&scope)
}

fn keyring_entry(scope: &AuthScope) -> Result<Entry, AuthError> {
    Entry::new(KEYRING_SERVICE, &keyring_account_name(scope))
        .map_err(|error| AuthError::Message(format!("failed to access local keyring: {error}")))
}

fn keyring_account_name(scope: &AuthScope) -> String {
    format!("host:{}", hash_string(&scope.key()))
}

fn hash_string(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

struct RpxHttpProgressTrace;

impl ReqwestOtelSpanBackend for RpxHttpProgressTrace {
    fn on_request_start(req: &reqwest::Request, _extension: &mut Extensions) -> Span {
        let message = request_progress_message(req);
        let span = reqwest_otel_span!(
            name = "http_request",
            req,
            url.full = %display_safe_url(req.url()),
            indicatif.pb_show = true,
        );
        span.pb_set_message(&message);
        span.pb_start();
        span
    }

    fn on_request_end(
        span: &Span,
        outcome: &reqwest_middleware::Result<reqwest::Response>,
        _extension: &mut Extensions,
    ) {
        default_on_request_end(span, outcome);
    }
}

fn request_progress_message(req: &reqwest::Request) -> String {
    format!("{} {}", req.method(), req.url().path())
}

pub(crate) fn display_safe_url(url: &reqwest::Url) -> reqwest::Url {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CranPackagesIndex {
    pub packages: Vec<CranPackageIndexEntry>,
}

#[derive(Clone, Debug, Error, Diagnostic)]
#[error("failed to parse CRAN PACKAGES index ({count} errors)")]
#[diagnostic(
    code(rpx::repository::cran_packages_parse_failed),
    help(
        "The repository returned invalid metadata. Try another mirror or contact the repository maintainer."
    )
)]
pub struct CranPackagesParseError {
    count: usize,

    #[source_code]
    source_code: NamedSource<String>,

    #[related]
    issues: Vec<CranPackagesParseIssue>,
}

impl CranPackagesParseError {
    fn new(
        source_name: impl Into<String>,
        source: String,
        issues: Vec<CranPackagesParseIssue>,
    ) -> Self {
        let source_name = source_name.into();
        Self {
            count: issues.len(),
            source_code: NamedSource::new(source_name, source),
            issues,
        }
    }
}

#[derive(Clone, Debug, Error, Diagnostic)]
pub enum CranPackagesParseIssue {
    #[error("{finding}")]
    Finding {
        finding: Finding,
        #[label("{finding}")]
        span: SourceSpan,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CranPackageIndexEntry {
    pub package: String,
    pub version: Version,
    pub depends: Vec<Relation>,
    pub imports: Vec<Relation>,
    pub suggests: Vec<Relation>,
    pub linking_to: Vec<Relation>,
}

impl CranPackagesIndex {
    pub fn parse(
        source_name: impl Into<String>,
        source: String,
    ) -> Result<Self, CranPackagesParseError> {
        let parsed = Packages::parse(&source);
        let issues = parsed
            .validate()
            .into_iter()
            .map(|finding| {
                let range = finding.span();
                CranPackagesParseIssue::Finding {
                    span: (range.start..range.end).into(),
                    finding,
                }
            })
            .collect::<Vec<_>>();

        if !issues.is_empty() {
            return Err(CranPackagesParseError::new(source_name, source, issues));
        }

        let packages = parsed
            .records()
            .filter_map(|record| cran_package_index_entry_from_validated_record(&record))
            .collect();
        Ok(Self { packages })
    }
}

fn cran_package_index_entry_from_validated_record(
    record: &r_packages::PackageRecord,
) -> Option<CranPackageIndexEntry> {
    Some(CranPackageIndexEntry {
        package: record.package()?.as_str().to_owned(),
        version: record.parsed_version()?.ok()?,
        depends: relation_values(record.parsed_depends()),
        imports: relation_values(record.parsed_imports()),
        suggests: relation_values(record.parsed_suggests()),
        linking_to: relation_values(record.parsed_linking_to()),
    })
}

fn relation_values(relations: Option<r_metadata::RelationList>) -> Vec<Relation> {
    relations.map_or_else(Vec::new, |relations| {
        relations
            .entries()
            .iter()
            .map(|entry| entry.value.clone())
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CranPackageArchiveListing {
    pub versions: Vec<Version>,
}

impl FromStr for CranPackageArchiveListing {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut versions = Vec::new();
        for part in archive_listing_parts(input) {
            let file_name = part.rsplit('/').next().unwrap_or(part);
            if !file_name.ends_with(".tar.gz") || !file_name.contains('_') {
                continue;
            }

            let file_name = html_unescape_minimal(file_name);
            let stem = file_name
                .strip_suffix(".tar.gz")
                .expect("archive file name was checked for tar.gz suffix");
            let Some((package, version)) = stem.rsplit_once('_') else {
                continue;
            };
            if package.is_empty() || version.is_empty() {
                continue;
            }

            let version = version
                .parse::<Version>()
                .map_err(|error| error.to_string())?;
            if !versions.iter().any(|seen| seen == &version) {
                versions.push(version);
            }
        }

        Ok(Self { versions })
    }
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct RrepoPackagesResponse {
    #[serde(rename = "repositorySlug")]
    pub repository_slug: String,
    pub packages: Vec<RrepoPackageSummary>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct RrepoPackageSummary {
    pub name: String,
    #[serde(rename = "latestVersion")]
    pub latest_version: String,
    #[serde(rename = "latestUploadedAt")]
    pub latest_uploaded_at: Option<String>,
    #[serde(rename = "versionCount")]
    pub version_count: Option<usize>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct RrepoPackageVersionsResponse {
    pub package: String,
    pub versions: Vec<RrepoVersionSummary>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct RrepoVersionSummary {
    pub version: String,
    #[serde(rename = "sourceUrl")]
    pub source_url: String,
}

fn archive_listing_parts(listing: &str) -> impl Iterator<Item = &str> {
    listing.split(['"', '\'', '<', '>', ' ', '\n', '\r', '\t'])
}

fn html_unescape_minimal(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

pub async fn rrepo_repository_packages(
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .push("packages");

    client().get(url).send().await
}

pub async fn rrepo_package_versions(
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions"]);

    client().get(url).send().await
}

pub async fn rrepo_package_description(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions", version, "description"]);

    client().get(url).send().await
}

pub async fn rrepo_source_artifact(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions", version, "source"]);

    client().get(url).send().await
}

#[derive(Debug, Error)]
pub enum BinaryArtifactRequestError {
    #[error("binary artifacts are not supported for target {target}")]
    UnsupportedTarget { target: Triple },
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
}

pub(crate) fn r_macos_binary_target(
    target: &Triple,
) -> Result<&'static str, BinaryArtifactRequestError> {
    match (target.operating_system, target.architecture) {
        (
            OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_),
            Architecture::Aarch64(Aarch64Architecture::Aarch64),
        ) => Ok("big-sur-arm64"),
        (OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_), Architecture::X86_64) => {
            Ok("big-sur-x86_64")
        }
        _ => Err(BinaryArtifactRequestError::UnsupportedTarget {
            target: target.clone(),
        }),
    }
}

pub async fn rrepo_binary(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
    target: &Triple,
    r_version: &semver::Version,
) -> Result<reqwest::Response, BinaryArtifactRequestError> {
    let mut url = base_url.clone();
    let r_version = format!("{}.{}", r_version.major, r_version.minor);
    let target_path = match target.operating_system {
        OperatingSystem::Windows => vec!["windows", r_version.as_str()],
        OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => {
            vec!["macos", r_macos_binary_target(target)?, r_version.as_str()]
        }
        _ => {
            return Err(BinaryArtifactRequestError::UnsupportedTarget {
                target: target.clone(),
            });
        }
    };
    let mut segments = url
        .path_segments_mut()
        .expect("repository base URL should support path segments");
    segments
        .pop_if_empty()
        .extend(["packages", package, "versions", version, "binaries"]);
    segments.extend(target_path);
    drop(segments);

    Ok(client().get(url).send().await?)
}

pub async fn cran_packages(
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "PACKAGES"]);

    client().get(url).send().await
}

pub async fn cran_archive_root(
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", ""]);

    client().get(url).send().await
}

pub async fn cran_package_archive_listing(
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", package, ""]);

    client().get(url).send().await
}

pub async fn cran_current_source_tarball(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let file_name = format!("{package}_{version}.tar.gz");
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", &file_name]);

    client().get(url).send().await
}

pub async fn cran_archive_source_tarball(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let file_name = format!("{package}_{version}.tar.gz");
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", package, &file_name]);

    client().get(url).send().await
}

#[allow(dead_code)]
pub async fn cran_latest_package_description(
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["web", "packages", package, "DESCRIPTION"]);

    client().get(url).send().await
}

pub async fn cran_binary(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
    target: &Triple,
    r_version: &semver::Version,
) -> Result<reqwest::Response, BinaryArtifactRequestError> {
    let r_version = format!("{}.{}", r_version.major, r_version.minor);
    let file_name;
    let target_path = match target.operating_system {
        OperatingSystem::Windows => {
            file_name = format!("{package}_{version}.zip");
            vec!["windows", "contrib", r_version.as_str(), file_name.as_str()]
        }
        OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => {
            file_name = format!("{package}_{version}.tgz");
            vec![
                "macosx",
                r_macos_binary_target(target)?,
                "contrib",
                r_version.as_str(),
                file_name.as_str(),
            ]
        }
        _ => {
            return Err(BinaryArtifactRequestError::UnsupportedTarget {
                target: target.clone(),
            });
        }
    };
    let mut url = base_url.clone();
    let mut segments = url
        .path_segments_mut()
        .expect("repository base URL should support path segments");
    segments.pop_if_empty().extend(["bin"]);
    segments.extend(target_path);
    drop(segments);

    Ok(client().get(url).send().await?)
}

#[cfg(test)]
mod tests {
    use miette::SourceSpan;

    use super::{
        BinaryArtifactRequestError, CranPackagesIndex, CranPackagesParseError,
        CranPackagesParseIssue, display_safe_url, r_macos_binary_target,
    };

    fn parse_packages_index(input: &str) -> Result<CranPackagesIndex, CranPackagesParseError> {
        CranPackagesIndex::parse("CRAN PACKAGES fixture", input.to_string())
    }

    #[test]
    fn display_safe_url_removes_credentials_query_and_fragment() {
        let url = reqwest::Url::parse(
            "https://user:password@example.test/repository/src/contrib/PACKAGES?token=secret#part",
        )
        .expect("URL fixture should parse");

        assert_eq!(
            display_safe_url(&url).as_str(),
            "https://example.test/repository/src/contrib/PACKAGES"
        );
    }

    #[test]
    fn derives_r_macos_binary_targets_from_target_lexicon() {
        let arm = "aarch64-apple-darwin".parse().unwrap();
        let x86 = "x86_64-apple-darwin".parse().unwrap();
        let linux = "x86_64-unknown-linux-gnu".parse().unwrap();

        assert_eq!(r_macos_binary_target(&arm).unwrap(), "big-sur-arm64");
        assert_eq!(r_macos_binary_target(&x86).unwrap(), "big-sur-x86_64");
        assert!(matches!(
            r_macos_binary_target(&linux),
            Err(BinaryArtifactRequestError::UnsupportedTarget { .. })
        ));
    }

    #[test]
    fn parses_azurestor_trailing_depends_comma() {
        let index = "Package: AzureStor\nVersion: 3.7.1\nDepends: R (>= 3.3),\n";

        let index = parse_packages_index(index).expect("trailing commas should be accepted");

        assert_eq!(index.packages.len(), 1);
        assert_eq!(index.packages[0].package, "AzureStor");
        assert_eq!(index.packages[0].depends[0].to_string(), "R (>= 3.3)");
    }

    #[test]
    fn packages_fields_are_case_sensitive_and_last_relation_field_wins() {
        let index = parse_packages_index(
            "package: ignored\nPackage: first\nVersion: 2.0.0\nImports: old\nImports: current\n",
        )
        .expect("the final exact-case fields should be selected");

        assert_eq!(index.packages[0].package, "first");
        assert_eq!(index.packages[0].version.to_string(), "2.0.0");
        assert_eq!(index.packages[0].imports[0].package(), "current");

        let error = parse_packages_index("package: wrong-case\nVersion: 1.0.0\n")
            .expect_err("wrong-case Package should remain missing");
        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, .. }
                if finding.kind() == r_packages::FindingKind::MissingPackage
        )));
    }

    #[test]
    fn rejects_duplicate_scalar_fields() {
        let error = parse_packages_index("Package: example\nVersion: 1.0.0\nVersion: 2.0.0\n")
            .expect_err("duplicate scalar fields should be rejected");

        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, .. }
                if finding.kind() == r_packages::FindingKind::DuplicateScalarField
        )));
    }

    #[test]
    fn surfaces_malformed_nonempty_cran_package_relations() {
        let error =
            parse_packages_index("Package: example\nVersion: 1.0.0\nImports: cli (>= invalid),\n")
                .expect_err("malformed nonempty relations should be rejected");

        assert!(matches!(
            error.issues.as_slice(),
            [CranPackagesParseIssue::Finding { finding, .. }]
                if finding.kind() == r_packages::FindingKind::InvalidRelation
                    && finding.field_name() == Some("Imports")
        ));
    }

    #[test]
    fn rejects_empty_relations_except_for_one_trailing_comma() {
        let error = parse_packages_index(
            "Package: first\nVersion: 1.0.0\nImports: cli,,digest\n\nPackage: second\nVersion: 2.0.0\nImports: cli,,\n",
        )
            .expect_err("internal and repeated empty relations should be rejected");

        assert_eq!(
            error
                .issues
                .iter()
                .filter(|issue| matches!(
                    issue,
                    CranPackagesParseIssue::Finding { finding, .. }
                        if finding.kind() == r_packages::FindingKind::InvalidRelation
                            && finding.field_name() == Some("Imports")
                ))
                .count(),
            2
        );
    }

    #[test]
    fn aggregates_invalid_fields_across_package_records() {
        let error = parse_packages_index(
            "Version: invalid\nImports: cli,,digest\nSuggests: testthat\nSuggests: knitr\n\nPackage: valid\nVersion: 2.0.0\n",
        )
            .expect_err("every invalid package field should be reported");

        assert_eq!(error.count, 3);
        assert_eq!(error.issues.len(), 3);
        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, .. }
                if finding.kind() == r_packages::FindingKind::MissingPackage
        )));
        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, .. }
                if finding.kind() == r_packages::FindingKind::InvalidVersion
        )));
        assert_eq!(
            error
                .issues
                .iter()
                .filter(|issue| matches!(
                    issue,
                    CranPackagesParseIssue::Finding { finding, .. }
                        if finding.kind() == r_packages::FindingKind::InvalidRelation
                ))
                .count(),
            1
        );
    }

    #[test]
    fn surfaces_invalid_package_and_version_with_source_spans() {
        let error = parse_packages_index("Package: _bad\nVersion: nope\n")
            .expect_err("invalid package identity metadata should be rejected");

        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, span }
                if finding.kind() == r_packages::FindingKind::InvalidPackageName
                    && *span == SourceSpan::from(8..13)
        )));
        assert!(error.issues.iter().any(|issue| matches!(
            issue,
            CranPackagesParseIssue::Finding { finding, span }
                if finding.kind() == r_packages::FindingKind::InvalidVersion
                    && *span == SourceSpan::from(22..27)
        )));
    }

    #[test]
    fn rejects_structurally_invalid_packages_before_reading_records() {
        let error = parse_packages_index("Package example\nVersion: 1.0.0\n")
            .expect_err("invalid DCF syntax should reject the complete index");

        assert!(
            error
                .issues
                .iter()
                .any(|issue| matches!(issue, CranPackagesParseIssue::Finding { .. }))
        );
    }
}

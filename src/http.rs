#![allow(dead_code)]

use async_trait::async_trait;
use flate2::read::GzDecoder;
use http::Extensions;
use keyring::Entry;
use miette::Diagnostic;
use moka::future::Cache;
use r_description::lossless::{Relations, Version};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use reqwest_tracing::{
    ReqwestOtelSpanBackend, TracingMiddleware, default_on_request_end, reqwest_otel_span,
};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Cursor, IsTerminal, Read};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::output::try_prompt;

pub type HttpClient = reqwest_middleware::ClientWithMiddleware;
const KEYRING_SERVICE: &str = "rpx";

pub fn client() -> HttpClient {
    ClientBuilder::new(reqwest::Client::new())
        .with(AuthMiddleware::new(AuthManager::new()))
        .with(TracingMiddleware::<RpxHttpProgressTrace>::new())
        .build()
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

pub fn has_stored_credential(base_url: &reqwest::Url) -> Result<bool, AuthError> {
    let Some(scope) = AuthScope::from_url(base_url) else {
        return Ok(false);
    };
    KeyringCredentialStore
        .get(&scope)
        .map(|token| token.is_some())
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
            url.full = %remove_credentials(req.url()),
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

fn remove_credentials(url: &reqwest::Url) -> reqwest::Url {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url
}

pub struct ArtifactResponse {
    pub content_length: Option<u64>,
    pub stream:
        Pin<Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CranPackagesIndex {
    pub packages: Vec<CranPackageIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CranPackageIndexEntry {
    pub package: String,
    pub version: String,
    pub depends: Relations,
    pub imports: Relations,
    pub suggests: Relations,
    pub linking_to: Relations,
    pub system_requirements: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CranArchiveRootListing {
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CranPackageArchiveListing {
    pub versions: Vec<Version>,
}

impl FromStr for CranPackagesIndex {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let reader = Cursor::new(input);
        let packages = deb822_fast::ParagraphReader::new(reader)
            .map(|paragraph| {
                paragraph
                    .map_err(|error| error.to_string())
                    .and_then(cran_package_index_entry_from_paragraph)
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self { packages })
    }
}

impl FromStr for CranArchiveRootListing {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut packages = Vec::new();
        for part in archive_listing_parts(input) {
            if part.starts_with('/') || !part.ends_with('/') || part.contains('?') {
                continue;
            }

            let package = part.trim_end_matches('/');
            if package.is_empty() || packages.iter().any(|seen| seen == package) {
                continue;
            }

            packages.push(html_unescape_minimal(package));
        }

        Ok(Self { packages })
    }
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

            let version = version.parse()?;
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

#[derive(Debug, Error, Diagnostic)]
pub enum HttpError {
    #[error("failed to send request to {url}: {source}")]
    #[diagnostic(code(rpx::http::request_failed))]
    RequestFailed {
        url: reqwest::Url,
        #[source]
        source: reqwest_middleware::Error,
    },

    #[error("unexpected response from {url}: {status}")]
    #[diagnostic(code(rpx::http::unexpected_status))]
    UnexpectedStatus {
        url: reqwest::Url,
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("failed to decode JSON response from {url}: {source}")]
    #[diagnostic(code(rpx::http::json_decode_failed))]
    JsonDecodeFailed {
        url: reqwest::Url,
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to extract source package from {url}: {details}")]
    #[diagnostic(code(rpx::http::artifact_extract_failed))]
    ArtifactExtractFailed { url: reqwest::Url, details: String },

    #[error("failed to read response body from {url}: {source}")]
    #[diagnostic(code(rpx::http::body_read_failed))]
    BodyReadFailed {
        url: reqwest::Url,
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to parse DESCRIPTION response from {url}: {details}")]
    #[diagnostic(code(rpx::http::description_parse_failed))]
    DescriptionParseFailed { url: reqwest::Url, details: String },

    #[error("failed to decompress PACKAGES.gz response from {url}: {source}")]
    #[diagnostic(code(rpx::http::packages_decompress_failed))]
    PackagesDecompressFailed {
        url: reqwest::Url,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse PACKAGES response from {url}: {details}")]
    #[diagnostic(code(rpx::http::packages_parse_failed))]
    PackagesParseFailed { url: reqwest::Url, details: String },

    #[error("failed to parse archive listing response from {url}: {details}")]
    #[diagnostic(code(rpx::http::archive_listing_parse_failed))]
    ArchiveListingParseFailed { url: reqwest::Url, details: String },
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

fn cran_package_index_entry_from_paragraph(
    paragraph: deb822_fast::Paragraph,
) -> Result<CranPackageIndexEntry, String> {
    let package = required_packages_field(&paragraph, "Package")?;
    let version = required_packages_field(&paragraph, "Version")?;

    Ok(CranPackageIndexEntry {
        package,
        version,
        depends: parse_packages_relations_field(&paragraph, "Depends")?,
        imports: parse_packages_relations_field(&paragraph, "Imports")?,
        suggests: parse_packages_relations_field(&paragraph, "Suggests")?,
        linking_to: parse_packages_relations_field(&paragraph, "LinkingTo")?,
        system_requirements: paragraph
            .get("SystemRequirements")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    })
}

fn required_packages_field(
    paragraph: &deb822_fast::Paragraph,
    field: &'static str,
) -> Result<String, String> {
    paragraph
        .get(field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("missing required field {field}"))
}

fn parse_packages_relations_field(
    paragraph: &deb822_fast::Paragraph,
    field: &'static str,
) -> Result<Relations, String> {
    paragraph
        .get(field)
        .map(str::parse)
        .transpose()
        .map_err(|details| format!("failed to parse {field}: {details}"))
        .map(Option::unwrap_or_default)
}

async fn get_response(
    client: &HttpClient,
    url: reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    client.get(url.clone()).send().await
}

async fn artifact_response(
    client: &HttpClient,
    url: reqwest::Url,
) -> Result<ArtifactResponse, HttpError> {
    let response =
        client
            .get(url.clone())
            .send()
            .await
            .map_err(|source| HttpError::RequestFailed {
                url: url.clone(),
                source,
            })?;

    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let body = response.text().await.unwrap_or_default();
        return Err(HttpError::UnexpectedStatus { url, status, body });
    }

    Ok(ArtifactResponse {
        content_length: response.content_length(),
        stream: Box::pin(response.bytes_stream()),
    })
}

pub async fn rrepo_repository_packages(
    client: &HttpClient,
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .push("packages");

    get_response(client, url).await
}

pub async fn rrepo_package_versions(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions"]);

    get_response(client, url).await
}

pub async fn rrepo_package_description(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions", version, "description"]);

    get_response(client, url).await
}

pub async fn rrepo_source_artifact(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["packages", package, "versions", version, "source"]);

    get_response(client, url).await
}

pub async fn rrepo_windows_binary(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
    r_minor: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend([
            "packages", package, "versions", version, "binaries", "windows", r_minor,
        ]);

    get_response(client, url).await
}

pub async fn rrepo_macos_binary(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
    target: &str,
    r_minor: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend([
            "packages", package, "versions", version, "binaries", "macos", target, r_minor,
        ]);

    get_response(client, url).await
}

pub async fn cran_packages(
    client: &HttpClient,
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "PACKAGES"]);

    client.get(url).send().await
}

pub async fn cran_archive_root(
    client: &HttpClient,
    base_url: &reqwest::Url,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", ""]);

    client.get(url).send().await
}

pub async fn cran_package_archive_listing(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", package, ""]);

    client.get(url).send().await
}

pub async fn cran_current_source_tarball(
    client: &HttpClient,
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

    client.get(url).send().await
}

pub async fn cran_archive_source_tarball(
    client: &HttpClient,
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

    client.get(url).send().await
}

pub async fn cran_latest_package_description(
    client: &HttpClient,
    base_url: &reqwest::Url,
    package: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["web", "packages", package, "DESCRIPTION"]);

    client.get(url).send().await
}

pub async fn cran_windows_binary(
    client: &HttpClient,
    base_url: &reqwest::Url,
    r_minor: &str,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let file_name = format!("{package}_{version}.zip");
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["bin", "windows", "contrib", r_minor, &file_name]);

    client.get(url).send().await
}

pub async fn cran_macos_binary(
    client: &HttpClient,
    base_url: &reqwest::Url,
    target: &str,
    r_minor: &str,
    package: &str,
    version: &str,
) -> Result<reqwest::Response, reqwest_middleware::Error> {
    let file_name = format!("{package}_{version}.tgz");
    let mut url = base_url.clone();
    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["bin", "macosx", target, "contrib", r_minor, &file_name]);

    client.get(url).send().await
}

async fn description_body_from_source_artifact(
    mut artifact: ArtifactResponse,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<String, HttpError> {
    use futures_util::TryStreamExt;

    let mut bytes = Vec::with_capacity(artifact.content_length.unwrap_or_default() as usize);

    while let Some(chunk) =
        artifact
            .stream
            .try_next()
            .await
            .map_err(|source| HttpError::BodyReadFailed {
                url: cran_archive_source_url_for_error(base_url, package, version),
                source,
            })?
    {
        bytes.extend_from_slice(&chunk);
    }

    description_body_from_tar_gz_bytes(&bytes, base_url, package, version)
}

fn description_body_from_tar_gz_bytes(
    bytes: &[u8],
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<String, HttpError> {
    let url = cran_archive_source_url_for_error(base_url, package, version);
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|source| HttpError::ArtifactExtractFailed {
            url: url.clone(),
            details: format!("failed to read source package archive: {source}"),
        })?;

    for entry in entries {
        let mut entry = entry.map_err(|source| HttpError::ArtifactExtractFailed {
            url: url.clone(),
            details: format!("failed to read archive entry: {source}"),
        })?;

        let is_description = {
            let path = entry
                .path()
                .map_err(|source| HttpError::ArtifactExtractFailed {
                    url: url.clone(),
                    details: format!("failed to read archive entry path: {source}"),
                })?;

            path_is_top_level_description(&path, package)
        };

        if !is_description {
            continue;
        }

        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .map_err(|source| HttpError::ArtifactExtractFailed {
                url: url.clone(),
                details: format!("failed to read DESCRIPTION from source package: {source}"),
            })?;

        return Ok(body);
    }

    Err(HttpError::ArtifactExtractFailed {
        url,
        details: format!("source package does not contain {package}/DESCRIPTION"),
    })
}

fn parse_cran_description_body(
    body: &str,
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> Result<r_description::lossy::RDescription, HttpError> {
    let url = cran_archive_source_url_for_error(base_url, package, version);

    let description = r_description::lossy::RDescription::from_str(body).map_err(|details| {
        HttpError::DescriptionParseFailed {
            url: url.clone(),
            details,
        }
    })?;

    Ok(description)
}

fn path_is_top_level_description(path: &std::path::Path, package: &str) -> bool {
    let mut components = path.components().filter_map(|component| {
        let component = component.as_os_str().to_str()?;
        (component != ".").then_some(component)
    });

    components.next() == Some(package)
        && components.next() == Some("DESCRIPTION")
        && components.next().is_none()
}

fn cran_archive_source_url_for_error(
    base_url: &reqwest::Url,
    package: &str,
    version: &str,
) -> reqwest::Url {
    let file_name = format!("{package}_{version}.tar.gz");
    let mut url = base_url.clone();

    url.path_segments_mut()
        .expect("repository base URL should support path segments")
        .pop_if_empty()
        .extend(["src", "contrib", "Archive", package, &file_name]);

    url
}

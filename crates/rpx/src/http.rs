use async_trait::async_trait;
use http::Extensions;
use keyring::Entry;
use moka::future::Cache;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest_middleware::{ClientBuilder, Middleware, Next};
use reqwest_tracing::{
    ReqwestOtelSpanBackend, TracingMiddleware, default_on_request_end, reqwest_otel_span,
};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::IsTerminal;
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

#[cfg(test)]
mod tests {
    use super::{BinaryArtifactRequestError, display_safe_url, r_macos_binary_target};

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
}

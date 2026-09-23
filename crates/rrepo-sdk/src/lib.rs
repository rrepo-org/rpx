//! Native rrepo endpoints with borrowed HTTP-client injection.
//!
//! Clients retain their middleware, request initializers, and connection pools.
//! The SDK owns endpoint paths and response models, but no cache, authentication
//! configuration, runtime, tracing subscriber, or application fallback policy.
//! Artifact responses are returned unconsumed with their original status/headers.

#![doc = include_str!("../README.md")]

pub use r_description::Description;
pub use reqwest::Url;
pub use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("repository base URL does not support path segments")]
    InvalidBaseUrl,
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
    #[error(transparent)]
    Response(#[from] reqwest::Error),
}

impl Error {
    pub fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Response(error) => error.status(),
            Self::Request(reqwest_middleware::Error::Reqwest(error)) => error.status(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PackagesResponse {
    #[serde(rename = "repositorySlug")]
    pub repository_slug: String,
    pub packages: Vec<PackageSummary>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PackageSummary {
    pub name: String,
    #[serde(rename = "latestVersion")]
    pub latest_version: String,
    #[serde(rename = "latestUploadedAt")]
    pub latest_uploaded_at: Option<String>,
    #[serde(rename = "versionCount")]
    pub version_count: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VersionsResponse {
    pub package: String,
    pub versions: Vec<VersionSummary>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VersionSummary {
    pub version: String,
    #[serde(rename = "sourceUrl")]
    pub source_url: String,
}

#[derive(Debug, Clone)]
pub struct Repository {
    base_url: Url,
}

impl Repository {
    pub fn new(base_url: Url) -> Self {
        Self { base_url }
    }
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn request(
        &self,
        client: &ClientWithMiddleware,
        segments: &[&str],
    ) -> Result<reqwest_middleware::RequestBuilder, Error> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| Error::InvalidBaseUrl)?
            .pop_if_empty()
            .extend(segments);
        Ok(client.get(url))
    }

    #[tracing::instrument(name = "rrepo.packages", skip_all)]
    pub async fn packages(&self, client: &ClientWithMiddleware) -> Result<PackagesResponse, Error> {
        Ok(self
            .request(client, &["packages"])?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    #[tracing::instrument(name = "rrepo.versions", skip_all, fields(package))]
    pub async fn versions(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
    ) -> Result<VersionsResponse, Error> {
        Ok(self
            .request(client, &["packages", package, "versions"])?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    #[tracing::instrument(name = "rrepo.description", skip_all, fields(package, version))]
    pub async fn description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<Description, Error> {
        let text = self
            .request(
                client,
                &["packages", package, "versions", version, "description"],
            )?
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Description::parse(&text))
    }

    /// Return an unconsumed response. The caller controls status/fallback policy.
    #[tracing::instrument(name = "rrepo.source", skip_all, fields(package, version))]
    pub async fn source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<reqwest::Response, Error> {
        Ok(self
            .request(
                client,
                &["packages", package, "versions", version, "source"],
            )?
            .send()
            .await?)
    }

    /// The R series is explicit (`major.minor`) and need not match the host.
    #[tracing::instrument(name = "rrepo.windows_binary", skip_all, fields(package, version))]
    pub async fn windows_binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
        r_series: &str,
    ) -> Result<reqwest::Response, Error> {
        Ok(self
            .request(
                client,
                &[
                    "packages", package, "versions", version, "binaries", "windows", r_series,
                ],
            )?
            .send()
            .await?)
    }

    /// The platform is a repository identifier such as `big-sur-arm64`.
    #[tracing::instrument(name = "rrepo.macos_binary", skip_all, fields(package, version))]
    pub async fn macos_binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
        platform: &str,
        r_series: &str,
    ) -> Result<reqwest::Response, Error> {
        Ok(self
            .request(
                client,
                &[
                    "packages", package, "versions", version, "binaries", "macos", platform,
                    r_series,
                ],
            )?
            .send()
            .await?)
    }
}

#[cfg(test)]
mod tests;

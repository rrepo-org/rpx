//! Native rrepo endpoints with borrowed HTTP-client injection.
//!
//! Clients retain their middleware, request initializers, and connection pools.
//! The SDK owns endpoint paths and response models, but no cache, authentication
//! configuration, runtime, tracing subscriber, or application fallback policy.
//! Artifact responses are returned unconsumed with their original status/headers.

#![doc = include_str!("../README.md")]

pub use r_description::Description;
pub use r_metadata::Version;
pub use reqwest::Url;
pub use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
    #[error(transparent)]
    Response(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Error)]
#[error("repository base URL does not support HTTP path segments")]
pub struct InvalidBaseUrl;

#[derive(Debug, Error)]
pub enum BinaryError {
    #[error("unsupported binary target or R version")]
    UnsupportedTarget,
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
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
    #[serde(deserialize_with = "deserialize_version")]
    pub version: Version,
    #[serde(rename = "sourceUrl")]
    pub source_url: String,
}

fn deserialize_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Version, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone)]
pub struct Repository {
    base_url: Url,
}

impl Repository {
    pub fn new(mut base_url: Url) -> Result<Self, InvalidBaseUrl> {
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(InvalidBaseUrl);
        }
        base_url
            .path_segments_mut()
            .map_err(|()| InvalidBaseUrl)?
            .pop_if_empty();
        Ok(Self { base_url })
    }
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    #[tracing::instrument(name = "rrepo.packages", skip_all)]
    pub async fn packages(
        &self,
        client: &ClientWithMiddleware,
    ) -> Result<PackagesResponse, FetchError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .push("packages");
        Ok(client
            .get(url)
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
    ) -> Result<VersionsResponse, FetchError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["packages", package, "versions"]);
        Ok(client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    #[tracing::instrument(name = "rrepo.description", skip_all, fields(package))]
    pub async fn description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<Description, FetchError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend([
                "packages",
                package,
                "versions",
                version.as_ref(),
                "description",
            ]);
        let text = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Description::parse(&text))
    }

    /// Return an unconsumed response. The caller controls status/fallback policy.
    #[tracing::instrument(name = "rrepo.source", skip_all, fields(package))]
    pub async fn source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["packages", package, "versions", version.as_ref(), "source"]);
        client.get(url).send().await
    }

    /// Return an unconsumed binary response for the target R installation.
    #[tracing::instrument(name = "rrepo.binary", skip_all, fields(package))]
    pub async fn binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
        target: &target_lexicon::Triple,
        r: &Version,
    ) -> Result<reqwest::Response, BinaryError> {
        use target_lexicon::{Architecture, OperatingSystem};
        let series = format!("{}.{}", r.major(), r.minor());
        let r = (r.major(), r.minor());
        let (os, platform) = match (target.operating_system, target.architecture) {
            (OperatingSystem::Windows, Architecture::X86_64) if r >= (3, 0) => ("windows", None),
            (OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_), arch) => {
                let platform = match arch {
                    Architecture::Aarch64(_) if r >= (4, 6) => "sonoma-arm64",
                    Architecture::Aarch64(_) if r >= (4, 1) => "big-sur-arm64",
                    Architecture::X86_64 if r >= (4, 3) => "big-sur-x86_64",
                    _ => return Err(BinaryError::UnsupportedTarget),
                };
                ("macos", Some(platform))
            }
            _ => return Err(BinaryError::UnsupportedTarget),
        };
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend([
                "packages",
                package,
                "versions",
                version.as_ref(),
                "binaries",
                os,
            ])
            .extend(platform)
            .push(&series);
        Ok(client.get(url).send().await?)
    }
}

#[cfg(test)]
mod tests;

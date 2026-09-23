//! CRAN protocol operations using a caller-supplied HTTP client.
//!
//! A [`Repository`] owns only its base URL. Calls borrow the complete middleware
//! client, preserving authentication, request initializers, tracing, and pooling.
//! Metadata methods check HTTP status and parse their native representations.
//! Artifact and archive-root methods return the response without consuming its
//! body or interpreting status, so callers control streaming and fallback policy.
//! No cache, runtime, subscriber, or terminal UI is installed by this crate.

#![doc = include_str!("../README.md")]

pub use r_description::Description;
pub use r_metadata::Version;
pub use r_packages::{Finding, Packages};
pub use reqwest::Url;
pub use reqwest_middleware::ClientWithMiddleware;
use std::{io::Read, str::FromStr};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("repository base URL does not support path segments")]
    InvalidBaseUrl,
    #[error(transparent)]
    Request(#[from] reqwest_middleware::Error),
    #[error(transparent)]
    Response(#[from] reqwest::Error),
    #[error(transparent)]
    Packages(#[from] Box<PackagesParseError>),
    #[error(transparent)]
    Listing(#[from] ArchiveListingError),
    #[error("failed to read source archive: {0}")]
    Archive(#[from] std::io::Error),
    #[error("source archive does not contain {package}/DESCRIPTION")]
    DescriptionNotFound { package: String },
}

impl Error {
    /// Status is preserved for callers implementing their own availability policy.
    pub fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            Self::Response(error) => error.status(),
            Self::Request(reqwest_middleware::Error::Reqwest(error)) => error.status(),
            _ => None,
        }
    }
}

/// Invalid index data with original text and positioned native parser findings.
/// Applications can adapt this into their diagnostic renderer without reparsing.
#[derive(Debug, Clone, Error)]
#[error("failed to parse CRAN PACKAGES index ({} errors)", .findings.len())]
pub struct PackagesParseError {
    pub url: Url,
    pub text: String,
    pub findings: Vec<Finding>,
}

pub fn parse_packages(url: Url, text: String) -> Result<Packages, Box<PackagesParseError>> {
    let packages = Packages::parse(&text);
    let findings: Vec<_> = packages.validate().into_iter().collect();
    if findings.is_empty() {
        Ok(packages)
    } else {
        Err(Box::new(PackagesParseError {
            url,
            text,
            findings,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    pub versions: Vec<Version>,
}

#[derive(Debug, Error)]
#[error("invalid archive version {version}: {reason}")]
pub struct ArchiveListingError {
    pub version: String,
    pub reason: String,
}

impl FromStr for ArchiveListing {
    type Err = ArchiveListingError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input
            .split(['"', '\'', '<', '>', ' ', '\n', '\r', '\t'])
            .filter_map(|part| {
                let file = part.rsplit('/').next().unwrap_or(part);
                (file.ends_with(".tar.gz") && file.contains('_')).then(|| {
                    file.replace("&amp;", "&")
                        .replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&quot;", "\"")
                })
            })
            .filter_map(|file| {
                let (package, version) = file.strip_suffix(".tar.gz")?.rsplit_once('_')?;
                (!package.is_empty() && !version.is_empty()).then(|| version.to_string())
            })
            .try_fold(Vec::new(), |mut versions, value| {
                let version = value
                    .parse::<Version>()
                    .map_err(|error| ArchiveListingError {
                        version: value,
                        reason: error.to_string(),
                    })?;
                if !versions.contains(&version) {
                    versions.push(version);
                }
                Ok(versions)
            })
            .map(|versions| Self { versions })
    }
}

#[derive(Debug, Clone)]
pub struct Repository {
    base_url: Url,
}

impl Repository {
    /// Construction does no I/O. Non-hierarchical URLs fail when building a request.
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

    #[tracing::instrument(name = "cran.packages", skip_all)]
    pub async fn packages(&self, client: &ClientWithMiddleware) -> Result<Packages, Error> {
        let response = self
            .request(client, &["src", "contrib", "PACKAGES"])?
            .send()
            .await?
            .error_for_status()?;
        let url = response.url().clone();
        parse_packages(url, response.text().await?).map_err(Error::Packages)
    }

    /// Fetch the directory root without classifying archive support.
    #[tracing::instrument(name = "cran.archive_root", skip_all)]
    pub async fn archive_root(
        &self,
        client: &ClientWithMiddleware,
    ) -> Result<reqwest::Response, Error> {
        Ok(self
            .request(client, &["src", "contrib", "Archive", ""])?
            .send()
            .await?)
    }

    #[tracing::instrument(name = "cran.archive_listing", skip_all, fields(package))]
    pub async fn archive_listing(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
    ) -> Result<ArchiveListing, Error> {
        let text = self
            .request(client, &["src", "contrib", "Archive", package, ""])?
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(text.parse()?)
    }

    /// Fetch the latest web DESCRIPTION. This is distinct from a version-pinned
    /// source DESCRIPTION and is useful independently of dependency resolution.
    #[tracing::instrument(name = "cran.latest_description", skip_all, fields(package))]
    pub async fn latest_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
    ) -> Result<Description, Error> {
        let text = self
            .request(client, &["web", "packages", package, "DESCRIPTION"])?
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Description::parse(&text))
    }

    /// Return an unconsumed response. Check its status before streaming its body.
    #[tracing::instrument(name = "cran.current_source", skip_all, fields(package, version))]
    pub async fn current_source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<reqwest::Response, Error> {
        let filename = format!("{package}_{version}.tar.gz");
        Ok(self
            .request(client, &["src", "contrib", &filename])?
            .send()
            .await?)
    }

    /// Return an unconsumed response; archive availability policy belongs to callers.
    #[tracing::instrument(name = "cran.archive_source", skip_all, fields(package, version))]
    pub async fn archive_source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<reqwest::Response, Error> {
        let filename = format!("{package}_{version}.tar.gz");
        Ok(self
            .request(client, &["src", "contrib", "Archive", package, &filename])?
            .send()
            .await?)
    }

    #[tracing::instrument(name = "cran.current_description", skip_all, fields(package, version))]
    pub async fn current_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<Description, Error> {
        description_from_source(
            self.current_source(client, package, version)
                .await?
                .error_for_status()?,
            package,
        )
        .await
    }

    #[tracing::instrument(name = "cran.archive_description", skip_all, fields(package, version))]
    pub async fn archive_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
    ) -> Result<Description, Error> {
        description_from_source(
            self.archive_source(client, package, version)
                .await?
                .error_for_status()?,
            package,
        )
        .await
    }

    /// `r_series` is the upstream major.minor series (for example `4.5`).
    #[tracing::instrument(name = "cran.windows_binary", skip_all, fields(package, version))]
    pub async fn windows_binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
        r_series: &str,
    ) -> Result<reqwest::Response, Error> {
        let filename = format!("{package}_{version}.zip");
        Ok(self
            .request(client, &["bin", "windows", "contrib", r_series, &filename])?
            .send()
            .await?)
    }

    /// `platform` is the native repository platform (for example `big-sur-arm64`),
    /// not the current machine. Mapping host triples is the caller's policy.
    #[tracing::instrument(name = "cran.macos_binary", skip_all, fields(package, version))]
    pub async fn macos_binary(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: &str,
        platform: &str,
        r_series: &str,
    ) -> Result<reqwest::Response, Error> {
        let filename = format!("{package}_{version}.tgz");
        Ok(self
            .request(
                client,
                &["bin", "macosx", platform, "contrib", r_series, &filename],
            )?
            .send()
            .await?)
    }
}

/// Extract the top-level DESCRIPTION without writing an archive to disk.
/// The caller selects/checks the HTTP response; parsing doesn't choose a source.
pub async fn description_from_source(
    response: reqwest::Response,
    package: &str,
) -> Result<Description, Error> {
    let bytes = response.bytes().await?;
    let decoder = flate2::read::GzDecoder::new(bytes.as_ref());
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()?
        .find_map(|entry| {
            (|| {
                let mut entry = entry?;
                let matches = {
                    let path = entry.path()?;
                    let mut components = path
                        .components()
                        .filter_map(|part| part.as_os_str().to_str())
                        .filter(|part| *part != ".");
                    components.next() == Some(package)
                        && components.next() == Some("DESCRIPTION")
                        && components.next().is_none()
                };
                if !matches {
                    return Ok(None);
                }
                let mut body = String::new();
                entry.read_to_string(&mut body)?;
                Ok::<_, Error>(Some(Description::parse(&body)))
            })()
            .transpose()
        })
        .unwrap_or_else(|| {
            Err(Error::DescriptionNotFound {
                package: package.into(),
            })
        })
}

#[cfg(test)]
mod tests;

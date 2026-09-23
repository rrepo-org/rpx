//! CRAN protocol operations using a caller-supplied HTTP client.
//!
//! A [`Repository`] owns only its base URL. Calls borrow the complete middleware
//! client, preserving authentication, request initializers, tracing, and pooling.
//! Metadata methods check HTTP status and parse their native representations.
//! Artifact and archive-root methods return the response without consuming its
//! body or interpreting status, so callers control streaming and fallback policy.
//! No cache, runtime, subscriber, or terminal UI is installed by this crate.

#![doc = include_str!("../README.md")]

use futures_util::TryStreamExt;
pub use r_description::Description;
pub use r_metadata::Version;
pub use r_packages::{Finding, Packages};
pub use reqwest::Url;
pub use reqwest_middleware::ClientWithMiddleware;
use std::collections::BTreeSet;
use thiserror::Error;

mod binary;
pub use binary::{BinaryError, BinaryPackagesError, RoutingError};

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
pub enum PackagesError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Invalid(#[from] Box<PackagesParseError>),
}

#[derive(Debug, Error)]
pub enum ListingError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Directory(#[from] directory_listing::Error),
    #[error("archive listing is explicitly truncated")]
    Truncated,
    #[error("invalid archive version {version}: {source}")]
    Version {
        version: String,
        #[source]
        source: r_metadata::VersionParseError,
    },
}

#[derive(Debug, Error)]
pub enum DescriptionError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("failed to read source archive: {0}")]
    Archive(#[from] std::io::Error),
    #[error("source archive does not contain {package}/DESCRIPTION")]
    DescriptionNotFound { package: String },
}

/// Invalid index data with original text and positioned native parser findings.
/// Applications can adapt this into their diagnostic renderer without reparsing.
#[derive(Debug, Clone, Error)]
#[error("failed to parse CRAN PACKAGES index ({} errors)", .findings.len())]
pub struct PackagesParseError {
    pub text: String,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
pub struct Repository {
    base_url: Url,
}

impl Repository {
    /// Validate and normalize the HTTP repository URL without doing I/O.
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

    #[tracing::instrument(name = "cran.packages", skip_all)]
    pub async fn packages(&self, client: &ClientWithMiddleware) -> Result<Packages, PackagesError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["src", "contrib", "PACKAGES"]);
        self.packages_at(client, url).await
    }

    async fn packages_at(
        &self,
        client: &ClientWithMiddleware,
        url: Url,
    ) -> Result<Packages, PackagesError> {
        let text = client
            .get(url)
            .send()
            .await
            .map_err(FetchError::from)?
            .error_for_status()
            .map_err(FetchError::from)?
            .text()
            .await
            .map_err(FetchError::from)?;
        let packages = Packages::parse(&text);
        let findings: Vec<_> = packages.validate().into_iter().collect();
        if findings.is_empty() {
            Ok(packages)
        } else {
            Err(PackagesError::Invalid(Box::new(PackagesParseError {
                text,
                findings,
            })))
        }
    }

    /// Fetch the directory root without classifying archive support.
    #[tracing::instrument(name = "cran.archive_root", skip_all)]
    pub async fn archive_root(
        &self,
        client: &ClientWithMiddleware,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["src", "contrib", "Archive", ""]);
        client.get(url).send().await
    }

    #[tracing::instrument(name = "cran.archive_listing", skip_all, fields(package))]
    pub async fn archive_listing(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
    ) -> Result<Vec<Version>, ListingError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["src", "contrib", "Archive", package, ""]);
        let response = client
            .get(url)
            .send()
            .await
            .map_err(FetchError::from)?
            .error_for_status()
            .map_err(FetchError::from)?;
        let base = response.url().clone();
        let json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            });
        let text = response.text().await.map_err(FetchError::from)?;
        let listing = if json {
            directory_listing::parse_nginx_json(&base, &text)?
        } else {
            directory_listing::parse_html(&base, &text)?
        };
        if listing.truncated {
            return Err(ListingError::Truncated);
        }
        let prefix = format!("{package}_");
        let mut seen = BTreeSet::new();
        listing
            .entries
            .into_iter()
            .filter(|entry| entry.kind == directory_listing::EntryKind::File)
            .filter_map(|entry| {
                entry
                    .name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".tar.gz")
                    .map(str::to_owned)
            })
            .map(|version| {
                version
                    .parse::<Version>()
                    .map_err(|source| ListingError::Version { version, source })
            })
            .filter(|result| match result {
                Ok(version) => seen.insert(version.clone()),
                Err(_) => true,
            })
            .collect()
    }

    /// Fetch the latest web DESCRIPTION. This is distinct from a version-pinned
    /// source DESCRIPTION and is useful independently of dependency resolution.
    #[tracing::instrument(name = "cran.latest_description", skip_all, fields(package))]
    pub async fn latest_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
    ) -> Result<Description, FetchError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["web", "packages", package, "DESCRIPTION"]);
        let text = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Description::parse(&text))
    }

    /// Return an unconsumed response. Check its status before streaming its body.
    #[tracing::instrument(name = "cran.current_source", skip_all, fields(package))]
    pub async fn current_source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let version = version.as_ref();
        let filename = format!("{package}_{version}.tar.gz");
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["src", "contrib", &filename]);
        client.get(url).send().await
    }

    /// Return an unconsumed response; archive availability policy belongs to callers.
    #[tracing::instrument(name = "cran.archive_source", skip_all, fields(package))]
    pub async fn archive_source(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        let version = version.as_ref();
        let filename = format!("{package}_{version}.tar.gz");
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .expect("validated repository URL")
            .extend(["src", "contrib", "Archive", package, &filename]);
        client.get(url).send().await
    }

    #[tracing::instrument(name = "cran.current_description", skip_all, fields(package))]
    pub async fn current_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<Description, DescriptionError> {
        description_from_source(
            self.current_source(client, package, version)
                .await
                .map_err(FetchError::from)?
                .error_for_status()
                .map_err(FetchError::from)?,
            package,
        )
        .await
    }

    #[tracing::instrument(name = "cran.archive_description", skip_all, fields(package))]
    pub async fn archive_description(
        &self,
        client: &ClientWithMiddleware,
        package: &str,
        version: impl AsRef<str>,
    ) -> Result<Description, DescriptionError> {
        description_from_source(
            self.archive_source(client, package, version)
                .await
                .map_err(FetchError::from)?
                .error_for_status()
                .map_err(FetchError::from)?,
            package,
        )
        .await
    }
}

/// Extract the top-level DESCRIPTION without writing an archive to disk.
/// The caller selects/checks the HTTP response; parsing doesn't choose a source.
pub async fn description_from_source(
    response: reqwest::Response,
    package: &str,
) -> Result<Description, DescriptionError> {
    let reader =
        tokio_util::io::StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
    let bytes = archive_stream::tar_gz_entry(reader, format!("{package}/DESCRIPTION"))
        .await?
        .ok_or_else(|| DescriptionError::DescriptionNotFound {
            package: package.into(),
        })?;
    let text = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(Description::parse(&text))
}

#[cfg(test)]
mod tests;

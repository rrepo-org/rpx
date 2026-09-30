use r_description::Description;
use r_metadata::Version;
use reqwest::{StatusCode, Url, multipart};
use serde::Deserialize;
use std::{io, path::PathBuf, str::FromStr};
use thiserror::Error;

const RREPO_URL: &str = "https://rrepo.dev/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositorySlug {
    namespace: String,
    repository: String,
}

#[derive(Debug, Error)]
#[error("expected a namespace-qualified rrepo repository such as `acme/internal`")]
pub struct RepositorySlugError;

impl FromStr for RepositorySlug {
    type Err = RepositorySlugError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (namespace, repository) = value.split_once('/').ok_or(RepositorySlugError)?;
        if !valid_segment(namespace) || !valid_segment(repository) {
            return Err(RepositorySlugError);
        }
        Ok(Self {
            namespace: namespace.to_string(),
            repository: repository.to_string(),
        })
    }
}

impl std::fmt::Display for RepositorySlug {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.namespace, self.repository)
    }
}

fn valid_segment(value: &str) -> bool {
    value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl RepositorySlug {
    fn upload_url(&self, base: &Url) -> Url {
        let mut url = base.clone();
        url.path_segments_mut()
            .expect("HTTP base URL has path segments")
            .pop_if_empty()
            .extend([&self.namespace, &self.repository, "upload"]);
        url
    }
}

/// The API key is intentionally not printable via `Debug`.
pub struct PublishRequest {
    pub artifact: PathBuf,
    pub package: String,
    pub version: Version,
    pub repository: RepositorySlug,
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub package: String,
    pub version: String,
    pub source_url: String,
}

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("failed to inspect source archive at {}: {source}", path.display())]
    Archive {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("source archive is not a regular file: {}", path.display())]
    NotAFile { path: PathBuf },
    #[error("source archive at {} does not contain {package}/DESCRIPTION", path.display())]
    DescriptionMissing { path: PathBuf, package: String },
    #[error("source archive at {} contains an invalid DESCRIPTION", path.display())]
    InvalidDescription { path: PathBuf },
    #[error("source archive at {} identifies {actual}, expected {expected}", path.display())]
    IdentityMismatch {
        path: PathBuf,
        actual: String,
        expected: String,
    },
    #[error("failed to create rrepo upload client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("failed to prepare source upload: {0}")]
    Prepare(#[source] io::Error),
    #[error("failed to send source upload: {0}")]
    Request(#[source] reqwest::Error),
    #[error("rrepo upload failed with HTTP {status}")]
    Status { status: StatusCode },
    #[error("rrepo rejected RREPO_API_KEY (HTTP {status}); use a packages:write API key")]
    Authorization { status: StatusCode },
    #[error("failed to decode rrepo upload response: {0}")]
    Response(#[source] reqwest::Error),
    #[error("rrepo reported {actual} after uploading {expected}")]
    ResponseMismatch { actual: String, expected: String },
}

pub async fn publish(request: PublishRequest) -> Result<PublishResult, PublishError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(PublishError::Client)?;
    let base = Url::parse(RREPO_URL).expect("canonical rrepo URL is valid");
    publish_to(&client, request.repository.upload_url(&base), request).await
}

async fn publish_to(
    client: &reqwest::Client,
    url: Url,
    request: PublishRequest,
) -> Result<PublishResult, PublishError> {
    let metadata = tokio::fs::metadata(&request.artifact)
        .await
        .map_err(|source| PublishError::Archive {
            path: request.artifact.clone(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(PublishError::NotAFile {
            path: request.artifact,
        });
    }
    let file = tokio::fs::File::open(&request.artifact)
        .await
        .map_err(|source| PublishError::Archive {
            path: request.artifact.clone(),
            source,
        })?;
    let description =
        archive_stream::tar_gz_entry(file, format!("{}/DESCRIPTION", request.package))
            .await
            .map_err(|source| PublishError::Archive {
                path: request.artifact.clone(),
                source,
            })?
            .ok_or_else(|| PublishError::DescriptionMissing {
                path: request.artifact.clone(),
                package: request.package.clone(),
            })?;
    let description =
        String::from_utf8(description).map_err(|_| PublishError::InvalidDescription {
            path: request.artifact.clone(),
        })?;
    let description = Description::parse(&description);
    let expected = format!("{} {}", request.package, request.version);
    if !description.diagnostics().is_empty() {
        return Err(PublishError::InvalidDescription {
            path: request.artifact,
        });
    }
    let actual = format!(
        "{} {}",
        description.package().map_or_else(
            || "<missing>".to_string(),
            |value| value.as_str().to_string()
        ),
        description.version().map_or_else(
            || "<missing>".to_string(),
            |value| value.as_str().to_string()
        )
    );
    if actual != expected {
        return Err(PublishError::IdentityMismatch {
            path: request.artifact,
            actual,
            expected,
        });
    }

    let part = multipart::Part::file(&request.artifact)
        .await
        .map_err(PublishError::Prepare)?
        .mime_str("application/gzip")
        .map_err(PublishError::Request)?;
    let response = client
        .post(url)
        .bearer_auth(&request.api_key)
        .multipart(multipart::Form::new().part("file", part))
        .send()
        .await
        .map_err(PublishError::Request)?;
    if response.status() != StatusCode::ACCEPTED {
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(PublishError::Authorization {
                status: response.status(),
            });
        }
        return Err(PublishError::Status {
            status: response.status(),
        });
    }
    let result: PublishResult = response.json().await.map_err(PublishError::Response)?;
    let actual = format!("{} {}", result.package, result.version);
    if actual != expected {
        return Err(PublishError::ResponseMismatch { actual, expected });
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

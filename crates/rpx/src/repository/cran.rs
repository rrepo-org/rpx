use super::{ArchiveSupport, RepositoryError};
use crate::{description::description_identity, http};
use futures_util::{StreamExt, TryStreamExt, future, stream};
use miette::{Diagnostic, NamedSource, SourceSpan};
use moka::future::Cache;
use r_description::Description;
use r_metadata::Version;
use r_packages::{Finding, Packages};
use reqwest::Url;
use std::{collections::BTreeSet, sync::Arc};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct CranRepository {
    source: cran_sdk::Repository,
    archives: ArchiveSupport,
    packages: Cache<(), Arc<Packages>>,
    archive_versions: Cache<String, Option<BTreeSet<Version>>>,
    descriptions: Cache<(String, Version), Option<Arc<Description>>>,
}

impl std::fmt::Display for CranRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url().fmt(formatter)
    }
}

impl CranRepository {
    pub fn new(url: Url, archives: ArchiveSupport) -> Result<Self, cran_sdk::InvalidBaseUrl> {
        Ok(Self {
            source: cran_sdk::Repository::new(url)?,
            archives,
            packages: Cache::new(1),
            archive_versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        })
    }

    pub fn url(&self) -> &Url {
        self.source.base_url()
    }

    pub fn archive_support(&self) -> ArchiveSupport {
        self.archives
    }

    /// Recognize CRAN and retain its index while probing archive support concurrently.
    pub async fn probe(url: Url) -> Result<Self, RepositoryError> {
        let source = cran_sdk::Repository::new(url)?;
        let client = http::client();
        let archive_probe = async {
            source
                .archive_root(&client)
                .await
                .map_err(cran_sdk::FetchError::from)?
                .error_for_status()
                .map_err(cran_sdk::FetchError::from)
        };
        let (index, archive) = tokio::join!(source.packages(&client), archive_probe);
        let index = index?;
        let archives = match archive.map_err(RepositoryError::from) {
            Ok(_) => ArchiveSupport::Available,
            Err(RepositoryError::Response { source })
                if matches!(
                    source.status(),
                    Some(reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN)
                ) =>
            {
                ArchiveSupport::Unavailable
            }
            Err(error) => return Err(error),
        };
        let packages = Cache::new(1);
        packages.insert((), Arc::new(index)).await;
        Ok(Self {
            source,
            archives,
            packages,
            archive_versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        })
    }

    pub async fn packages_index(&self) -> Result<Arc<Packages>, RepositoryError> {
        self.packages
            .try_get_with((), async {
                self.source
                    .packages(&http::client())
                    .await
                    .map(Arc::new)
                    .map_err(RepositoryError::from)
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }

    pub async fn archive_versions(
        &self,
        package: &str,
    ) -> Result<Option<BTreeSet<Version>>, RepositoryError> {
        self.archive_versions
            .try_get_with(package.to_string(), async {
                match self.source.archive_listing(&http::client(), package).await {
                    Ok(versions) => Ok(Some(versions.into_iter().collect())),
                    // Preserve the existing listing-support policy in rpx.
                    Err(cran_sdk::ListingError::Fetch(
                        cran_sdk::FetchError::Response(error)
                        | cran_sdk::FetchError::Request(reqwest_middleware::Error::Reqwest(error)),
                    )) if matches!(
                        error.status(),
                        Some(
                            reqwest::StatusCode::FORBIDDEN
                                | reqwest::StatusCode::NOT_FOUND
                                | reqwest::StatusCode::GONE
                        )
                    ) =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(RepositoryError::from(error)),
                }
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }

    /// Resolve metadata from the index, current source, then archive. Only
    /// confirmed absence (404/410) advances source lookup; errors are not cached.
    pub async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Option<Arc<Description>>, RepositoryError> {
        self.descriptions
            .try_get_with((package.to_string(), version.clone()), async {
                let index = self.packages_index().await?;
                if let Some(record) = index.records().find(|record| {
                    record
                        .package()
                        .is_some_and(|value| value.as_str() == package)
                        && matches!(record.parsed_version(), Some(Ok(found)) if &found == version)
                }) {
                    return Ok(Some(Arc::new(Description::from(record))));
                }
                let client = http::client();
                let descriptions =
                    stream::once(self.source.current_description(&client, package, version))
                        .chain(stream::once(
                            self.source.archive_description(&client, package, version),
                        ))
                        .filter_map(|result| {
                            future::ready(match result {
                                Ok(description) => Some(Ok(description)),
                                Err(cran_sdk::DescriptionError::Fetch(
                                    cran_sdk::FetchError::Response(error)
                                    | cran_sdk::FetchError::Request(
                                        reqwest_middleware::Error::Reqwest(error),
                                    ),
                                )) if matches!(
                                    error.status(),
                                    Some(
                                        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
                                    )
                                ) =>
                                {
                                    None
                                }
                                Err(error) => Some(Err(RepositoryError::from(error))),
                            })
                        });
                futures_util::pin_mut!(descriptions);

                descriptions
                    .try_next()
                    .await?
                    .map(|description| {
                        let (found_name, found_version) = description_identity(
                            format!("source DESCRIPTION from {}", self.url()),
                            &description,
                        )?;
                        if found_name != package || &found_version != version {
                            return Err(RepositoryError::InvalidData {
                                resource: format!(
                                    "source archive for {package} {version} from {}",
                                    self.url()
                                ),
                                details: format!("contains {found_name} {found_version}"),
                            });
                        }
                        Ok(Arc::new(description))
                    })
                    .transpose()
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }

    pub async fn current_source(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        self.source
            .current_source(&http::client(), package, version.as_ref())
            .await
    }

    pub async fn archive_source(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        self.source
            .archive_source(&http::client(), package, version.as_ref())
            .await
    }

    pub async fn binary(
        &self,
        package: &str,
        version: &Version,
        target: &target_lexicon::Triple,
        r_version: &semver::Version,
    ) -> Result<reqwest::Response, http::BinaryArtifactRequestError> {
        let client = http::client();
        let r_version: Version = format!("{}.{}", r_version.major, r_version.minor)
            .parse()
            .expect("numeric R major/minor version");
        self.source
            .binary(&client, package, version, target, &r_version)
            .await
            .map_err(|error| match error {
                cran_sdk::BinaryError::Request(error) => error.into(),
                cran_sdk::BinaryError::Routing(_) => {
                    http::BinaryArtifactRequestError::UnsupportedTarget {
                        target: target.clone(),
                    }
                }
            })
    }
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
    pub(crate) fn new(
        source_name: impl Into<String>,
        source: String,
        findings: Vec<Finding>,
    ) -> Self {
        let issues: Vec<_> = findings
            .into_iter()
            .map(|finding| {
                let range = finding.span();
                CranPackagesParseIssue {
                    span: (range.start..range.end).into(),
                    finding,
                }
            })
            .collect();
        Self {
            count: issues.len(),
            source_code: NamedSource::new(source_name.into(), source),
            issues,
        }
    }
}

#[derive(Clone, Debug, Error, Diagnostic)]
#[error("{finding}")]
struct CranPackagesParseIssue {
    finding: Finding,
    #[label("{finding}")]
    span: SourceSpan,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_archive(package: &str, version: &str) -> Vec<u8> {
        let body = format!("Package: {package}\nVersion: {version}\n");
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("{package}/DESCRIPTION"),
                body.as_bytes(),
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[tokio::test]
    async fn metadata_lookup_coalesces_and_caches_the_complete_search() {
        for location in ["index", "current", "archive", "missing"] {
            let mut server = mockito::Server::new_async().await;
            let repo =
                CranRepository::new(server.url().parse().unwrap(), ArchiveSupport::Unavailable)
                    .unwrap();
            let clone = repo.clone();
            let index = server
                .mock("GET", "/src/contrib/PACKAGES")
                .with_body(if location == "index" {
                    "Package: example\nVersion: 1.0\n"
                } else {
                    ""
                })
                .expect(1)
                .create_async()
                .await;
            let current = server
                .mock("GET", "/src/contrib/example_1.0.tar.gz")
                .with_status(if location == "current" { 200 } else { 404 })
                .with_body(source_archive("example", "1.0"))
                .expect(usize::from(location != "index"))
                .create_async()
                .await;
            let archived = server
                .mock("GET", "/src/contrib/Archive/example/example_1.0.tar.gz")
                .with_status(if location == "archive" { 200 } else { 410 })
                .with_body(source_archive("example", "1.0"))
                .expect(usize::from(matches!(location, "archive" | "missing")))
                .create_async()
                .await;
            let version = "1.0".parse().unwrap();
            let (first, concurrent) = tokio::join!(
                repo.description("example", &version),
                clone.description("example", &version)
            );
            let first = first.unwrap();
            let concurrent = concurrent.unwrap();
            let repeated = repo.description("example", &version).await.unwrap();
            if location == "missing" {
                assert!(first.is_none() && concurrent.is_none() && repeated.is_none());
            } else {
                let first = first.unwrap();
                assert!(Arc::ptr_eq(&first, &concurrent.unwrap()));
                assert!(Arc::ptr_eq(&first, &repeated.unwrap()));
                assert_eq!(first.package().unwrap().as_str(), "example");
            }
            index.assert_async().await;
            current.assert_async().await;
            archived.assert_async().await;
        }
    }

    #[tokio::test]
    async fn current_metadata_failures_do_not_fall_back_or_enter_the_cache() {
        for (status, body) in [
            (403, Vec::new()),
            (500, Vec::new()),
            (200, b"bad archive".to_vec()),
            (200, source_archive("example", "9.0")),
        ] {
            let mut server = mockito::Server::new_async().await;
            let repo =
                CranRepository::new(server.url().parse().unwrap(), ArchiveSupport::Available)
                    .unwrap();
            let index = server
                .mock("GET", "/src/contrib/PACKAGES")
                .with_body("")
                .expect(1)
                .create_async()
                .await;
            let current = server
                .mock("GET", "/src/contrib/example_1.0.tar.gz")
                .with_status(status)
                .with_body(body)
                .expect(2)
                .create_async()
                .await;
            let archived = server
                .mock("GET", "/src/contrib/Archive/example/example_1.0.tar.gz")
                .with_body(source_archive("example", "1.0"))
                .expect(0)
                .create_async()
                .await;
            let version = "1.0".parse().unwrap();
            assert!(repo.description("example", &version).await.is_err());
            assert!(repo.description("example", &version).await.is_err());
            index.assert_async().await;
            current.assert_async().await;
            archived.assert_async().await;
        }
    }

    #[tokio::test]
    async fn probe_owns_archive_policy_and_retains_the_index() {
        for status in [200, 403, 404, 410, 500] {
            let mut server = mockito::Server::new_async().await;
            let index = server
                .mock("GET", "/src/contrib/PACKAGES")
                .with_body("Package: example\nVersion: 1.0\n")
                .expect(1)
                .create_async()
                .await;
            let archive = server
                .mock("GET", "/src/contrib/Archive/")
                .with_status(status)
                .expect(1)
                .create_async()
                .await;
            let result = CranRepository::probe(server.url().parse().unwrap()).await;
            match status {
                200 | 403 | 404 => {
                    let repo = result.unwrap();
                    assert_eq!(
                        repo.archive_support(),
                        if status == 200 {
                            ArchiveSupport::Available
                        } else {
                            ArchiveSupport::Unavailable
                        }
                    );
                    let first = repo.packages_index().await.unwrap();
                    let clone = repo.clone().packages_index().await.unwrap();
                    assert!(Arc::ptr_eq(&first, &clone));
                }
                _ => assert!(
                    matches!(result, Err(RepositoryError::Response { source }) if source.status().unwrap().as_u16() == status as u16)
                ),
            }
            index.assert_async().await;
            archive.assert_async().await;
        }
    }

    #[tokio::test]
    async fn indexed_description_preserves_all_record_metadata() {
        let mut server = mockito::Server::new_async().await;
        let repo =
            CranRepository::new(server.url().parse().unwrap(), ArchiveSupport::Available).unwrap();
        let record = concat!(
            "Package: example\nVersion: 1.0.0\n",
            "Depends: R (>= 4.0)\n",
            "Imports: old\nImports: current,\n    another (>= 1.0)\n",
            "Suggests: optional\nLinkingTo: headers\n",
            "License: MIT\nNeedsCompilation: yes\n",
            "X-Custom: preserved\n    continuation\n",
        );
        let index = server
            .mock("GET", "/src/contrib/PACKAGES")
            .with_body(format!(
                "Package: other\nVersion: 2.0\n\n{record}\nPackage: last\nVersion: 3.0\n"
            ))
            .expect(1)
            .create_async()
            .await;
        let current = server
            .mock("GET", "/src/contrib/example_1.0.0.tar.gz")
            .expect(0)
            .create_async()
            .await;
        let archived = server
            .mock("GET", "/src/contrib/Archive/example/example_1.0.0.tar.gz")
            .expect(0)
            .create_async()
            .await;

        let description = repo
            .description("example", &"1.0.0".parse().unwrap())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(description.to_string(), record);
        assert_eq!(
            description.imports().unwrap().as_str(),
            "current,\nanother (>= 1.0)"
        );
        assert_eq!(description.package().unwrap().as_str(), "example");
        assert_eq!(
            description.version_parsed().unwrap().unwrap().as_str(),
            "1.0.0"
        );
        assert_eq!(
            description
                .imports_parsed()
                .values()
                .map(r_metadata::Relation::package)
                .collect::<Vec<_>>(),
            ["old", "current", "another"]
        );
        index.assert_async().await;
        current.assert_async().await;
        archived.assert_async().await;
    }
}

use super::{ArchiveSupport, RepositoryError};
use crate::{description::description_identity, http};
use miette::{Diagnostic, NamedSource, SourceSpan};
use moka::future::Cache;
use r_description::{Description, LogicalValue};
use r_metadata::Version;
use r_packages::{Finding, PackageRecord, Packages};
use reqwest::Url;
use std::{collections::BTreeSet, sync::Arc};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct CranRepository {
    source: cran_sdk::Repository,
    archives: ArchiveSupport,
    packages: Cache<(), Arc<Packages>>,
    archive_versions: Cache<String, Option<BTreeSet<Version>>>,
    descriptions: Cache<(SourceLocation, String, Version), Option<Arc<Description>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SourceLocation {
    Current,
    Archive,
}

impl std::fmt::Display for CranRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url().fmt(formatter)
    }
}

impl CranRepository {
    pub fn new(url: Url, archives: ArchiveSupport) -> Self {
        Self {
            source: cran_sdk::Repository::new(url),
            archives,
            packages: Cache::new(1),
            archive_versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        }
    }

    pub fn url(&self) -> &Url {
        self.source.base_url()
    }

    pub fn archive_support(&self) -> ArchiveSupport {
        self.archives
    }

    pub(crate) fn with_archive_support(mut self, archives: ArchiveSupport) -> Self {
        self.archives = archives;
        self
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
                    Ok(listing) => Ok(Some(listing.versions.into_iter().collect())),
                    // Preserve the existing listing-support policy in rpx.
                    Err(error)
                        if matches!(
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

    pub async fn indexed_description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Option<Arc<Description>>, RepositoryError> {
        let index = self.packages_index().await?;
        Ok(index
            .records()
            .find(|record| {
                record
                    .package()
                    .is_some_and(|value| value.as_str() == package)
                    && record
                        .parsed_version()
                        .is_some_and(|value| value.as_ref().is_ok_and(|value| value == version))
            })
            .map(|entry| Arc::new(packages_record_to_description(&entry))))
    }

    pub async fn current_description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Option<Arc<Description>>, RepositoryError> {
        self.tarball_description(SourceLocation::Current, package, version)
            .await
    }

    pub async fn archive_description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Option<Arc<Description>>, RepositoryError> {
        self.tarball_description(SourceLocation::Archive, package, version)
            .await
    }

    async fn tarball_description(
        &self,
        location: SourceLocation,
        package: &str,
        version: &Version,
    ) -> Result<Option<Arc<Description>>, RepositoryError> {
        self.descriptions
            .try_get_with((location, package.to_string(), version.clone()), async {
                let client = http::client();
                let result = match location {
                    SourceLocation::Current => {
                        self.source
                            .current_description(&client, package, version.as_ref())
                            .await
                    }
                    SourceLocation::Archive => {
                        self.source
                            .archive_description(&client, package, version.as_ref())
                            .await
                    }
                };
                let description = match result {
                    Ok(description) => description,
                    Err(error)
                        if matches!(
                            error.status(),
                            Some(reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE)
                        ) =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(RepositoryError::from(error)),
                };
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
                Ok::<_, RepositoryError>(Some(Arc::new(description)))
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
            .map_err(request_error)
    }

    pub async fn archive_source(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        self.source
            .archive_source(&http::client(), package, version.as_ref())
            .await
            .map_err(request_error)
    }

    pub async fn binary(
        &self,
        package: &str,
        version: &Version,
        target: &target_lexicon::Triple,
        r_version: &semver::Version,
    ) -> Result<reqwest::Response, http::BinaryArtifactRequestError> {
        use target_lexicon::OperatingSystem;
        let client = http::client();
        let r_series = format!("{}.{}", r_version.major, r_version.minor);
        match target.operating_system {
            OperatingSystem::Windows => {
                self.source
                    .windows_binary(&client, package, version.as_ref(), &r_series)
                    .await
            }
            OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => {
                self.source
                    .macos_binary(
                        &client,
                        package,
                        version.as_ref(),
                        http::r_macos_binary_target(target)?,
                        &r_series,
                    )
                    .await
            }
            _ => {
                return Err(http::BinaryArtifactRequestError::UnsupportedTarget {
                    target: target.clone(),
                });
            }
        }
        .map_err(request_error)
        .map_err(Into::into)
    }

    pub(crate) async fn archive_root(
        &self,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        self.source
            .archive_root(&http::client())
            .await
            .map_err(request_error)
    }
}

fn request_error(error: cran_sdk::Error) -> reqwest_middleware::Error {
    match error {
        cran_sdk::Error::Request(error) => error,
        error => reqwest_middleware::Error::middleware(error),
    }
}

fn packages_record_to_description(record: &PackageRecord) -> Description {
    let value = |value: r_description::ValueText| {
        LogicalValue::new(value.as_str()).expect("validated metadata is a valid DCF value")
    };
    let mut builder = Description::builder()
        .package(value(
            record.package().expect("validated Package should exist"),
        ))
        .version(value(
            record.version().expect("validated Version should exist"),
        ));
    if let Some(depends) = record.depends() {
        builder = builder.depends(value(depends));
    }
    if let Some(imports) = record.imports() {
        builder = builder.imports(value(imports));
    }
    if let Some(suggests) = record.suggests() {
        builder = builder.suggests(value(suggests));
    }
    if let Some(linking_to) = record.linking_to() {
        builder = builder.field(
            r_description::FieldName::new("LinkingTo").expect("constant field name is valid"),
            value(linking_to),
        );
    }
    builder.build()
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

    #[test]
    fn builds_description_from_validated_package_record() {
        let packages = r_packages::Packages::parse(
            "Package: example\nVersion: 1.0.0\nImports: old\nImports: current,\n",
        );
        assert!(packages.validate().is_empty());
        let record = packages.record(0).expect("package record should exist");

        let description = packages_record_to_description(&record);

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
            ["current"]
        );
    }
}

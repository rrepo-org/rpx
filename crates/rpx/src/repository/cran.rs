use super::{ArchiveSupport, PackageRepository, RepositoryError};
use crate::{http, resolver::PackageVersion};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use moka::future::Cache;
use r_description::{Description, LogicalValue};
use r_metadata::Version;
use r_packages::{PackageRecord, Packages};
use reqwest::Url;
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct CranRepository {
    url: Url,
    archives: ArchiveSupport,
    packages: Cache<(), Arc<Packages>>,
    archive_versions: Cache<String, BTreeSet<Version>>,
    descriptions: Cache<(String, Version), Arc<Description>>,
}

impl std::fmt::Display for CranRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.url.fmt(formatter)
    }
}

impl CranRepository {
    pub fn new(url: Url, archives: ArchiveSupport) -> Self {
        Self {
            url,
            archives,
            packages: Cache::new(1),
            archive_versions: Cache::new(1024),
            descriptions: Cache::new(4096),
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn archive_support(&self) -> ArchiveSupport {
        self.archives
    }

    async fn packages_index(&self) -> Result<Arc<Packages>, RepositoryError> {
        self.packages
            .try_get_with((), async {
                let response = http::cran_packages(&self.url)
                    .await
                    .map_err(|source| RepositoryError::Request {
                        source: Arc::new(source),
                    })?
                    .error_for_status()
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?;
                let source_name = http::display_safe_url(response.url()).to_string();
                let text = response
                    .text()
                    .await
                    .map_err(|source| RepositoryError::Response {
                        source: Arc::new(source),
                    })?;
                let packages = Packages::parse(&text);
                let findings = packages.validate().into_iter().collect::<Vec<_>>();
                if !findings.is_empty() {
                    let source = http::CranPackagesParseError::new(source_name, text, findings);
                    return Err(RepositoryError::CranPackages(Box::new(source)));
                }

                Ok::<Arc<Packages>, RepositoryError>(Arc::new(packages))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }
}

#[async_trait]
impl PackageRepository for CranRepository {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn equals(&self, other: &dyn PackageRepository) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.url == other.url && self.archives == other.archives)
    }

    async fn packages(&self) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let index = self.packages_index().await?;

        Ok(index
            .records()
            .map(|record| {
                let package = record.package().expect("validated Package should exist");
                let version = record
                    .parsed_version()
                    .expect("validated Version should exist")
                    .expect("validated Version should parse");
                (
                    package.as_str().to_owned(),
                    PackageVersion::new(version, Arc::clone(&repository)),
                )
            })
            .collect())
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let index = self.packages_index().await?;
        let mut versions = index
            .records()
            .filter(|record| {
                record
                    .package()
                    .is_some_and(|value| value.as_str() == package)
            })
            .map(|record| {
                record
                    .parsed_version()
                    .expect("validated Version should exist")
                    .expect("validated Version should parse")
            })
            .collect::<BTreeSet<_>>();

        if versions.is_empty() {
            return Ok(BTreeSet::new());
        }

        if self.archives == ArchiveSupport::Available {
            let archived_versions = self
                .archive_versions
                .try_get_with(package.to_string(), async {
                    let text = http::cran_package_archive_listing(&self.url, package)
                        .await
                        .map_err(|source| RepositoryError::Request {
                            source: Arc::new(source),
                        })?
                        .error_for_status()
                        .map_err(|source| RepositoryError::Response {
                            source: Arc::new(source),
                        })?
                        .text()
                        .await
                        .map_err(|source| RepositoryError::Response {
                            source: Arc::new(source),
                        })?;
                    let listing =
                        text.parse::<http::CranPackageArchiveListing>()
                            .map_err(|source| RepositoryError::InvalidData {
                                resource: "CRAN package archive listing".to_string(),
                                details: source.to_string(),
                            })?;

                    Ok::<BTreeSet<Version>, RepositoryError>(listing.versions.into_iter().collect())
                })
                .await
                .map_err(Arc::unwrap_or_clone)?;

            versions.extend(archived_versions);
        }

        tracing::trace!(
            package,
            repository = %self.url,
            versions = versions.len(),
            "loaded package versions"
        );

        Ok(versions
            .into_iter()
            .map(|version| PackageVersion::new(version, Arc::clone(&repository)))
            .collect())
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<Description>, RepositoryError> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let index = self.packages_index().await?;
                let description = if let Some(entry) = index.records().find(|record| {
                    record
                        .package()
                        .is_some_and(|value| value.as_str() == package)
                        && record
                            .parsed_version()
                            .is_some_and(|value| value.as_ref().is_ok_and(|value| value == version))
                }) {
                    packages_record_to_description(&entry)
                } else {
                    let version_string = version.to_string();
                    let response =
                        http::cran_archive_source_tarball(&self.url, package, &version_string)
                            .await
                            .map_err(|source| RepositoryError::Request {
                                source: Arc::new(source),
                            })?
                            .error_for_status()
                            .map_err(|source| RepositoryError::Response {
                                source: Arc::new(source),
                            })?;

                    description_from_source_tarball_response(response, package).await?
                };

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url,
                    "fetched package description"
                );

                Ok::<Arc<Description>, RepositoryError>(Arc::new(description))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
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

async fn description_from_source_tarball_response(
    response: reqwest::Response,
    package: &str,
) -> Result<Description, RepositoryError> {
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|source| RepositoryError::Response {
            source: Arc::new(source),
        })?
    {
        bytes.extend_from_slice(&chunk);
    }

    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|source| RepositoryError::Archive {
            source: Arc::new(source),
        })?;

    for entry in entries {
        let mut entry = entry.map_err(|source| RepositoryError::Archive {
            source: Arc::new(source),
        })?;
        let is_description = {
            let path = entry.path().map_err(|source| RepositoryError::Archive {
                source: Arc::new(source),
            })?;

            path_is_top_level_description(&path, package)
        };

        if !is_description {
            continue;
        }

        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .map_err(|source| RepositoryError::Archive {
                source: Arc::new(source),
            })?;

        return Ok(Description::parse(&body));
    }

    Err(RepositoryError::DescriptionNotFound {
        package: package.to_string(),
    })
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

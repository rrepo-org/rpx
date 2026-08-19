use super::{ArchiveSupport, PackageRepository, RepositoryError};
use crate::{http, resolver::PackageVersion};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use moka::future::Cache;
use r_description::{RDescription, Version};
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
    packages: Cache<(), Arc<http::CranPackagesIndex>>,
    archive_versions: Cache<String, BTreeSet<Version>>,
    descriptions: Cache<(String, Version), Arc<RDescription>>,
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

    async fn packages_index(&self) -> Result<Arc<http::CranPackagesIndex>, RepositoryError> {
        self.packages
            .try_get_with((), async {
                let text = http::cran_packages(&self.url)
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
                let index = text.parse::<http::CranPackagesIndex>().map_err(|details| {
                    RepositoryError::InvalidData {
                        resource: "CRAN PACKAGES index".to_string(),
                        details,
                    }
                })?;

                Ok::<Arc<http::CranPackagesIndex>, RepositoryError>(Arc::new(index))
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
            .packages
            .iter()
            .filter_map(|package| {
                let version = match package.version.parse::<Version>() {
                    Ok(version) => version,
                    Err(error) => {
                        tracing::debug!(
                            package = %package.package,
                            version = %package.version,
                            repository = %self.url,
                            error = %error,
                            "skipping package with invalid latest version"
                        );
                        return None;
                    }
                };

                Some((
                    package.package.clone(),
                    PackageVersion::new(version, Arc::clone(&repository)),
                ))
            })
            .collect())
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let index = self.packages_index().await?;
        let mut versions = index
            .packages
            .iter()
            .filter(|entry| entry.package == package)
            .map(|entry| {
                entry
                    .version
                    .parse::<Version>()
                    .map_err(|source| RepositoryError::InvalidData {
                        resource: "package version".to_string(),
                        details: source.to_string(),
                    })
            })
            .collect::<Result<BTreeSet<_>, RepositoryError>>()?;

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
    ) -> Result<Arc<RDescription>, RepositoryError> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let index = self.packages_index().await?;
                let version_string = version.to_string();

                let description = if let Some(entry) = index
                    .packages
                    .iter()
                    .find(|entry| entry.package == package && entry.version == version_string)
                {
                    packages_entry_to_description(entry)?
                } else {
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

                Ok::<Arc<RDescription>, RepositoryError>(Arc::new(description))
            })
            .await
            .map_err(Arc::unwrap_or_clone)
    }
}

fn packages_entry_to_description(
    entry: &http::CranPackageIndexEntry,
) -> Result<RDescription, RepositoryError> {
    let mut description = RDescription::parse("");

    description
        .set_package(&entry.package)
        .map_err(|source| RepositoryError::InvalidData {
            resource: "Package in CRAN PACKAGES index".to_string(),
            details: source.to_string(),
        })?;
    let version =
        entry
            .version
            .parse::<Version>()
            .map_err(|source| RepositoryError::InvalidData {
                resource: "Version in CRAN PACKAGES index".to_string(),
                details: source.to_string(),
            })?;
    description.set_version(&version);

    if !entry.depends.is_empty() {
        description.set_depends(entry.depends.clone());
    }

    if !entry.imports.is_empty() {
        description.set_imports(entry.imports.clone());
    }

    if !entry.suggests.is_empty() {
        description.set_suggests(entry.suggests.clone());
    }

    if !entry.linking_to.is_empty() {
        description.set_linking_to(entry.linking_to.clone());
    }

    if let Some(system_requirements) = &entry.system_requirements {
        description
            .set_system_requirements(system_requirements)
            .map_err(|source| RepositoryError::InvalidData {
                resource: "SystemRequirements in CRAN PACKAGES index".to_string(),
                details: source.to_string(),
            })?;
    }

    Ok(description)
}

async fn description_from_source_tarball_response(
    response: reqwest::Response,
    package: &str,
) -> Result<RDescription, RepositoryError> {
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

        return Ok(RDescription::parse(&body));
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

use super::{ArchiveSupport, PackageRepository, RepositoryError};
use crate::{http, resolver::PackageVersion};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use moka::future::Cache;
use r_description::lossless::{RDescription, Version};
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

impl CranRepository {
    pub fn new(mut url: Url, archives: ArchiveSupport) -> Self {
        url.path_segments_mut()
            .expect("repository base URL should support path segments")
            .pop_if_empty();

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

    async fn packages_index(
        &self,
        client: &http::HttpClient,
    ) -> Result<Arc<http::CranPackagesIndex>, RepositoryError> {
        self.packages
            .try_get_with((), async {
                let text = http::cran_packages(client, &self.url)
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

    async fn packages(
        &self,
        client: &http::HttpClient,
    ) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let index = self.packages_index(client).await?;

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

    async fn versions(
        &self,
        client: &http::HttpClient,
        package: &str,
    ) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
        let repository: Arc<dyn PackageRepository> = Arc::new(self.clone());
        let index = self.packages_index(client).await?;
        let mut versions = index
            .packages
            .iter()
            .filter(|entry| entry.package == package)
            .map(|entry| {
                entry
                    .version
                    .parse::<Version>()
                    .map_err(|details| RepositoryError::InvalidData {
                        resource: "package version".to_string(),
                        details,
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
                    let text = http::cran_package_archive_listing(client, &self.url, package)
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
                            .map_err(|details| RepositoryError::InvalidData {
                                resource: "CRAN package archive listing".to_string(),
                                details,
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
        client: &http::HttpClient,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, RepositoryError> {
        let key = (package.to_string(), version.clone());

        self.descriptions
            .try_get_with(key, async {
                let index = self.packages_index(client).await?;
                let version_string = version.to_string();

                let description = if let Some(entry) = index
                    .packages
                    .iter()
                    .find(|entry| entry.package == package && entry.version == version_string)
                {
                    packages_entry_to_description(entry)
                } else {
                    let response = http::cran_archive_source_tarball(
                        client,
                        &self.url,
                        package,
                        &version_string,
                    )
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

fn packages_entry_to_description(entry: &http::CranPackageIndexEntry) -> RDescription {
    let mut description = RDescription::new();

    description.set_package(&entry.package);
    description.set_version(&entry.version);

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
        description.set_system_requirements(&[system_requirements]);
    }

    description
}

async fn description_from_source_tarball_response(
    response: reqwest::Response,
    package: &str,
) -> Result<RDescription, RepositoryError> {
    let mut bytes = Vec::with_capacity(response.content_length().unwrap_or_default() as usize);
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

        return body
            .parse::<RDescription>()
            .map_err(|source| RepositoryError::Description {
                location: format!("in source package for {package}"),
                source: Arc::new(source),
            });
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

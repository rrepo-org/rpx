use super::{ArchiveSupport, PackageRepository};
use crate::http;
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
    client: http::HttpClient,
    archives: ArchiveSupport,
    packages: Cache<(), Arc<http::CranPackagesIndex>>,
    archive_versions: Cache<String, BTreeSet<Version>>,
    descriptions: Cache<(String, Version), Arc<RDescription>>,
}

impl CranRepository {
    pub fn new(client: http::HttpClient, url: Url, archives: ArchiveSupport) -> Self {
        Self {
            url,
            client,
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

    async fn packages_index(&self) -> Result<Arc<http::CranPackagesIndex>, String> {
        self.packages
            .try_get_with((), async {
                let text = http::cran_packages(&self.client, &self.url)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?
                    .text()
                    .await
                    .map_err(|error| error.to_string())?;

                let index = text
                    .parse::<http::CranPackagesIndex>()
                    .map_err(|error| error.to_string())?;

                Ok::<Arc<http::CranPackagesIndex>, String>(Arc::new(index))
            })
            .await
            .map_err(|error| error.as_ref().clone())
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
            .is_some_and(|other| self.url == other.url)
    }

    async fn packages(&self) -> Result<BTreeMap<String, Version>, String> {
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

                Some((package.package.clone(), version))
            })
            .collect())
    }

    async fn versions(&self, package: &str) -> Result<BTreeSet<Version>, String> {
        let index = self.packages_index().await?;
        let mut versions = index
            .packages
            .iter()
            .filter(|entry| entry.package == package)
            .map(|entry| {
                entry.version.parse::<Version>().map_err(|error| {
                    format!("invalid version {} for {package}: {error}", entry.version)
                })
            })
            .collect::<Result<BTreeSet<_>, String>>()?;

        if self.archives == ArchiveSupport::Available {
            let archived_versions = self
                .archive_versions
                .try_get_with(package.to_string(), async {
                    let listing =
                        http::cran_package_archive_listing(&self.client, &self.url, package)
                            .await
                            .map_err(|error| error.to_string())?
                            .error_for_status()
                            .map_err(|error| error.to_string())?
                            .text()
                            .await
                            .map_err(|error| error.to_string())?
                            .parse::<http::CranPackageArchiveListing>()
                            .map_err(|error| error.to_string())?;

                    Ok::<BTreeSet<Version>, String>(listing.versions.into_iter().collect())
                })
                .await
                .map_err(|error| error.as_ref().clone())?;

            versions.extend(archived_versions);
        }

        tracing::trace!(
            package,
            repository = %self.url,
            versions = versions.len(),
            "loaded package versions"
        );

        Ok(versions)
    }

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, String> {
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
                    packages_entry_to_description(entry)
                } else {
                    let response = http::cran_archive_source_tarball(
                        &self.client,
                        &self.url,
                        package,
                        &version_string,
                    )
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?;

                    description_from_source_tarball_response(response, package).await?
                };

                tracing::trace!(
                    package,
                    version = %version,
                    repository = %self.url,
                    "fetched package description"
                );

                Ok::<Arc<RDescription>, String>(Arc::new(description))
            })
            .await
            .map_err(|error| error.as_ref().clone())
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
) -> Result<RDescription, String> {
    let mut bytes = Vec::with_capacity(response.content_length().unwrap_or_default() as usize);
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|error| format!("failed to read source package response body: {error}"))?
    {
        bytes.extend_from_slice(&chunk);
    }

    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("failed to read source package archive: {error}"))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|error| format!("failed to read source package archive entry: {error}"))?;
        let is_description = {
            let path = entry
                .path()
                .map_err(|error| format!("failed to read source package archive path: {error}"))?;

            path_is_top_level_description(&path, package)
        };

        if !is_description {
            continue;
        }

        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .map_err(|error| format!("failed to read DESCRIPTION from source package: {error}"))?;

        return body.parse::<RDescription>().map_err(|error| {
            format!("failed to parse DESCRIPTION from source package for {package}: {error}")
        });
    }

    Err(format!(
        "source package does not contain {package}/DESCRIPTION"
    ))
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

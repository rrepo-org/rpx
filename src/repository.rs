mod cran;
mod local;
mod rrepo;

use crate::http;
use async_trait::async_trait;
use r_description::lossless::{RDescription, Version};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    sync::Arc,
};

pub use cran::CranRepository;
pub use local::LocalRepository;
pub use rrepo::RrepoRepository;

pub const DEFAULT_REGISTRY_BASE_URL: &str = "https://upstream.rrepo.dev/cran";

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveSupport {
    Available,
    Unavailable,
}

#[async_trait]
pub trait PackageRepository: Any + Debug + Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn equals(&self, other: &dyn PackageRepository) -> bool;

    async fn packages(&self) -> Result<BTreeMap<String, Version>, String>;

    async fn versions(&self, package: &str) -> Result<BTreeSet<Version>, String>;

    async fn description(
        &self,
        package: &str,
        version: &Version,
    ) -> Result<Arc<RDescription>, String>;
}

impl dyn PackageRepository {
    pub fn downcast_ref<T: PackageRepository + 'static>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }

    pub async fn from_url(
        client: &http::HttpClient,
        value: &str,
    ) -> Result<Arc<dyn PackageRepository>, String> {
        let normalized_url = normalize_repository_url(value);
        let url = Url::parse(&normalized_url)
            .map_err(|error| format!("invalid repository URL {normalized_url}: {error}"))?;

        let rrepo_url = url.clone();
        let rrepo_probe = async {
            http::rrepo_repository_packages(client, &rrepo_url)
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?;

            Ok::<Arc<dyn PackageRepository>, String>(Arc::new(RrepoRepository::new(
                client.clone(),
                rrepo_url,
            )))
        };

        let cran_url = url;
        let cran_probe = async {
            let packages_probe = async {
                http::cran_packages(client, &cran_url)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())
            };
            let archive_probe = async {
                http::cran_archive_root(client, &cran_url)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())
            };

            let (packages_result, archive_result) = tokio::join!(packages_probe, archive_probe);
            packages_result?;

            let archives = match archive_result {
                Ok(_) => ArchiveSupport::Available,
                Err(error)
                    if error.contains("404 Not Found") || error.contains("403 Forbidden") =>
                {
                    ArchiveSupport::Unavailable
                }
                Err(error) => return Err(error),
            };

            Ok::<Arc<dyn PackageRepository>, String>(Arc::new(CranRepository::new(
                client.clone(),
                cran_url,
                archives,
            )))
        };

        tokio::pin!(rrepo_probe);
        tokio::pin!(cran_probe);

        tokio::select! {
            rrepo_result = &mut rrepo_probe => {
                match rrepo_result {
                    Ok(repository) => Ok(repository),
                    Err(rrepo_error) => {
                        match cran_probe.await {
                            Ok(repository) => Ok(repository),
                            Err(cran_error) => Err(format!(
                                "not an rrepo API ({rrepo_error}) or CRAN-like repository ({cran_error})"
                            )),
                        }
                    }
                }
            }

            cran_result = &mut cran_probe => {
                match cran_result {
                    Ok(repository) => Ok(repository),
                    Err(cran_error) => {
                        match rrepo_probe.await {
                            Ok(repository) => Ok(repository),
                            Err(rrepo_error) => Err(format!(
                                "not an rrepo API ({rrepo_error}) or CRAN-like repository ({cran_error})"
                            )),
                        }
                    }
                }
            }
        }
    }
}

impl PartialEq for dyn PackageRepository {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for dyn PackageRepository {}

pub fn normalize_repository_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

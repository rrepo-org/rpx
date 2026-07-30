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

#[async_trait]
pub trait RepositoryFromUrl: PackageRepository + Sized {
    async fn from_url(client: http::HttpClient, url: Url) -> Result<Self, String>;
}

impl dyn PackageRepository {
    pub fn downcast_ref<T: PackageRepository + 'static>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }
}

impl PartialEq for dyn PackageRepository {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for dyn PackageRepository {}

pub async fn from_url(
    client: &http::HttpClient,
    value: &str,
) -> Result<Arc<dyn PackageRepository>, String> {
    let normalized_url = normalize_repository_url(value);
    let url = Url::parse(&normalized_url)
        .map_err(|error| format!("invalid repository URL {normalized_url}: {error}"))?;

    let rrepo = RrepoRepository::from_url(client.clone(), url.clone());
    let cran = CranRepository::from_url(client.clone(), url);
    tokio::pin!(rrepo);
    tokio::pin!(cran);

    tokio::select! {
        rrepo_result = &mut rrepo => {
            match rrepo_result {
                Ok(repository) => Ok(Arc::new(repository)),
                Err(rrepo_error) => {
                    match cran.await {
                        Ok(repository) => Ok(Arc::new(repository)),
                        Err(cran_error) => Err(format!(
                            "not an rrepo API ({rrepo_error}) or CRAN-like repository ({cran_error})"
                        )),
                    }
                }
            }
        }

        cran_result = &mut cran => {
            match cran_result {
                Ok(repository) => Ok(Arc::new(repository)),
                Err(cran_error) => {
                    match rrepo.await {
                        Ok(repository) => Ok(Arc::new(repository)),
                        Err(rrepo_error) => Err(format!(
                            "not an rrepo API ({rrepo_error}) or CRAN-like repository ({cran_error})"
                        )),
                    }
                }
            }
        }
    }
}

pub fn normalize_repository_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

use pubgrub::{
    Dependencies, DependencyConstraints, DependencyProvider, PackageResolutionStatistics,
    PubGrubError, Ranges, resolve,
};
use r_description::{
    VersionConstraint,
    lossless::{RDescription, Relation, Version},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{
    http,
    repository::{DEFAULT_REGISTRY_BASE_URL, PackageRepository, RepositoryError, RrepoRepository},
};

const ROOT_PACKAGE: &str = "__rpx_root__";
const DESCRIPTION_PREFETCH_WORKERS: usize = 50;
const BASE_PACKAGES: &[&str] = &[
    "base",
    "compiler",
    "datasets",
    "graphics",
    "grDevices",
    "grid",
    "methods",
    "parallel",
    "splines",
    "stats",
    "stats4",
    "tcltk",
    "tools",
    "utils",
];
#[derive(Debug, Clone, Error)]
pub(crate) enum ProviderError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),

    #[error("failed to load dependency metadata from {repository}")]
    DependencyMetadata {
        repository: String,
        #[source]
        source: RepositoryError,
    },
}

#[derive(Debug, Error)]
pub(crate) enum ResolutionError {
    #[error("no repositories configured")]
    NoRepositories,

    #[error(transparent)]
    PubGrub(#[from] PubGrubError<RDependencyProvider>),

    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct PackageVersion {
    version: Version,
    repository: Arc<dyn PackageRepository>,
}

impl PackageVersion {
    pub fn new(version: Version, repository: Arc<dyn PackageRepository>) -> Self {
        Self {
            version,
            repository,
        }
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn repository(&self) -> &Arc<dyn PackageRepository> {
        &self.repository
    }
}

impl PartialEq for PackageVersion {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
    }
}

impl Eq for PackageVersion {}

impl PartialOrd for PackageVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PackageVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.version.cmp(&other.version)
    }
}

impl std::hash::Hash for PackageVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.version.hash(state);
    }
}
impl std::fmt::Display for PackageVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} from {}", self.version, self.repository)
    }
}

#[derive(Debug)]
pub(crate) struct RDependencyProvider {
    client: http::HttpClient,
    repositories: Vec<Arc<dyn PackageRepository>>,
    root_dependencies: BTreeMap<String, Ranges<PackageVersion>>,
    preferred_versions: BTreeMap<String, PackageVersion>,
    description_prefetch_permits: Arc<Semaphore>,
}

impl RDependencyProvider {
    fn new(
        client: http::HttpClient,
        repositories: Vec<Arc<dyn PackageRepository>>,
        root_dependencies: BTreeMap<String, Ranges<PackageVersion>>,
        preferred_versions: BTreeMap<String, PackageVersion>,
    ) -> Self {
        Self {
            client,
            repositories,
            root_dependencies,
            preferred_versions,
            description_prefetch_permits: Arc::new(Semaphore::new(DESCRIPTION_PREFETCH_WORKERS)),
        }
    }

    fn prefetch_descriptions(
        &self,
        constraints: &DependencyConstraints<String, Ranges<PackageVersion>>,
    ) {
        let parent_span = tracing::Span::current();

        for (package, range) in constraints {
            let client = self.client.clone();
            let repositories = self.repositories.clone();
            let preferred_versions = self.preferred_versions.clone();
            let permits = Arc::clone(&self.description_prefetch_permits);
            let package = package.clone();
            let range = range.clone();
            let span = tracing::info_span!(
                parent: &parent_span,
                "prefetch_description",
                package = %package,
                version = tracing::field::Empty,
                repository = tracing::field::Empty,
                stage = "queued",
            );

            tokio::runtime::Handle::current().spawn(
                async move {
                    tracing::Span::current().record("stage", "waiting permit");
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };

                    tracing::Span::current().record("stage", "selecting version");
                    let version = match choose_package_version(
                        &client,
                        &repositories,
                        &preferred_versions,
                        &package,
                        &range,
                    )
                    .await
                    {
                        Ok(Some(version)) => version,
                        Ok(None) => {
                            tracing::Span::current().record("stage", "missing");
                            return;
                        }
                        Err(error) => {
                            tracing::debug!(error = %error, "description prefetch version selection failed");
                            tracing::Span::current().record("stage", "failed");
                            return;
                        }
                    };

                    tracing::Span::current().record("version", version.version().to_string());
                    tracing::Span::current().record(
                        "repository",
                        version.repository().to_string(),
                    );
                    tracing::Span::current().record("stage", "fetching description");

                    if let Err(error) = version
                        .repository()
                        .description(&client, &package, version.version())
                        .await
                    {
                        tracing::debug!(error = %error, "description prefetch failed");
                        tracing::Span::current().record("stage", "failed");
                        return;
                    }

                    tracing::Span::current().record("stage", "done");
                }
                .instrument(span),
            );
        }
    }
}

impl DependencyProvider for RDependencyProvider {
    type P = String;
    type V = PackageVersion;
    type VS = Ranges<PackageVersion>;
    type Priority = u32;
    type M = String;
    type Err = ProviderError;

    fn prioritize(
        &self,
        _package: &Self::P,
        _range: &Self::VS,
        package_conflicts_counts: &PackageResolutionStatistics,
    ) -> Self::Priority {
        package_conflicts_counts.conflict_count()
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        if package == ROOT_PACKAGE {
            let root = root_package_version();

            return Ok(range.contains(&root).then_some(root));
        }

        tokio::runtime::Handle::current().block_on(choose_package_version(
            &self.client,
            &self.repositories,
            &self.preferred_versions,
            package,
            range,
        ))
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        if package == ROOT_PACKAGE {
            let constraints = self
                .root_dependencies
                .iter()
                .map(|(package, range)| (package.clone(), range.clone()))
                .collect::<DependencyConstraints<_, _>>();

            self.prefetch_descriptions(&constraints);

            return Ok(Dependencies::Available(constraints));
        }

        if is_base_package(package) {
            return Ok(Dependencies::Available(DependencyConstraints::default()));
        }

        let description = tokio::runtime::Handle::current()
            .block_on(
                version
                    .repository
                    .description(&self.client, package, &version.version),
            )
            .map_err(|source| ProviderError::DependencyMetadata {
                repository: version.repository.to_string(),
                source,
            })?;

        let constraints =
            dependency_constraints_from_description(Arc::clone(&version.repository), &description);

        self.prefetch_descriptions(&constraints);

        Ok(Dependencies::Available(constraints))
    }
}

async fn choose_package_version(
    client: &http::HttpClient,
    repositories: &[Arc<dyn PackageRepository>],
    preferred_versions: &BTreeMap<String, PackageVersion>,
    package: &str,
    range: &Ranges<PackageVersion>,
) -> Result<Option<PackageVersion>, ProviderError> {
    if let Some(preferred) = preferred_versions.get(package) {
        if range.contains(preferred) {
            return Ok(Some(preferred.clone()));
        }
    }

    let candidates = futures_util::future::join_all(repositories.iter().enumerate().map(
        |(repository_index, repository)| async move {
            let version =
                choose_repository_version(client, repository.as_ref(), package, range).await?;

            Ok::<_, ProviderError>(version.map(|version| (version, repository_index)))
        },
    ))
    .await
    .into_iter()
    .collect::<Result<Vec<_>, ProviderError>>()?;

    Ok(candidates
        .into_iter()
        .flatten()
        .max_by(|(left_version, left_repo), (right_version, right_repo)| {
            left_version
                .cmp(right_version)
                // For equal versions, prefer lower repository index.
                // `max_by` wants the preferred item to compare greater,
                // so reverse the repo-index comparison.
                .then_with(|| right_repo.cmp(left_repo))
        })
        .map(|(version, _repository_index)| version))
}

async fn choose_repository_version(
    client: &http::HttpClient,
    repository: &dyn PackageRepository,
    package: &str,
    range: &Ranges<PackageVersion>,
) -> Result<Option<PackageVersion>, ProviderError> {
    let packages = repository.packages(client).await?;

    let Some(latest) = packages.get(package) else {
        return Ok(None);
    };

    if range.contains(latest) {
        return Ok(Some(latest.clone()));
    }

    Ok(repository
        .versions(client, package)
        .await?
        .into_iter()
        .filter(|version| range.contains(version))
        .max())
}

fn dependency_constraints_from_description(
    repository: Arc<dyn PackageRepository>,
    description: &RDescription,
) -> DependencyConstraints<String, Ranges<PackageVersion>> {
    description
        .depends()
        .into_iter()
        .chain(description.imports())
        .chain(description.linking_to())
        .flat_map(|relations| relations.iter())
        .filter(|relation| relation.name() != "R")
        .filter(|relation| !is_base_package(&relation.name()))
        .map(|relation| {
            (
                relation.name().to_string(),
                relation_package_range(Arc::clone(&repository), &relation),
            )
        })
        .collect()
}

fn root_package_version() -> PackageVersion {
    let url = reqwest::Url::parse(DEFAULT_REGISTRY_BASE_URL)
        .expect("default registry URL should be valid");

    let repository: Arc<dyn PackageRepository> = Arc::new(RrepoRepository::new(url));

    PackageVersion::new(
        Version::from_str("0.0.0").expect("root version should parse"),
        repository,
    )
}

fn relation_package_range(
    repository: Arc<dyn PackageRepository>,
    relation: &Relation,
) -> Ranges<PackageVersion> {
    let relation_version = relation.version();

    let Some((operator, version)) = relation_version.as_ref() else {
        return Ranges::full();
    };

    let bound = PackageVersion {
        version: version.clone(),
        repository,
    };

    match operator {
        VersionConstraint::Equal => Ranges::singleton(bound),
        VersionConstraint::GreaterThan => Ranges::strictly_higher_than(bound),
        VersionConstraint::GreaterThanEqual => Ranges::higher_than(bound),
        VersionConstraint::LessThan => Ranges::strictly_lower_than(bound),
        VersionConstraint::LessThanEqual => Ranges::lower_than(bound),
        VersionConstraint::NotEqual => Ranges::singleton(bound).complement(),
    }
}

pub fn is_base_package(package: &str) -> bool {
    BASE_PACKAGES.contains(&package)
}

pub(crate) async fn resolve_from_registry(
    client: http::HttpClient,
    repositories: Vec<Arc<dyn PackageRepository>>,
    root_relations: BTreeSet<Relation>,
    preferred_versions: BTreeMap<String, PackageVersion>,
) -> Result<Vec<(String, PackageVersion)>, ResolutionError> {
    if root_relations.is_empty() {
        return Ok(Vec::new());
    }

    let root_count = root_relations.len();
    let root_dependencies = root_dependency_ranges(&repositories, &root_relations)?;

    let span = tracing::info_span!(
        "resolve_dependencies",
        roots = root_count,
        repositories = repositories.len(),
        preferred = preferred_versions.len(),
        selected = tracing::field::Empty,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    span.pb_set_message("resolve dependencies");
    span.pb_start();

    let root_version = root_package_version();

    let provider =
        RDependencyProvider::new(client, repositories, root_dependencies, preferred_versions);

    let resolve_span = span.clone();
    let selected = tokio::task::spawn_blocking(move || {
        let _enter = resolve_span.enter();
        resolve_span.record("stage", "solving");

        let selected = resolve(&provider, ROOT_PACKAGE.to_string(), root_version)?;

        let mut selected = selected
            .into_iter()
            .filter(|(package, _)| package != ROOT_PACKAGE)
            .collect::<Vec<_>>();

        selected.sort_by(|left, right| left.0.cmp(&right.0));
        resolve_span.record("selected", selected.len());

        Ok::<_, PubGrubError<RDependencyProvider>>(selected)
    })
    .await??;

    span.record("stage", "done");
    span.record("selected", selected.len());
    span.pb_set_finish_message(&format!("resolve dependencies {} packages", selected.len()));

    Ok(selected)
}

fn root_dependency_ranges(
    repositories: &[Arc<dyn PackageRepository>],
    roots: &BTreeSet<Relation>,
) -> Result<BTreeMap<String, Ranges<PackageVersion>>, ResolutionError> {
    let mut root_dependencies: BTreeMap<String, Ranges<PackageVersion>> = BTreeMap::new();

    for relation in roots {
        let package = relation.name();
        if is_base_package(&package) {
            continue;
        }

        let repository = repositories
            .first()
            .map(Arc::clone)
            .ok_or(ResolutionError::NoRepositories)?;
        let range = relation_package_range(Arc::clone(&repository), relation);
        match root_dependencies.entry(package) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let combined = entry.get().intersection(&range);
                entry.insert(combined);
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(range);
            }
        }
    }

    Ok(root_dependencies)
}

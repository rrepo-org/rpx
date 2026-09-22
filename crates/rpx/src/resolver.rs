use pubgrub::{
    Dependencies, DependencyConstraints, DependencyProvider, PackageResolutionStatistics,
    PubGrubError, Ranges, resolve,
};
use r_description::Description;
use r_metadata::{Relation, RequirementVersion, Version, VersionRequirement};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{
    description::{ProjectType, description_identity, required_dependencies},
    r::{BasePackagesError, base_packages},
    repository::{
        ArchiveSupport, LocalRepository, PackageRepository, RepositoryError, built_in_repository,
    },
};

const DESCRIPTION_PREFETCH_WORKERS: usize = 50;

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
    #[error(transparent)]
    BasePackages(#[from] BasePackagesError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    PubGrub(#[from] PubGrubError<RDependencyProvider>),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

/// A solver version carries its source, but range equality intentionally uses
/// only R version semantics. It must not be used as a source-aware metadata key.
#[derive(Debug, Clone)]
pub struct PackageVersion {
    version: Version,
    repository: PackageRepository,
}

impl PackageVersion {
    pub fn new(version: Version, repository: impl Into<PackageRepository>) -> Self {
        Self {
            version,
            repository: repository.into(),
        }
    }
    pub fn version(&self) -> &Version {
        &self.version
    }
    pub fn repository(&self) -> &PackageRepository {
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
        self.version.fmt(f)
    }
}

/// Native metadata dispatch shared by solving, prefetch, and hydration. Local
/// overrides and Git commit state stay on the original shared source handles.
pub(crate) async fn package_description(
    repository: &PackageRepository,
    package: &str,
    version: &Version,
) -> Result<Arc<Description>, RepositoryError> {
    let description = match repository {
        PackageRepository::Rrepo(repo) => return repo.description(package, version).await,
        PackageRepository::Cran(repo) => return repo.description(package, version).await,
        PackageRepository::Git(repo) => repo.description().await?,
        PackageRepository::Local(repo) => repo.description().await?,
    };
    let (name, found_version) =
        description_identity(format!("DESCRIPTION from {repository}"), &description)?;
    if name != package || &found_version != version {
        return Err(RepositoryError::RepositoryPackageVersionNotFound {
            repository: repository.to_string(),
            package: package.into(),
            version: version.clone(),
        });
    }
    Ok(description)
}

#[derive(Debug)]
pub(crate) struct RDependencyProvider {
    repositories: Vec<PackageRepository>,
    root: Arc<LocalRepository>,
    root_type: ProjectType,
    root_relations: BTreeSet<Relation>,
    preferred_versions: BTreeMap<String, Version>,
    base_packages: BTreeSet<String>,
    description_prefetch_permits: Arc<Semaphore>,
}

impl RDependencyProvider {
    fn new(
        repositories: Vec<PackageRepository>,
        root: Arc<LocalRepository>,
        root_type: ProjectType,
        root_relations: BTreeSet<Relation>,
        preferred_versions: BTreeMap<String, Version>,
        base_packages: BTreeSet<String>,
    ) -> Self {
        Self {
            repositories,
            root,
            root_type,
            root_relations,
            preferred_versions,
            base_packages,
            description_prefetch_permits: Arc::new(Semaphore::new(DESCRIPTION_PREFETCH_WORKERS)),
        }
    }

    fn root_package(&self) -> Result<(String, PackageVersion), ProviderError> {
        let (name, version) = tokio::runtime::Handle::current().block_on(self.root.package())?;
        Ok((name, PackageVersion::new(version, self.root.clone())))
    }

    fn prefetch_descriptions(
        &self,
        constraints: &DependencyConstraints<String, Ranges<PackageVersion>>,
    ) -> Result<(), ProviderError> {
        let (root_package, _) = self.root_package()?;
        constraints
            .iter()
            .filter(|(package, _)| *package != &root_package)
            .for_each(|(package, range)| {
                let repositories = self.repositories.clone();
                let preferred_versions = self.preferred_versions.clone();
                let permits = self.description_prefetch_permits.clone();
                let package = package.clone();
                let range = range.clone();
                let span = tracing::info_span!("prefetch_description", package = %package);
                tokio::runtime::Handle::current().spawn(
                    async move {
                        let Ok(_permit) = permits.acquire_owned().await else {
                            return;
                        };
                        match choose_package_version(
                            &repositories,
                            &preferred_versions,
                            &package,
                            &range,
                        )
                        .await
                        {
                            Ok(Some(version)) => {
                                if let Err(error) = package_description(
                                    version.repository(),
                                    &package,
                                    version.version(),
                                )
                                .await
                                {
                                    tracing::debug!(%error, "description prefetch failed");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::debug!(%error, "description prefetch selection failed")
                            }
                        }
                    }
                    .instrument(span),
                );
            });
        Ok(())
    }

    fn dependency_ranges(
        &self,
        relations: &BTreeSet<Relation>,
    ) -> Result<DependencyConstraints<String, Ranges<PackageVersion>>, ProviderError> {
        let mut constraints = dependency_ranges_from_relations(relations, &self.base_packages);
        if self.root_type == ProjectType::Project {
            let (root_package, _) = self.root_package()?;
            if relations
                .iter()
                .any(|relation| relation.package() == root_package)
            {
                constraints.insert(root_package, Ranges::empty());
            }
        }
        Ok(constraints)
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
        _: &Self::P,
        _: &Self::VS,
        stats: &PackageResolutionStatistics,
    ) -> Self::Priority {
        stats.conflict_count()
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        let (root, version) = self.root_package()?;
        if package == &root {
            return Ok(range.contains(&version).then_some(version));
        }
        tokio::runtime::Handle::current().block_on(choose_package_version(
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
        let (root, _) = self.root_package()?;
        if package == &root {
            let constraints = self.dependency_ranges(&self.root_relations)?;
            self.prefetch_descriptions(&constraints)?;
            return Ok(Dependencies::Available(constraints));
        }
        if self.base_packages.contains(package) {
            return Ok(Dependencies::Available(DependencyConstraints::default()));
        }
        let description = tokio::runtime::Handle::current()
            .block_on(package_description(
                version.repository(),
                package,
                version.version(),
            ))
            .map_err(|source| ProviderError::DependencyMetadata {
                repository: version.repository.to_string(),
                source,
            })?;
        let relations = match required_dependencies(
            format!("{package} {version} from {}", version.repository),
            &description,
        ) {
            Ok(relations) => relations,
            Err(error) => {
                return Ok(Dependencies::Unavailable(format!(
                    "invalid dependency metadata: {}",
                    error.messages().join("; ")
                )));
            }
        };
        let constraints = self.dependency_ranges(&relations)?;
        self.prefetch_descriptions(&constraints)?;
        Ok(Dependencies::Available(constraints))
    }
}

async fn choose_package_version(
    repositories: &[PackageRepository],
    preferred_versions: &BTreeMap<String, Version>,
    package: &str,
    range: &Ranges<PackageVersion>,
) -> Result<Option<PackageVersion>, ProviderError> {
    let preferred = preferred_versions.get(package).filter(|preferred| {
        range.contains(&PackageVersion::new(
            (*preferred).clone(),
            built_in_repository(),
        ))
    });
    let candidates = futures_util::future::join_all(repositories.iter().enumerate().map(
        |(index, repository)| async move {
            let candidate =
                choose_repository_version(repository, package, range, preferred).await?;
            Ok::<_, ProviderError>(candidate.map(|candidate| (candidate, index)))
        },
    ))
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    Ok(candidates
        .into_iter()
        .flatten()
        .max_by(|(a, ai), (b, bi)| {
            preferred
                .is_some_and(|v| a.version() == v)
                .cmp(&preferred.is_some_and(|v| b.version() == v))
                .then_with(|| a.cmp(b))
                .then_with(|| bi.cmp(ai))
        })
        .map(|(version, _)| version))
}

async fn choose_repository_version(
    repository: &PackageRepository,
    package: &str,
    range: &Ranges<PackageVersion>,
    preferred: Option<&Version>,
) -> Result<Option<PackageVersion>, ProviderError> {
    let latest = match repository {
        PackageRepository::Rrepo(repo) => repo
            .packages()
            .await?
            .packages
            .iter()
            .find(|entry| entry.name == package)
            .and_then(|entry| entry.latest_version.parse::<Version>().ok()),
        PackageRepository::Cran(repo) => repo
            .packages_index()
            .await?
            .records()
            .find(|record| {
                record
                    .package()
                    .is_some_and(|name| name.as_str() == package)
            })
            .map(|record| {
                record
                    .parsed_version()
                    .expect("validated Version")
                    .expect("validated Version")
            }),
        PackageRepository::Git(repo) => {
            let (name, version) = repo.package().await?;
            (name == package).then_some(version)
        }
        PackageRepository::Local(repo) => {
            let (name, version) = repo.package().await?;
            (name == package).then_some(version)
        }
    };
    let Some(latest) = latest.map(|v| PackageVersion::new(v, repository.clone())) else {
        return Ok(None);
    };
    if range.contains(&latest) && (preferred.is_none() || preferred == Some(latest.version())) {
        return Ok(Some(latest));
    }
    let versions: BTreeSet<Version> = match repository {
        PackageRepository::Rrepo(repo) => repo.versions(package).await?.keys().cloned().collect(),
        PackageRepository::Cran(repo) if repo.archive_support() == ArchiveSupport::Available => {
            repo.archive_versions(package).await?
        }
        PackageRepository::Cran(_) | PackageRepository::Git(_) | PackageRepository::Local(_) => {
            BTreeSet::new()
        }
    };
    Ok(std::iter::once(latest)
        .chain(
            versions
                .into_iter()
                .map(|v| PackageVersion::new(v, repository.clone())),
        )
        .filter(|v| range.contains(v))
        .max_by(|a, b| {
            preferred
                .is_some_and(|v| a.version() == v)
                .cmp(&preferred.is_some_and(|v| b.version() == v))
                .then_with(|| a.cmp(b))
        }))
}

fn package_version_range_from_relation(relation: &Relation) -> Ranges<PackageVersion> {
    let bound = |version: &Version| PackageVersion::new(version.clone(), built_in_repository());
    match relation.requirement() {
        VersionRequirement::Any => Ranges::full(),
        VersionRequirement::Equal(RequirementVersion::Version(v)) => Ranges::singleton(bound(v)),
        VersionRequirement::GreaterThan(RequirementVersion::Version(v)) => {
            Ranges::strictly_higher_than(bound(v))
        }
        VersionRequirement::GreaterThanEqual(RequirementVersion::Version(v)) => {
            Ranges::higher_than(bound(v))
        }
        VersionRequirement::LessThan(RequirementVersion::Version(v)) => {
            Ranges::strictly_lower_than(bound(v))
        }
        VersionRequirement::LessThanEqual(RequirementVersion::Version(v)) => {
            Ranges::lower_than(bound(v))
        }
        VersionRequirement::NotEqual(RequirementVersion::Version(v)) => {
            Ranges::singleton(bound(v)).complement()
        }
        _ => unreachable!("R revision requirement reached the package version resolver"),
    }
}

pub(crate) async fn resolve_from_registry(
    repositories: Vec<PackageRepository>,
    root: Arc<LocalRepository>,
    root_type: ProjectType,
    root_relations: BTreeSet<Relation>,
    preferred_versions: BTreeMap<String, Version>,
) -> Result<BTreeMap<String, PackageVersion>, ResolutionError> {
    let base_packages = base_packages().await?;
    let span = tracing::info_span!(
        "resolve_dependencies",
        roots = root_relations.len(),
        repositories = repositories.len(),
        preferred = preferred_versions.len(),
        selected = tracing::field::Empty,
        stage = "solving",
        indicatif.pb_show = true
    );
    span.pb_set_message("resolve dependencies");
    span.pb_start();
    let solve_span = span.clone();
    let selected = tokio::task::spawn_blocking(move || {
        let _entered = solve_span.enter();
        let provider = RDependencyProvider::new(
            repositories,
            root,
            root_type,
            root_relations,
            preferred_versions,
            base_packages,
        );
        let (name, version) = provider.root_package()?;
        let selected = resolve(&provider, name, version)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        Ok::<_, ResolutionError>(selected)
    })
    .await??;
    span.record("stage", "done");
    span.record("selected", selected.len());
    span.pb_set_finish_message(&format!("resolve dependencies {} packages", selected.len()));
    Ok(selected)
}

fn dependency_ranges_from_relations(
    relations: &BTreeSet<Relation>,
    base_packages: &BTreeSet<String>,
) -> DependencyConstraints<String, Ranges<PackageVersion>> {
    relations
        .iter()
        .filter(|r| !base_packages.contains(r.package()))
        .fold(
            DependencyConstraints::default(),
            |mut constraints, relation| {
                let range = package_version_range_from_relation(relation);
                constraints
                    .entry(relation.package().into())
                    .and_modify(|existing| *existing = existing.intersection(&range))
                    .or_insert(range);
                constraints
            },
        )
}

#[cfg(test)]
mod tests;

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
    description::required_dependencies,
    r::{BasePackagesError, base_packages},
    repository::{LocalRepository, PackageRepository, RepositoryError, built_in_repository},
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
        self.version.fmt(f)
    }
}

#[derive(Debug)]
pub(crate) struct RDependencyProvider {
    repositories: Vec<Arc<dyn PackageRepository>>,
    root: Arc<LocalRepository>,
    root_dependencies: DependencyConstraints<String, Ranges<PackageVersion>>,
    preferred_versions: BTreeMap<String, Version>,
    base_packages: BTreeSet<String>,
    description_prefetch_permits: Arc<Semaphore>,
}

impl RDependencyProvider {
    fn new(
        repositories: Vec<Arc<dyn PackageRepository>>,
        root: Arc<LocalRepository>,
        root_dependencies: DependencyConstraints<String, Ranges<PackageVersion>>,
        preferred_versions: BTreeMap<String, Version>,
        base_packages: BTreeSet<String>,
    ) -> Self {
        Self {
            repositories,
            root,
            root_dependencies,
            preferred_versions,
            base_packages,
            description_prefetch_permits: Arc::new(Semaphore::new(DESCRIPTION_PREFETCH_WORKERS)),
        }
    }

    fn prefetch_descriptions(
        &self,
        constraints: &DependencyConstraints<String, Ranges<PackageVersion>>,
    ) -> Result<(), ProviderError> {
        let (root_package, _) = self.root_package()?;
        let parent_span = tracing::Span::current();

        for (package, range) in constraints {
            if package == &root_package {
                continue;
            }

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
                        .description(&package, version.version())
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

        Ok(())
    }

    fn root_package(&self) -> Result<(String, PackageVersion), ProviderError> {
        tokio::runtime::Handle::current()
            .block_on(self.root.package())
            .map_err(Into::into)
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
        let (root_package, root_version) = self.root_package()?;
        if package == &root_package {
            return Ok(range.contains(&root_version).then_some(root_version));
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
        let (root_package, _) = self.root_package()?;
        if package == &root_package {
            let constraints = self
                .root_dependencies
                .iter()
                .map(|(package, range)| (package.clone(), range.clone()))
                .collect::<DependencyConstraints<_, _>>();

            self.prefetch_descriptions(&constraints)?;

            return Ok(Dependencies::Available(constraints));
        }

        if self.base_packages.contains(package) {
            return Ok(Dependencies::Available(DependencyConstraints::default()));
        }

        let description = tokio::runtime::Handle::current()
            .block_on(version.repository.description(package, &version.version))
            .map_err(|source| ProviderError::DependencyMetadata {
                repository: version.repository.to_string(),
                source,
            })?;

        let dependencies = dependencies_from_description(
            format!("{package} {} from {}", version.version, version.repository),
            &description,
            &self.base_packages,
        );
        let Dependencies::Available(constraints) = dependencies else {
            return Ok(dependencies);
        };

        self.prefetch_descriptions(&constraints)?;

        Ok(Dependencies::Available(constraints))
    }
}

async fn choose_package_version(
    repositories: &[Arc<dyn PackageRepository>],
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
        |(repository_index, repository)| async move {
            let version =
                choose_repository_version(repository.as_ref(), package, range, preferred).await?;

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
            let left_preferred =
                preferred.is_some_and(|preferred| left_version.version() == preferred);
            let right_preferred =
                preferred.is_some_and(|preferred| right_version.version() == preferred);

            left_preferred
                .cmp(&right_preferred)
                .then_with(|| left_version.cmp(right_version))
                // For equal versions, prefer lower repository index.
                // `max_by` wants the preferred item to compare greater,
                // so reverse the repo-index comparison.
                .then_with(|| right_repo.cmp(left_repo))
        })
        .map(|(version, _repository_index)| version))
}

async fn choose_repository_version(
    repository: &dyn PackageRepository,
    package: &str,
    range: &Ranges<PackageVersion>,
    preferred: Option<&Version>,
) -> Result<Option<PackageVersion>, ProviderError> {
    let packages = repository.packages().await?;

    let Some(latest) = packages.get(package).cloned() else {
        return Ok(None);
    };

    if preferred.is_some_and(|preferred| latest.version() == preferred) && range.contains(&latest) {
        return Ok(Some(latest));
    }

    if preferred.is_none() && range.contains(&latest) {
        return Ok(Some(latest));
    }

    let versions = repository.versions(package).await?;
    if let Some(preferred) = preferred
        && let Some(version) = versions
            .iter()
            .find(|version| version.version() == preferred && range.contains(version))
    {
        return Ok(Some(version.clone()));
    }

    Ok(std::iter::once(latest)
        .chain(versions)
        .filter(|version| range.contains(version))
        .max())
}

fn dependencies_from_description(
    source_name: impl Into<String>,
    description: &Description,
    base_packages: &BTreeSet<String>,
) -> Dependencies<String, Ranges<PackageVersion>, String> {
    match required_dependencies(source_name, description) {
        Ok(relations) => {
            Dependencies::Available(dependency_ranges_from_relations(&relations, base_packages))
        }
        Err(error) => Dependencies::Unavailable(format!(
            "invalid dependency metadata: {}",
            error.messages().join("; ")
        )),
    }
}

fn package_version_range_from_relation(relation: &Relation) -> Ranges<PackageVersion> {
    let bound = |version: &Version| PackageVersion::new(version.clone(), built_in_repository());

    match relation.requirement() {
        VersionRequirement::Any => Ranges::full(),
        VersionRequirement::Equal(RequirementVersion::Version(version)) => {
            Ranges::singleton(bound(version))
        }
        VersionRequirement::GreaterThan(RequirementVersion::Version(version)) => {
            Ranges::strictly_higher_than(bound(version))
        }
        VersionRequirement::GreaterThanEqual(RequirementVersion::Version(version)) => {
            Ranges::higher_than(bound(version))
        }
        VersionRequirement::LessThan(RequirementVersion::Version(version)) => {
            Ranges::strictly_lower_than(bound(version))
        }
        VersionRequirement::LessThanEqual(RequirementVersion::Version(version)) => {
            Ranges::lower_than(bound(version))
        }
        VersionRequirement::NotEqual(RequirementVersion::Version(version)) => {
            Ranges::singleton(bound(version)).complement()
        }
        VersionRequirement::Equal(RequirementVersion::Revision(_))
        | VersionRequirement::GreaterThan(RequirementVersion::Revision(_))
        | VersionRequirement::GreaterThanEqual(RequirementVersion::Revision(_))
        | VersionRequirement::LessThan(RequirementVersion::Revision(_))
        | VersionRequirement::LessThanEqual(RequirementVersion::Revision(_))
        | VersionRequirement::NotEqual(RequirementVersion::Revision(_)) => {
            unreachable!("R revision requirement reached the package version resolver")
        }
    }
}

pub(crate) async fn resolve_from_registry(
    repositories: Vec<Arc<dyn PackageRepository>>,
    root: Arc<LocalRepository>,
    root_relations: BTreeSet<Relation>,
    preferred_versions: BTreeMap<String, Version>,
) -> Result<BTreeMap<String, PackageVersion>, ResolutionError> {
    let base_packages = base_packages().await?;
    let root_count = root_relations.len();
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

    let resolve_span = span.clone();
    let selected = tokio::task::spawn_blocking(move || {
        let _enter = resolve_span.enter();
        resolve_span.record("stage", "solving");

        let (root_package, root_version) = tokio::runtime::Handle::current()
            .block_on(root.package())
            .map_err(ProviderError::from)?;
        let root_dependencies = dependency_ranges_from_relations(&root_relations, &base_packages);
        let provider = RDependencyProvider::new(
            repositories,
            root,
            root_dependencies,
            preferred_versions,
            base_packages,
        );

        let selected = resolve(&provider, root_package, root_version)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        resolve_span.record("selected", selected.len());

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
        .filter(|relation| !base_packages.contains(relation.package()))
        .fold(
            DependencyConstraints::default(),
            |mut dependencies, relation| {
                let range = package_version_range_from_relation(relation);
                dependencies
                    .entry(relation.package().to_string())
                    .and_modify(|existing| *existing = existing.intersection(&range))
                    .or_insert(range);
                dependencies
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::{
        any::Any,
        fmt,
        str::FromStr,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Debug)]
    struct TestRepository {
        name: &'static str,
        packages: BTreeMap<String, PackageVersion>,
        versions: BTreeMap<String, BTreeSet<PackageVersion>>,
        descriptions: BTreeMap<(String, Version), Arc<Description>>,
        package_queries: AtomicUsize,
        version_queries: Mutex<Vec<String>>,
        description_queries: Mutex<Vec<String>>,
    }

    impl TestRepository {
        fn empty(name: &'static str) -> Self {
            Self {
                name,
                packages: BTreeMap::new(),
                versions: BTreeMap::new(),
                descriptions: BTreeMap::new(),
                package_queries: AtomicUsize::new(0),
                version_queries: Mutex::new(Vec::new()),
                description_queries: Mutex::new(Vec::new()),
            }
        }
    }

    impl fmt::Display for TestRepository {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.name)
        }
    }

    #[async_trait]
    impl PackageRepository for TestRepository {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn equals(&self, other: &dyn PackageRepository) -> bool {
            other
                .downcast_ref::<Self>()
                .is_some_and(|other| self.name == other.name)
        }

        async fn packages(&self) -> Result<BTreeMap<String, PackageVersion>, RepositoryError> {
            self.package_queries.fetch_add(1, Ordering::SeqCst);
            Ok(self.packages.clone())
        }

        async fn versions(
            &self,
            package: &str,
        ) -> Result<BTreeSet<PackageVersion>, RepositoryError> {
            self.version_queries
                .lock()
                .expect("version query lock should not be poisoned")
                .push(package.to_string());
            Ok(self.versions.get(package).cloned().unwrap_or_default())
        }

        async fn description(
            &self,
            package: &str,
            version: &Version,
        ) -> Result<Arc<Description>, RepositoryError> {
            self.description_queries
                .lock()
                .expect("description query lock should not be poisoned")
                .push(package.to_string());
            self.descriptions
                .get(&(package.to_string(), version.clone()))
                .cloned()
                .ok_or_else(|| RepositoryError::InvalidData {
                    resource: format!("{package} {version}"),
                    details: "missing test description".to_string(),
                })
        }
    }

    fn version(value: &str, repository: Arc<dyn PackageRepository>) -> PackageVersion {
        PackageVersion::new(
            Version::from_str(value).expect("valid test version"),
            repository,
        )
    }

    fn local_repository(package: &str, version: &str) -> Arc<LocalRepository> {
        let description = Description::parse(&format!("Package: {package}\nVersion: {version}\n"));
        Arc::new(
            LocalRepository::new(std::path::PathBuf::from("unused")).with_description(description),
        )
    }

    #[test]
    fn rejects_malformed_hard_dependency_metadata() {
        let description = Description::parse(
            "Package: example\nVersion: 1.0.0\nImports: cli (>= invalid)\nSuggests: also-invalid (>= invalid)\n",
        );

        let Dependencies::Unavailable(reason) =
            dependencies_from_description("example 1.0.0", &description, &BTreeSet::new())
        else {
            panic!("malformed Imports should make the package version unavailable");
        };
        assert!(reason.contains("Imports"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backtracks_from_version_with_malformed_dependency_metadata() {
        let local_repository = local_repository("project", "1.0.0");
        let mut metadata_repository = TestRepository::empty("remote metadata");
        metadata_repository.descriptions.insert(
            (
                "example".to_string(),
                Version::from_str("2.0.0").expect("valid test version"),
            ),
            Arc::new(Description::parse(
                "Package: example\nVersion: 2.0.0\nImports: cli (>= invalid)\n",
            )),
        );
        metadata_repository.descriptions.insert(
            (
                "example".to_string(),
                Version::from_str("1.0.0").expect("valid test version"),
            ),
            Arc::new(Description::parse("Package: example\nVersion: 1.0.0\n")),
        );
        let metadata: Arc<dyn PackageRepository> = Arc::new(metadata_repository);
        let latest = version("2.0.0", Arc::clone(&metadata));
        let fallback = version("1.0.0", metadata);
        let mut remote_repository = TestRepository::empty("remote index");
        remote_repository
            .packages
            .insert("example".to_string(), latest.clone());
        remote_repository.versions.insert(
            "example".to_string(),
            BTreeSet::from([fallback.clone(), latest]),
        );

        let selected = resolve_from_registry(
            vec![Arc::new(remote_repository)],
            local_repository,
            BTreeSet::from([Relation::any("example").expect("valid example relation")]),
            BTreeMap::new(),
        )
        .await
        .expect("resolution should reject only the malformed version");

        assert_eq!(selected["example"], fallback);
    }

    #[test]
    fn ignores_unconsumed_malformed_suggests_metadata() {
        let description = Description::parse(
            "Package: example\nVersion: 1.0.0\nImports: cli\nSuggests: also-invalid (>= invalid)\n",
        );

        let Dependencies::Available(dependencies) =
            dependencies_from_description("example 1.0.0", &description, &BTreeSet::new())
        else {
            panic!("malformed Suggests should not make the package version unavailable");
        };
        assert!(dependencies.contains_key("cli"));
    }

    #[test]
    fn intersects_transitive_constraints_across_dependency_fields() {
        let description = Description::parse(
            "Package: example\nVersion: 1.0.0\nDepends: cli (>= 1.0.0)\nImports: cli (< 2.0.0)\n",
        );

        let Dependencies::Available(dependencies) =
            dependencies_from_description("example 1.0.0", &description, &BTreeSet::new())
        else {
            panic!("hard dependencies should be available");
        };
        let range = dependencies.get("cli").expect("cli should be a dependency");
        let candidate = |version: &str| {
            PackageVersion::new(
                Version::from_str(version).expect("valid test version"),
                built_in_repository(),
            )
        };

        assert!(!range.contains(&candidate("0.9.0")));
        assert!(range.contains(&candidate("1.5.0")));
        assert!(!range.contains(&candidate("2.0.0")));
    }

    #[tokio::test]
    async fn preferred_version_uses_the_repository_that_contains_it() {
        let first_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("first source"));
        let second_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("second source"));
        let first_latest = version("3.0.0", Arc::clone(&first_source));
        let second_latest = version("2.0.0", Arc::clone(&second_source));
        let preferred = Version::from_str("1.5.0").expect("valid preferred version");
        let preferred_candidate = PackageVersion::new(preferred.clone(), second_source);
        let first_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "first",
            packages: BTreeMap::from([("example".to_string(), first_latest.clone())]),
            versions: BTreeMap::from([("example".to_string(), BTreeSet::from([first_latest]))]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });
        let second_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "second",
            packages: BTreeMap::from([("example".to_string(), second_latest.clone())]),
            versions: BTreeMap::from([(
                "example".to_string(),
                BTreeSet::from([preferred_candidate.clone(), second_latest]),
            )]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });

        let selected = choose_package_version(
            &[first_repository, second_repository],
            &BTreeMap::from([("example".to_string(), preferred)]),
            "example",
            &Ranges::full(),
        )
        .await
        .expect("version selection should succeed")
        .expect("preferred version should be available");

        assert_eq!(selected.version(), preferred_candidate.version());
        assert_eq!(selected.repository().to_string(), "second source");
    }

    #[tokio::test]
    async fn missing_preferred_version_uses_the_normal_best_candidate() {
        let first_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("first source"));
        let second_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("second source"));
        let first_latest = version("2.0.0", first_source);
        let second_latest = version("3.0.0", second_source);
        let first_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "first",
            packages: BTreeMap::from([("example".to_string(), first_latest.clone())]),
            versions: BTreeMap::from([("example".to_string(), BTreeSet::from([first_latest]))]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });
        let second_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "second",
            packages: BTreeMap::from([("example".to_string(), second_latest.clone())]),
            versions: BTreeMap::from([(
                "example".to_string(),
                BTreeSet::from([second_latest.clone()]),
            )]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });

        let selected = choose_package_version(
            &[first_repository, second_repository],
            &BTreeMap::from([(
                "example".to_string(),
                Version::from_str("1.5.0").expect("valid preferred version"),
            )]),
            "example",
            &Ranges::full(),
        )
        .await
        .expect("version selection should succeed")
        .expect("fallback version should be available");

        assert_eq!(selected.version(), second_latest.version());
        assert_eq!(selected.repository().to_string(), "second source");
    }

    #[tokio::test]
    async fn equal_versions_use_the_earlier_repository() {
        let first_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("first source"));
        let second_source: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("second source"));
        let first_candidate = version("2.0.0", first_source);
        let second_candidate = version("2.0.0", second_source);
        let first_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "first",
            packages: BTreeMap::from([("example".to_string(), first_candidate.clone())]),
            versions: BTreeMap::from([("example".to_string(), BTreeSet::from([first_candidate]))]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });
        let second_repository: Arc<dyn PackageRepository> = Arc::new(TestRepository {
            name: "second",
            packages: BTreeMap::from([("example".to_string(), second_candidate.clone())]),
            versions: BTreeMap::from([("example".to_string(), BTreeSet::from([second_candidate]))]),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });

        let selected = choose_package_version(
            &[first_repository, second_repository],
            &BTreeMap::new(),
            "example",
            &Ranges::full(),
        )
        .await
        .expect("version selection should succeed")
        .expect("candidate should be available");

        assert_eq!(selected.repository().to_string(), "first source");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn root_name_is_reserved_for_the_supplied_version() {
        let local_repository = local_repository("project", "1.0.0");
        let candidate_repository: Arc<dyn PackageRepository> =
            Arc::new(TestRepository::empty("candidate"));
        let mut packages = BTreeMap::new();
        packages.insert(
            "project".to_string(),
            version("9.0.0", candidate_repository),
        );
        let remote_repository = Arc::new(TestRepository {
            name: "remote",
            packages,
            versions: BTreeMap::new(),
            descriptions: BTreeMap::new(),
            package_queries: AtomicUsize::new(0),
            version_queries: Mutex::new(Vec::new()),
            description_queries: Mutex::new(Vec::new()),
        });
        let (_, root_version) = local_repository.package().await.expect("root should load");
        let provider = RDependencyProvider::new(
            vec![remote_repository.clone()],
            local_repository,
            DependencyConstraints::default(),
            BTreeMap::new(),
            BTreeSet::new(),
        );
        let expected = root_version.clone();
        let incompatible =
            Ranges::higher_than(version("2.0.0", Arc::clone(root_version.repository())));
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                provider
                    .choose_version(&"project".to_string(), &Ranges::full())
                    .expect("root selection should succeed"),
                Some(expected)
            );
            assert_eq!(
                provider
                    .choose_version(&"project".to_string(), &incompatible)
                    .expect("root selection should succeed"),
                None
            );
        })
        .await
        .expect("root selection task should join");
        assert_eq!(remote_repository.package_queries.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn prefetch_does_not_query_the_root_package() {
        let local_repository = local_repository("project", "1.0.0");
        let remote_repository = Arc::new(TestRepository::empty("remote"));
        let provider = RDependencyProvider::new(
            vec![remote_repository.clone()],
            local_repository,
            DependencyConstraints::default(),
            BTreeMap::new(),
            BTreeSet::new(),
        );
        let constraints =
            DependencyConstraints::from_iter([("project".to_string(), Ranges::full())]);

        tokio::task::spawn_blocking(move || {
            provider
                .prefetch_descriptions(&constraints)
                .expect("prefetch should succeed");
        })
        .await
        .expect("prefetch task should join");

        assert_eq!(remote_repository.package_queries.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn root_dependencies_preserve_explicit_constraints() {
        let local_repository = local_repository("project", "1.0.0");
        let (_, root_version) = local_repository.package().await.expect("root should load");
        let suggested_version = Version::from_str("2.0.0").expect("valid test version");
        let suggested = Relation::new(
            "suggested",
            VersionRequirement::GreaterThanEqual(RequirementVersion::Version(suggested_version)),
        )
        .expect("valid suggested relation");
        let roots = BTreeSet::from([suggested]);
        let root_dependencies = dependency_ranges_from_relations(&roots, &BTreeSet::new());
        let provider = RDependencyProvider::new(
            Vec::new(),
            Arc::clone(&local_repository),
            root_dependencies,
            BTreeMap::new(),
            BTreeSet::new(),
        );

        let Dependencies::Available(constraints) = tokio::task::spawn_blocking(move || {
            provider
                .get_dependencies(&"project".to_string(), &root_version)
                .expect("root dependencies should be available")
        })
        .await
        .expect("root dependency task should join") else {
            panic!("root dependencies should be available");
        };
        let range = constraints
            .get("suggested")
            .expect("caller-supplied relation should be preserved");

        let local_repository: Arc<dyn PackageRepository> = local_repository;
        assert!(!range.contains(&version("1.9.9", Arc::clone(&local_repository))));
        assert!(range.contains(&version("2.0.0", local_repository)));
    }

    #[test]
    #[should_panic(expected = "R revision requirement reached the package version resolver")]
    fn revision_constraints_cannot_reach_pubgrub() {
        let relation = "example (>= r123)"
            .parse()
            .expect("revision relation should parse");
        package_version_range_from_relation(&relation);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolution_includes_the_actual_root_without_remote_queries() {
        let local_repository = local_repository("project", "1.0.0");
        let (_, root_version) = local_repository.package().await.expect("root should load");
        let remote_repository = Arc::new(TestRepository::empty("remote"));
        let selected = resolve_from_registry(
            vec![remote_repository.clone()],
            local_repository,
            BTreeSet::from([Relation::any("testBasePackage").expect("valid base relation")]),
            BTreeMap::new(),
        )
        .await
        .expect("root-only resolution should succeed");

        assert_eq!(
            selected,
            BTreeMap::from([("project".to_string(), root_version)])
        );
        assert_eq!(remote_repository.package_queries.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolution_with_no_dependencies_still_includes_the_root() {
        let local_repository = local_repository("project", "1.0.0");
        let (_, root_version) = local_repository.package().await.expect("root should load");
        let remote_repository = Arc::new(TestRepository::empty("remote"));

        let selected = resolve_from_registry(
            vec![remote_repository.clone()],
            local_repository,
            BTreeSet::new(),
            BTreeMap::new(),
        )
        .await
        .expect("dependency-free resolution should succeed");

        assert_eq!(
            selected,
            BTreeMap::from([("project".to_string(), root_version)])
        );
        assert_eq!(remote_repository.package_queries.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backtracks_instead_of_replacing_the_fixed_root() {
        let local_repository = local_repository("project", "1.0.1");
        let (_, root_version) = local_repository.package().await.expect("root should load");
        let mut metadata_repository = TestRepository::empty("remote metadata");
        metadata_repository.descriptions.insert(
            (
                "testthat".to_string(),
                Version::from_str("3.2.0").expect("valid test version"),
            ),
            Arc::new(Description::parse(
                "Package: testthat\nVersion: 3.2.0\nTitle: Testthat\nDescription: Test package.\nLicense: MIT\nDepends: project (>= 1.1.0)\n",
            )),
        );
        metadata_repository.descriptions.insert(
            (
                "testthat".to_string(),
                Version::from_str("3.0.0").expect("valid test version"),
            ),
            Arc::new(Description::parse(
                "Package: testthat\nVersion: 3.0.0\nTitle: Testthat\nDescription: Test package.\nLicense: MIT\nDepends: project (>= 1.0.0)\n",
            )),
        );
        let metadata_repository = Arc::new(metadata_repository);
        let metadata: Arc<dyn PackageRepository> = metadata_repository.clone();
        let testthat_3_2 = version("3.2.0", Arc::clone(&metadata));
        let testthat_3_0 = version("3.0.0", Arc::clone(&metadata));
        let mut remote_repository = TestRepository::empty("remote index");
        remote_repository
            .packages
            .insert("testthat".to_string(), testthat_3_2.clone());
        remote_repository.packages.insert(
            "project".to_string(),
            version("9.0.0", Arc::clone(&metadata)),
        );
        remote_repository.versions.insert(
            "testthat".to_string(),
            BTreeSet::from([testthat_3_0.clone(), testthat_3_2]),
        );
        remote_repository.versions.insert(
            "project".to_string(),
            BTreeSet::from([version("9.0.0", Arc::clone(&metadata))]),
        );
        let remote_repository = Arc::new(remote_repository);
        let selected = resolve_from_registry(
            vec![remote_repository.clone()],
            local_repository,
            BTreeSet::from([Relation::new(
                "testthat",
                VersionRequirement::GreaterThanEqual(RequirementVersion::Version(
                    Version::from_str("3.0.0").expect("valid test version"),
                )),
            )
            .expect("valid testthat relation")]),
            BTreeMap::new(),
        )
        .await
        .expect("resolution should backtrack to a root-compatible version");

        assert_eq!(
            selected,
            BTreeMap::from([
                ("project".to_string(), root_version),
                ("testthat".to_string(), testthat_3_0),
            ])
        );
        assert!(
            !remote_repository
                .version_queries
                .lock()
                .expect("version query lock should not be poisoned")
                .iter()
                .any(|package| package == "project")
        );
        assert!(
            !metadata_repository
                .description_queries
                .lock()
                .expect("description query lock should not be poisoned")
                .iter()
                .any(|package| package == "project")
        );
    }
}

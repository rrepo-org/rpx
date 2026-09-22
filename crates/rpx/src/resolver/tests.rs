use super::*;
use crate::description::required_dependencies;
use crate::repository::RrepoRepository;

struct Registry {
    server: mockito::ServerGuard,
    repository: PackageRepository,
    mocks: Vec<mockito::Mock>,
}

impl Registry {
    async fn new(packages: &[(&str, &str)]) -> Self {
        let mut server = mockito::Server::new_async().await;
        let repository = PackageRepository::Rrepo(Arc::new(RrepoRepository::new(
            server.url().parse().unwrap(),
        )));
        let index = serde_json::json!({"repositorySlug":"fixture", "packages": packages.iter().map(|(name, version)|
            serde_json::json!({"name":name,"latestVersion":version})).collect::<Vec<_>>()});
        let mock = server
            .mock("GET", "/packages")
            .with_status(200)
            .with_body(index.to_string())
            .expect_at_least(0)
            .create_async()
            .await;
        Self {
            server,
            repository,
            mocks: vec![mock],
        }
    }

    async fn versions(&mut self, package: &str, versions: &[&str]) {
        self.mocks.push(self.server.mock("GET", format!("/packages/{package}/versions").as_str()).with_status(200)
            .with_body(serde_json::json!({"package":package,"versions":versions.iter().map(|v|
                serde_json::json!({"version":v,"sourceUrl":format!("{}/{package}/{v}", self.server.url())})).collect::<Vec<_>>()} ).to_string())
            .expect_at_least(0).create_async().await);
    }

    async fn description(&mut self, package: &str, version: &str, fields: &str) {
        self.mocks.push(
            self.server
                .mock(
                    "GET",
                    format!("/packages/{package}/versions/{version}/description").as_str(),
                )
                .with_status(200)
                .with_body(format!("Package: {package}\nVersion: {version}\n{fields}"))
                .expect_at_least(0)
                .create_async()
                .await,
        );
    }
}

fn local_repository(package: &str, version: &str) -> Arc<LocalRepository> {
    Arc::new(
        LocalRepository::new("unused".into()).with_description(Description::parse(&format!(
            "Package: {package}\nVersion: {version}\n"
        ))),
    )
}

fn version(value: &str, repository: impl Into<PackageRepository>) -> PackageVersion {
    PackageVersion::new(value.parse().unwrap(), repository)
}

fn dependencies_from_description(
    description: &str,
) -> Result<
    DependencyConstraints<String, Ranges<PackageVersion>>,
    crate::description::DescriptionParseError,
> {
    required_dependencies("fixture", &Description::parse(description))
        .map(|relations| dependency_ranges_from_relations(&relations, &BTreeSet::new()))
}

#[test]
fn rejects_malformed_hard_dependency_metadata() {
    assert!(
        dependencies_from_description(
            "Package: example\nVersion: 1.0\nImports: cli (>= invalid)\n"
        )
        .is_err()
    );
}

#[test]
fn ignores_unconsumed_malformed_suggests_metadata() {
    let deps = dependencies_from_description(
        "Package: example\nVersion: 1.0\nImports: cli\nSuggests: invalid (>= invalid)\n",
    )
    .unwrap();
    assert!(deps.contains_key("cli"));
}

#[test]
fn intersects_transitive_constraints_across_dependency_fields() {
    let deps = dependencies_from_description(
        "Package: example\nVersion: 1.0\nDepends: cli (>= 1.0.0)\nImports: cli (< 2.0.0)\n",
    )
    .unwrap();
    let candidate = |v| version(v, built_in_repository());
    assert!(!deps["cli"].contains(&candidate("0.9.0")));
    assert!(deps["cli"].contains(&candidate("1.5.0")));
    assert!(!deps["cli"].contains(&candidate("2.0.0")));
}

#[tokio::test]
async fn preferred_version_uses_the_repository_that_contains_it() {
    let mut first = Registry::new(&[("example", "3.0.0")]).await;
    first.versions("example", &["3.0.0"]).await;
    let mut second = Registry::new(&[("example", "2.0.0")]).await;
    second.versions("example", &["1.5.0", "2.0.0"]).await;
    let selected = choose_package_version(
        &[first.repository.clone(), second.repository.clone()],
        &BTreeMap::from([("example".into(), "1.5.0".parse().unwrap())]),
        "example",
        &Ranges::full(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(selected.version().to_string(), "1.5.0");
    assert!(selected.repository().same_instance(&second.repository));
}

#[tokio::test]
async fn missing_preferred_version_uses_the_normal_best_candidate() {
    let mut first = Registry::new(&[("example", "2.0.0")]).await;
    first.versions("example", &["2.0.0"]).await;
    let mut second = Registry::new(&[("example", "3.0.0")]).await;
    second.versions("example", &["3.0.0"]).await;
    let selected = choose_package_version(
        &[first.repository.clone(), second.repository.clone()],
        &BTreeMap::from([("example".into(), "1.5.0".parse().unwrap())]),
        "example",
        &Ranges::full(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(selected.version().to_string(), "3.0.0");
    assert!(selected.repository().same_instance(&second.repository));
}

#[tokio::test]
async fn equal_versions_use_the_earlier_repository() {
    let first = Registry::new(&[("example", "2.0.0")]).await;
    let second = Registry::new(&[("example", "2.0.0")]).await;
    let selected = choose_package_version(
        &[first.repository.clone(), second.repository.clone()],
        &BTreeMap::new(),
        "example",
        &Ranges::full(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(selected.repository().same_instance(&first.repository));
}

#[tokio::test(flavor = "multi_thread")]
async fn backtracks_from_version_with_malformed_dependency_metadata() {
    let mut registry = Registry::new(&[("example", "2.0.0")]).await;
    registry.versions("example", &["1.0.0", "2.0.0"]).await;
    registry
        .description("example", "2.0.0", "Imports: cli (>= invalid)\n")
        .await;
    registry.description("example", "1.0.0", "").await;
    let selected = resolve_from_registry(
        vec![registry.repository.clone()],
        local_repository("project", "1.0.0"),
        ProjectType::Package,
        BTreeSet::from([Relation::any("example").unwrap()]),
        BTreeMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(selected["example"].version().to_string(), "1.0.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_namespace_is_unsatisfiable_even_when_it_is_a_base_package() {
    let provider = RDependencyProvider::new(
        vec![],
        local_repository("stats", "1.0.0"),
        ProjectType::Project,
        BTreeSet::from([Relation::any("stats").unwrap()]),
        BTreeMap::new(),
        BTreeSet::from(["stats".into()]),
    );
    let deps =
        tokio::task::spawn_blocking(move || provider.dependency_ranges(&provider.root_relations))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(deps["stats"], Ranges::empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn root_name_is_reserved_for_the_supplied_version() {
    let mut registry = Registry::new(&[("project", "9.0.0")]).await;
    let never = registry
        .server
        .mock("GET", "/packages")
        .expect(0)
        .create_async()
        .await;
    let root = local_repository("project", "1.0.0");
    let provider = RDependencyProvider::new(
        vec![registry.repository.clone()],
        root.clone(),
        ProjectType::Package,
        BTreeSet::new(),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            provider
                .choose_version(&"project".into(), &Ranges::full())
                .unwrap(),
            Some(version("1.0.0", root.clone()))
        );
        assert!(
            provider
                .choose_version(
                    &"project".into(),
                    &Ranges::higher_than(version("2.0.0", root))
                )
                .unwrap()
                .is_none()
        );
    })
    .await
    .unwrap();
    never.assert_async().await;
}

#[tokio::test]
async fn prefetch_does_not_query_the_root_package() {
    let mut registry = Registry::new(&[]).await;
    let never = registry
        .server
        .mock("GET", "/packages")
        .expect(0)
        .create_async()
        .await;
    let provider = RDependencyProvider::new(
        vec![registry.repository.clone()],
        local_repository("project", "1.0"),
        ProjectType::Package,
        BTreeSet::new(),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    tokio::task::spawn_blocking(move || {
        provider.prefetch_descriptions(&DependencyConstraints::from_iter([(
            "project".into(),
            Ranges::full(),
        )]))
    })
    .await
    .unwrap()
    .unwrap();
    never.assert_async().await;
}

#[tokio::test]
async fn root_dependencies_preserve_explicit_constraints() {
    let root = local_repository("project", "1.0.0");
    let root_version = version("1.0.0", root.clone());
    let provider = RDependencyProvider::new(
        vec![],
        root.clone(),
        ProjectType::Package,
        BTreeSet::from(["suggested (>= 2.0.0)".parse().unwrap()]),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    let Dependencies::Available(deps) = tokio::task::spawn_blocking(move || {
        provider.get_dependencies(&"project".into(), &root_version)
    })
    .await
    .unwrap()
    .unwrap() else {
        panic!("expected dependencies")
    };
    assert!(!deps["suggested"].contains(&version("1.9.9", root.clone())));
    assert!(deps["suggested"].contains(&version("2.0.0", root)));
}

#[test]
#[should_panic(expected = "R revision requirement reached the package version resolver")]
fn revision_constraints_cannot_reach_pubgrub() {
    package_version_range_from_relation(&"example (>= r123)".parse().unwrap());
}

async fn root_only(roots: BTreeSet<Relation>) {
    let mut registry = Registry::new(&[]).await;
    let never = registry
        .server
        .mock("GET", "/packages")
        .expect(0)
        .create_async()
        .await;
    let selected = resolve_from_registry(
        vec![registry.repository.clone()],
        local_repository("project", "1.0.0"),
        ProjectType::Package,
        roots,
        BTreeMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected["project"].version().to_string(), "1.0.0");
    never.assert_async().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_includes_the_actual_root_without_remote_queries() {
    root_only(BTreeSet::from([Relation::any("testBasePackage").unwrap()])).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_with_no_dependencies_still_includes_the_root() {
    root_only(BTreeSet::new()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn package_depending_on_project_namespace_is_unsatisfiable() {
    let mut registry = Registry::new(&[("dependent", "1.0.0"), ("project", "9.0.0")]).await;
    registry
        .description("dependent", "1.0.0", "Imports: project\n")
        .await;
    registry.versions("dependent", &["1.0.0"]).await;
    let never = registry
        .server
        .mock("GET", "/packages/project/versions")
        .expect(0)
        .create_async()
        .await;
    let result = resolve_from_registry(
        vec![registry.repository.clone()],
        local_repository("project", "1.0.0"),
        ProjectType::Project,
        BTreeSet::from([Relation::any("dependent").unwrap()]),
        BTreeMap::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(ResolutionError::PubGrub(PubGrubError::NoSolution(_)))
    ));
    never.assert_async().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn backtracks_instead_of_replacing_the_fixed_root() {
    let mut registry = Registry::new(&[("testthat", "3.2.0"), ("project", "9.0.0")]).await;
    registry.versions("testthat", &["3.2.0", "3.0.0"]).await;
    registry
        .description("testthat", "3.2.0", "Depends: project (>= 1.1.0)\n")
        .await;
    registry
        .description("testthat", "3.0.0", "Depends: project (>= 1.0.0)\n")
        .await;
    let never = registry
        .server
        .mock("GET", mockito::Matcher::Regex("^/packages/project/".into()))
        .expect(0)
        .create_async()
        .await;
    let selected = resolve_from_registry(
        vec![registry.repository.clone()],
        local_repository("project", "1.0.1"),
        ProjectType::Package,
        BTreeSet::from(["testthat (>= 3.0.0)".parse().unwrap()]),
        BTreeMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(selected["project"].version().to_string(), "1.0.1");
    assert_eq!(selected["testthat"].version().to_string(), "3.0.0");
    never.assert_async().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_retains_metadata_after_repository_cache_eviction() {
    let mut registry = Registry::new(&[("example", "1.0.0")]).await;
    let metadata = registry
        .server
        .mock("GET", "/packages/example/versions/1.0.0/description")
        .with_status(200)
        .with_body("Package: example\nVersion: 1.0.0\nDepends: R (>= 4.0), stats\n")
        .expect(1)
        .create_async()
        .await;
    let selected = resolve_from_registry(
        vec![registry.repository.clone()],
        local_repository("root", "1.0.0"),
        ProjectType::Package,
        BTreeSet::from([Relation::any("example").unwrap()]),
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let PackageRepository::Rrepo(repo) = &registry.repository else {
        unreachable!()
    };
    repo.invalidate_descriptions();
    let selected: BTreeMap<_, _> = selected
        .into_iter()
        .filter(|(name, _)| name != "root")
        .collect();
    let lock = crate::project::lockfile_from_resolution(
        BTreeSet::new(),
        &selected,
        &[registry.repository.clone()],
        &semver::Version::new(4, 5, 0),
    )
    .await
    .unwrap();
    assert_eq!(
        lock.packages["example"].dependencies,
        BTreeSet::from(["R (>= 4.0)".parse().unwrap(), "stats".parse().unwrap()])
    );
    metadata.assert_async().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn version_only_solver_equality_does_not_conflate_source_metadata() {
    let source = |dependency: &str| {
        Arc::new(
            LocalRepository::new("same-path".into()).with_description(Description::parse(
                &format!("Package: example\nVersion: 1.0.0\nImports: {dependency}\n"),
            )),
        )
    };
    let a = PackageRepository::Local(source("firstdep"));
    let b = PackageRepository::Local(source("seconddep"));
    assert_eq!(a, b); // configuration equality is deliberately not snapshot identity
    let first = version("1.0.0", a.clone());
    let second = version("1.0.0", b.clone());
    assert_eq!(first, second);
    let provider = RDependencyProvider::new(
        vec![a, b],
        local_repository("root", "1.0.0"),
        ProjectType::Package,
        BTreeSet::new(),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    tokio::task::spawn_blocking(move || {
        provider
            .get_dependencies(&"example".into(), &first)
            .unwrap();
        provider
            .get_dependencies(&"example".into(), &second)
            .unwrap();
        assert_eq!(
            provider
                .resolved_package("example", first)
                .unwrap()
                .dependencies,
            BTreeSet::from([Relation::any("firstdep").unwrap()])
        );
        assert_eq!(
            provider
                .resolved_package("example", second)
                .unwrap()
                .dependencies,
            BTreeSet::from([Relation::any("seconddep").unwrap()])
        );
    })
    .await
    .unwrap();
}

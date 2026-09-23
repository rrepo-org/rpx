//! Reconcile package snapshots, assemble the workflow, and report package progress.
//! Graph IDs, resources, and operation metadata stay inside this module.

use super::operations::{self, BuildInput, DependencyInput, OperationError, PreparedArtifact};
use crate::{
    cache::installer_cache_path,
    project::RequiredPackages,
    r::{InstalledPackagesError, installed_packages},
    repository::PackageRepository,
    resolver::PackageVersion,
};
use miette::Diagnostic;
use r_metadata::Version;
use r_package_installer::Installer;
use rpx_task::{
    BuildError, ExecutableGraph, ExecutionError, ExecutionEvent, GraphBuilder, Inputs, NodeId,
    ResourceId, TaskRef,
};
use std::{collections::BTreeMap, future::Future, path::PathBuf, sync::Arc};
use thiserror::Error;
use tracing::Instrument;

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_CHECKOUT_WORKERS: usize = 1;
const SYNC_R_WORKERS: usize = 8;

/// A library observation bound to the exact target used by execution. The
/// installed snapshot is version-only: scanned repositories are not provenance.
pub(super) struct SyncTarget {
    library: PathBuf,
    r_version: semver::Version,
    installed: BTreeMap<String, Version>,
}

impl SyncTarget {
    pub async fn inspect(
        library: PathBuf,
        r_version: semver::Version,
    ) -> Result<Self, InstalledPackagesError> {
        let installed = installed_packages(&library)
            .await?
            .into_iter()
            .map(|(name, package)| (name, package.version().clone()))
            .collect();
        Ok(Self {
            library,
            r_version,
            installed,
        })
    }
}

/// A validated package change, independent of graph handles and runtime services.
struct InstallRequest {
    selected: PackageVersion,
    dependencies: BTreeMap<String, Option<Version>>,
}

struct Changes {
    installs: BTreeMap<String, InstallRequest>,
    removals: Vec<String>,
}

/// Pure reconciliation of already-validated dependency records. All dependency
/// versions come from this resolution (or explicitly replayed lockfile) snapshot.
fn reconcile(required: RequiredPackages, installed: &BTreeMap<String, Version>) -> Changes {
    let removals = installed
        .keys()
        .filter(|name| !required.contains_key(*name))
        .cloned()
        .collect();
    let versions: BTreeMap<_, _> = required
        .iter()
        .map(|(name, package)| (name.clone(), package.version().clone()))
        .collect();
    let installs = required
        .into_iter()
        .filter(|(name, package)| package_requires_install(&package.selected, installed.get(name)))
        .map(|(name, package)| {
            let dependencies = package
                .dependencies
                .into_iter()
                .filter(|relation| relation.package() != "R")
                .map(|relation| {
                    let name = relation.package().to_string();
                    let version = versions.get(&name).cloned();
                    (name, version)
                })
                .collect();
            (
                name,
                InstallRequest {
                    selected: package.selected,
                    dependencies,
                },
            )
        })
        .collect();
    Changes { installs, removals }
}

fn package_requires_install(required: &PackageVersion, installed: Option<&Version>) -> bool {
    let repository = required.repository();
    // Preserve the existing policy for sources that can change without a version bump.
    matches!(
        repository,
        PackageRepository::Git(_) | PackageRepository::Local(_)
    ) || installed != Some(required.version())
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum PlanError {
    #[error("cannot determine package installation order")]
    #[diagnostic(
        code(rpx::sync::dependency_cycle),
        help("Update the package requirements to break the dependency cycle before syncing.")
    )]
    DependencyCycle {
        #[related]
        packages: Vec<CycleBlockedPackage>,
    },
    #[error("failed to construct sync task graph: {0}")]
    #[diagnostic(code(rpx::sync::task_graph_invalid))]
    Graph(#[from] BuildError),
}

#[derive(Debug, Error, Diagnostic)]
#[error("package `{package}` is blocked by a dependency cycle")]
pub(crate) struct CycleBlockedPackage {
    package: String,
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum RunError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Operation(#[from] OperationError),
    #[error("failed to join {operation} task for package {package}: {source}")]
    #[diagnostic(code(rpx::sync::task_join_failed))]
    TaskJoin {
        package: String,
        operation: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("sync task engine invariant failed: {message}")]
    #[diagnostic(code(rpx::sync::task_engine_invariant))]
    TaskInvariant { message: &'static str },
}

/// Domain progress, without graph IDs or operation-kind interpretation by callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SyncProgress {
    pub running_operations: usize,
    pub installed_packages: usize,
}

pub(super) struct SyncPlan {
    graph: ExecutableGraph<OperationError>,
    tasks: BTreeMap<NodeId, TaskMetadata>,
    install_count: usize,
}

impl SyncPlan {
    /// Call under the desired tracing span; registration binds it to each operation.
    pub fn prepare(required: RequiredPackages, target: SyncTarget) -> Result<Self, PlanError> {
        let changes = reconcile(required, &target.installed);
        let assembly = Assembly::new(target.library, target.r_version)
            .reserve_installs(changes.installs.keys());
        let assembly = changes
            .installs
            .into_iter()
            .try_fold(assembly, |assembly, (name, request)| {
                assembly.install(name, request)
            })?;
        changes
            .removals
            .into_iter()
            .try_fold(assembly, Assembly::remove)?
            .finish()
    }

    pub fn install_count(&self) -> usize {
        self.install_count
    }

    pub async fn run(self, mut report: impl FnMut(SyncProgress)) -> Result<(), RunError> {
        let mut progress = SyncProgress::default();
        self.graph
            .execute(|event| {
                match event {
                    ExecutionEvent::Started(_) => progress.running_operations += 1,
                    ExecutionEvent::Succeeded(node) => {
                        progress.running_operations -= 1;
                        if self.tasks[&node].kind == TaskKind::Install {
                            progress.installed_packages += 1;
                        }
                    }
                    ExecutionEvent::Failed(_) => progress.running_operations -= 1,
                }
                report(progress);
            })
            .await
            .map_err(|error| execution_error(error, &self.tasks))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    Remove,
    Download,
    Checkout,
    Build,
    Install,
}

impl TaskKind {
    fn name(self) -> &'static str {
        match self {
            Self::Remove => "remove",
            Self::Download => "download",
            Self::Checkout => "checkout",
            Self::Build => "build",
            Self::Install => "install",
        }
    }
}

struct TaskMetadata {
    package: String,
    kind: TaskKind,
}

fn execution_error(
    error: ExecutionError<OperationError>,
    tasks: &BTreeMap<NodeId, TaskMetadata>,
) -> RunError {
    match error {
        ExecutionError::Operation { source, .. } => source.into(),
        ExecutionError::Join { node, source } => {
            let task = &tasks[&node];
            RunError::TaskJoin {
                package: task.package.clone(),
                operation: task.kind.name(),
                source,
            }
        }
        ExecutionError::Invariant(message) => RunError::TaskInvariant { message },
    }
}

/// Private accumulator for graph construction. Registration owns metadata,
/// resource assignment, and tracing so these cannot drift apart at call sites.
struct Assembly {
    graph: GraphBuilder<OperationError>,
    tasks: BTreeMap<NodeId, TaskMetadata>,
    installs: BTreeMap<String, TaskRef<()>>,
    shared: ResourceId,
    checkout_slot: ResourceId,
    r: ResourceId,
    installer: Installer,
    library: PathBuf,
    r_version: Arc<semver::Version>,
}

impl Assembly {
    fn new(library: PathBuf, r_version: semver::Version) -> Self {
        let mut graph = GraphBuilder::new();
        let shared = graph.resource(SYNC_SHARED_WORKERS);
        let checkout_slot = graph.resource(SYNC_CHECKOUT_WORKERS);
        let r = graph.resource(SYNC_R_WORKERS);
        Self {
            graph,
            shared,
            checkout_slot,
            r,
            tasks: BTreeMap::new(),
            installs: BTreeMap::new(),
            installer: Installer::new(installer_cache_path()),
            library,
            r_version: Arc::new(r_version),
        }
    }

    fn reserve_installs<'a>(self, names: impl Iterator<Item = &'a String>) -> Self {
        names.fold(self, |mut assembly, name| {
            let task = assembly.graph.reserve();
            assembly.installs.insert(name.clone(), task);
            assembly
        })
    }

    fn requirements(&self, kind: TaskKind) -> Vec<(ResourceId, usize)> {
        match kind {
            TaskKind::Checkout => vec![(self.shared, 1), (self.checkout_slot, 1)],
            TaskKind::Build | TaskKind::Install => vec![(self.shared, 1), (self.r, 1)],
            TaskKind::Download | TaskKind::Remove => vec![(self.shared, 1)],
        }
    }

    fn register<I, F, Fut, T>(
        &mut self,
        package: &str,
        kind: TaskKind,
        inputs: I,
        operation: F,
    ) -> Result<TaskRef<T>, PlanError>
    where
        I: Inputs,
        F: FnOnce(I::Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, OperationError>> + Send + 'static,
        T: Send + Sync + 'static,
    {
        let task = self.graph.reserve();
        self.define(&task, package, kind, inputs, operation)?;
        Ok(task)
    }

    fn define<I, F, Fut, T>(
        &mut self,
        task: &TaskRef<T>,
        package: &str,
        kind: TaskKind,
        inputs: I,
        operation: F,
    ) -> Result<(), PlanError>
    where
        I: Inputs,
        F: FnOnce(I::Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, OperationError>> + Send + 'static,
        T: Send + Sync + 'static,
    {
        let span = tracing::Span::current();
        self.graph
            .define(task, inputs, self.requirements(kind), move |inputs| {
                operation(inputs).instrument(span)
            })?;
        self.tasks.insert(
            task.id(),
            TaskMetadata {
                package: package.into(),
                kind,
            },
        );
        Ok(())
    }

    fn prepare_artifact(
        &mut self,
        package: &str,
        selected: &PackageVersion,
    ) -> Result<TaskRef<PreparedArtifact>, PlanError> {
        let name = package.to_string();
        let version = selected.version().clone();
        match selected.repository() {
            PackageRepository::Local(local) => {
                let root = local.path().to_path_buf();
                self.register(package, TaskKind::Build, (), move |()| async move {
                    let input = Arc::new(BuildInput::local(root, name.clone(), version));
                    operations::build(input)
                        .await
                        .map_err(|source| OperationError::Build {
                            package: name,
                            source: Box::new(source),
                        })
                })
            }
            PackageRepository::Git(git) => {
                let git = git.as_ref().clone();
                let source =
                    self.register(package, TaskKind::Checkout, (), move |()| async move {
                        let version_string = version.to_string();
                        operations::checkout(git, name.clone(), version)
                            .await
                            .map_err(|source| OperationError::Checkout {
                                package: name,
                                version: version_string,
                                source,
                            })
                    })?;
                let name = package.to_string();
                self.register(package, TaskKind::Build, source, move |input| async move {
                    operations::build_checkout(input).await.map_err(|source| {
                        OperationError::Build {
                            package: name,
                            source: Box::new(source),
                        }
                    })
                })
            }
            PackageRepository::Cran(_) | PackageRepository::Rrepo(_) => {
                let selected = selected.clone();
                let r_version = self.r_version.clone();
                self.register(package, TaskKind::Download, (), move |()| async move {
                    operations::download_package_artifact(name.clone(), selected, r_version)
                        .await
                        .map_err(|source| OperationError::Download {
                            package: name,
                            version: version.to_string(),
                            source,
                        })
                })
            }
        }
    }

    fn install(mut self, package: String, request: InstallRequest) -> Result<Self, PlanError> {
        let artifact = self.prepare_artifact(&package, &request.selected)?;
        let prerequisites: Vec<_> = request
            .dependencies
            .keys()
            .filter_map(|name| self.installs.get(name).cloned())
            .collect();
        let dependencies: Vec<_> = request
            .dependencies
            .into_iter()
            .map(|(name, version)| DependencyInput { name, version })
            .collect();
        let version = request.selected.version().clone();
        let installer = self.installer.clone();
        let library = self.library.clone();
        let r_version = self.r_version.clone();
        let install = self.installs[&package].clone();
        let name = package.clone();
        self.define(
            &install,
            &package,
            TaskKind::Install,
            (artifact, prerequisites),
            move |(artifact, _completed_dependencies)| async move {
                operations::install_package(
                    &installer,
                    &library,
                    &name,
                    &version,
                    &r_version,
                    &dependencies,
                    artifact,
                )
                .await
                .map_err(|source| OperationError::Install {
                    package: name,
                    source: Box::new(source),
                })
            },
        )?;
        Ok(self)
    }

    fn remove(mut self, package: String) -> Result<Self, PlanError> {
        let name = package.clone();
        let installer = self.installer.clone();
        let library = self.library.clone();
        self.register(&package, TaskKind::Remove, (), move |()| async move {
            operations::remove_package(installer, library, name.clone())
                .await
                .map_err(|source| OperationError::Remove {
                    package: name,
                    source,
                })
        })?;
        Ok(self)
    }

    fn finish(self) -> Result<SyncPlan, PlanError> {
        let graph = self.graph.finish().map_err(|error| match error {
            BuildError::Cycle { blocked } => PlanError::DependencyCycle {
                packages: blocked
                    .into_iter()
                    .filter_map(|node| {
                        let task = &self.tasks[&node];
                        (task.kind == TaskKind::Install).then(|| CycleBlockedPackage {
                            package: task.package.clone(),
                        })
                    })
                    .collect(),
            },
            other => PlanError::Graph(other),
        })?;
        Ok(SyncPlan {
            graph,
            tasks: self.tasks,
            install_count: self.installs.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{GitRepository, LocalRepository, built_in_repository};
    use crate::resolver::ResolvedPackage;
    use r_description::Description;
    use r_metadata::Remote;
    use std::error::Error;

    fn required_packages(packages: &[(&str, &str)]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, fields)| {
                (
                    (*name).into(),
                    ResolvedPackage::from_description(
                        name,
                        PackageVersion::new("1.0.0".parse().unwrap(), built_in_repository()),
                        &Description::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}")),
                    )
                    .unwrap(),
                )
            })
            .collect()
    }

    fn target(installed: &[(&str, &str)]) -> SyncTarget {
        SyncTarget {
            library: PathBuf::from("unused-library"),
            r_version: semver::Version::new(4, 5, 0),
            installed: installed
                .iter()
                .map(|(name, version)| ((*name).into(), version.parse().unwrap()))
                .collect(),
        }
    }

    #[test]
    fn reconciliation_separates_kept_changed_missing_and_extra_packages() {
        let desired = required_packages(&[("kept", ""), ("changed", ""), ("missing", "")]);
        let observed = target(&[("kept", "1.0.0"), ("changed", "0.9.0"), ("extra", "1.0.0")]);
        let changes = reconcile(desired, &observed.installed);
        assert_eq!(
            changes
                .installs
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["changed", "missing"]
        );
        assert_eq!(changes.removals, ["extra"]);
        assert!(
            changes
                .removals
                .iter()
                .all(|name| !changes.installs.contains_key(name))
        );
    }

    #[test]
    fn dependency_snapshot_includes_kept_and_runtime_dependencies() {
        let desired = required_packages(&[
            (
                "consumer",
                "Imports: kept, newdep, stats\nDepends: R (>= 4.0)\n",
            ),
            ("kept", ""),
            ("newdep", ""),
        ]);
        let observed = target(&[("kept", "1.0.0")]);
        let changes = reconcile(desired, &observed.installed);
        let dependencies = &changes.installs["consumer"].dependencies;
        assert_eq!(
            dependencies.get("kept"),
            Some(&Some("1.0.0".parse().unwrap()))
        );
        assert_eq!(
            dependencies.get("newdep"),
            Some(&Some("1.0.0".parse().unwrap()))
        );
        assert_eq!(dependencies.get("stats"), Some(&None));
        assert!(!dependencies.contains_key("R"));
        assert!(!changes.installs.contains_key("kept"));
        assert!(!changes.installs.contains_key("stats"));
    }

    #[test]
    fn source_and_version_install_policy_is_preserved() {
        let version: Version = "1.0.0".parse().unwrap();
        let registry = PackageVersion::new(version.clone(), built_in_repository());
        assert!(!package_requires_install(&registry, Some(&version)));
        assert!(package_requires_install(&registry, None));
        assert!(package_requires_install(
            &registry,
            Some(&"0.9.0".parse().unwrap())
        ));
        let local = Arc::new(LocalRepository::new(PathBuf::from("vendor/selected")));
        let git = Arc::new(
            GitRepository::new("github::owner/repository".parse::<Remote>().unwrap()).unwrap(),
        );
        assert!(package_requires_install(
            &PackageVersion::new(version.clone(), local),
            Some(&version)
        ));
        assert!(package_requires_install(
            &PackageVersion::new(version.clone(), git),
            Some(&version)
        ));
    }

    #[test]
    fn cycles_are_rejected_before_execution_and_report_blocked_packages() {
        let desired = required_packages(&[
            ("a", "Imports: b\n"),
            ("b", "Imports: a\n"),
            ("blocked", "Imports: b\n"),
        ]);
        let Err(error @ PlanError::DependencyCycle { .. }) =
            SyncPlan::prepare(desired, target(&[]))
        else {
            panic!("expected cycle error")
        };
        assert_eq!(
            error.code().unwrap().to_string(),
            "rpx::sync::dependency_cycle"
        );
        let PlanError::DependencyCycle { packages } = error else {
            unreachable!()
        };
        assert_eq!(
            packages
                .iter()
                .map(|package| package.package.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "blocked"]
        );
    }

    #[test]
    fn extra_packages_get_removal_tasks_without_installations() {
        let plan = SyncPlan::prepare(
            required_packages(&[("kept", "")]),
            target(&[("kept", "1.0.0"), ("extra", "2.0.0")]),
        )
        .unwrap();
        assert_eq!(plan.install_count(), 0);
        assert_eq!(plan.tasks.len(), 1);
        assert!(
            plan.tasks
                .values()
                .all(|task| task.package == "extra" && task.kind == TaskKind::Remove)
        );
    }

    #[test]
    fn malformed_metadata_cannot_become_a_resolved_record() {
        let error = ResolvedPackage::from_description(
            "example",
            PackageVersion::new("1.0.0".parse().unwrap(), built_in_repository()),
            &Description::parse("Package: example\nVersion: 1.0.0\nImports: cli (>= invalid)\n"),
        )
        .unwrap_err();
        assert!(!error.messages().is_empty());
        let outer = super::super::SyncError::from(error);
        assert_eq!(
            outer.code().unwrap().to_string(),
            "rpx::description::parse_failed"
        );
        assert!(outer.source_code().is_some());
        assert!(outer.related().unwrap().next().is_some());
    }

    #[test]
    fn undefined_installation_keeps_the_task_runner_error() {
        let names = ["undefined".to_string()];
        let assembly = Assembly::new(PathBuf::from("unused"), semver::Version::new(4, 5, 0))
            .reserve_installs(names.iter());
        let id = assembly.installs["undefined"].id();
        let Err(error) = assembly.finish() else {
            panic!("undefined task should fail")
        };
        assert!(
            matches!(error.source().unwrap().downcast_ref::<BuildError>(), Some(BuildError::Undefined(node)) if *node == id)
        );
    }

    #[tokio::test]
    async fn run_reports_install_completions_without_exposing_graph_events() {
        let names = ["example".to_string()];
        let mut assembly = Assembly::new(PathBuf::from("unused"), semver::Version::new(4, 5, 0))
            .reserve_installs(names.iter());
        let artifact = assembly
            .register("example", TaskKind::Download, (), |()| async { Ok(42_u32) })
            .unwrap();
        let install = assembly.installs["example"].clone();
        assembly
            .define(
                &install,
                "example",
                TaskKind::Install,
                artifact,
                |value| async move {
                    assert_eq!(*value, 42);
                    Ok(())
                },
            )
            .unwrap();
        assembly
            .register("extra", TaskKind::Remove, (), |()| async { Ok(()) })
            .unwrap();
        let plan = assembly.finish().unwrap();
        assert_eq!(plan.install_count(), 1);
        let mut updates = Vec::new();
        plan.run(|progress| updates.push(progress)).await.unwrap();
        assert_eq!(
            updates.last(),
            Some(&SyncProgress {
                running_operations: 0,
                installed_packages: 1
            })
        );
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].installed_packages <= pair[1].installed_packages)
        );
    }

    #[tokio::test]
    async fn run_keeps_operation_context_diagnostic_and_io_cause() {
        use operations::InstallPackageError;
        let names = ["example".to_string()];
        let mut assembly = Assembly::new(PathBuf::from("unused"), semver::Version::new(4, 5, 0))
            .reserve_installs(names.iter());
        let install = assembly.installs["example"].clone();
        assembly
            .define(&install, "example", TaskKind::Install, (), |()| async {
                Err::<(), _>(OperationError::Install {
                    package: "example".into(),
                    source: Box::new(InstallPackageError::ArtifactDigest {
                        path: PathBuf::from("missing.tar.gz"),
                        source: std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "missing artifact",
                        ),
                    }),
                })
            })
            .unwrap();
        let error = super::super::SyncError::from(
            assembly.finish().unwrap().run(|_| {}).await.unwrap_err(),
        );
        assert!(
            error
                .to_string()
                .contains("failed to install package example")
        );
        assert_eq!(
            error.code().unwrap().to_string(),
            "rpx::sync::package_install_failed"
        );
        let install = error
            .source()
            .unwrap()
            .downcast_ref::<Box<InstallPackageError>>()
            .unwrap();
        assert_eq!(
            install
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn run_attributes_panics_without_caller_access_to_task_metadata() {
        let mut assembly = Assembly::new(PathBuf::from("unused"), semver::Version::new(4, 5, 0));
        assembly
            .register("example", TaskKind::Checkout, (), |()| async {
                panic!("operation panicked");
                #[allow(unreachable_code)]
                Ok(())
            })
            .unwrap();
        let mut last = SyncProgress::default();
        let error = assembly
            .finish()
            .unwrap()
            .run(|progress| last = progress)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("checkout task for package example")
        );
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
        assert_eq!(last, SyncProgress::default());
    }

    #[test]
    fn download_diagnostic_remains_visible_at_the_sync_boundary() {
        use operations::DownloadPackageArtifactError;
        let error = super::super::SyncError::from(RunError::from(OperationError::Download {
            package: "example".into(),
            version: "1.2.3".into(),
            source: DownloadPackageArtifactError::UnsupportedRepository,
        }));
        assert!(error.to_string().contains("example 1.2.3"));
        assert_eq!(
            error.code().unwrap().to_string(),
            "rpx::sync::package_artifact_download_failed"
        );
        assert!(error.source().unwrap().is::<DownloadPackageArtifactError>());
    }

    #[tokio::test]
    async fn target_observation_and_removal_use_the_same_library() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("library");
        std::fs::create_dir_all(library.join("extra/Meta")).unwrap();
        std::fs::write(
            library.join("extra/DESCRIPTION"),
            "Package: extra\nVersion: 1.0.0\nBuilt: R 4.5.0; x86_64-pc-linux-gnu; 2026-01-01; unix\n",
        )
        .unwrap();
        std::fs::write(library.join("extra/Meta/package.rds"), "metadata").unwrap();
        let target = SyncTarget::inspect(library.clone(), semver::Version::new(4, 5, 0))
            .await
            .unwrap();
        assert_eq!(
            target.installed.get("extra"),
            Some(&"1.0.0".parse().unwrap())
        );
        let plan = SyncPlan::prepare(BTreeMap::new(), target).unwrap();
        let mut final_progress = SyncProgress::default();
        plan.run(|progress| final_progress = progress)
            .await
            .unwrap();
        assert!(!library.join("extra").exists());
        assert_eq!(final_progress, SyncProgress::default());
    }
}

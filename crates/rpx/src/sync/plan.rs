//! Select package operations, connect their inputs, and assign resources.

use super::{
    SyncTaskContext,
    operations::{self, BuildInput, DependencyInput, OperationError},
};
use crate::{
    description::{DescriptionParseError, required_dependencies},
    project::RequiredPackages,
    repository::{GitRepository, LocalRepository},
    resolver::PackageVersion,
};
use miette::Diagnostic;
use rpx_task::{BuildError, ExecutableGraph, GraphBuilder, NodeId, TaskRef};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};
use thiserror::Error;
use tracing::Instrument;

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_CHECKOUT_WORKERS: usize = 1;
const SYNC_R_WORKERS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskKind {
    Remove,
    Download,
    Checkout,
    Build,
    Install,
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Remove => "remove",
            Self::Download => "download",
            Self::Checkout => "checkout",
            Self::Build => "build",
            Self::Install => "install",
        })
    }
}

pub(super) type TaskId = (String, TaskKind);

pub(super) struct SyncPlan {
    pub graph: ExecutableGraph<OperationError>,
    pub tasks: BTreeMap<NodeId, TaskId>,
    pub install_count: usize,
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum PlanError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Dependencies(#[from] DescriptionParseError),
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

fn package_requires_install(required: &PackageVersion, installed: Option<&PackageVersion>) -> bool {
    let repository = required.repository().as_ref();
    // Git and local sources can change without changing their package version.
    repository.downcast_ref::<GitRepository>().is_some()
        || repository.downcast_ref::<LocalRepository>().is_some()
        || installed != Some(required)
}

pub(super) fn sync_plan(
    required: &RequiredPackages,
    installed: &BTreeMap<String, PackageVersion>,
    context: SyncTaskContext,
) -> Result<SyncPlan, PlanError> {
    let mut graph = GraphBuilder::<OperationError>::new();
    let shared = graph.resource(SYNC_SHARED_WORKERS);
    let checkout_slot = graph.resource(SYNC_CHECKOUT_WORKERS);
    let r = graph.resource(SYNC_R_WORKERS);
    let mut tasks = BTreeMap::new();
    let installs: BTreeMap<String, TaskRef<()>> = required
        .iter()
        .filter(|(name, (version, _))| package_requires_install(version, installed.get(*name)))
        .map(|(name, _)| {
            let task = graph.reserve();
            tasks.insert(task.id(), (name.clone(), TaskKind::Install));
            (name.clone(), task)
        })
        .collect();

    for (package, install) in &installs {
        let (selected, description) = &required[package];
        let dependency_names: BTreeSet<_> =
            required_dependencies(format!("{package} {}", selected.version()), description)?
                .into_iter()
                .map(|relation| relation.package().to_string())
                .collect();
        let dependencies: Vec<_> = dependency_names
            .iter()
            .map(|name| DependencyInput {
                name: name.clone(),
                version: required.get(name).map(|(v, _)| v.version().to_string()),
            })
            .collect();
        let prerequisites: Vec<_> = dependency_names
            .iter()
            .filter_map(|name| installs.get(name).cloned())
            .collect();
        let repository = selected.repository().as_ref();
        let name = package.clone();
        let span = context.span.clone();
        let artifact = if let Some(local) = repository.downcast_ref::<LocalRepository>() {
            let input = Arc::new(BuildInput::local(
                local.path().to_path_buf(),
                name.clone(),
                selected.version().clone(),
            ));
            let task = graph.task((), vec![(shared, 1), (r, 1)], move |()| {
                async move {
                    operations::build(input)
                        .await
                        .map_err(|source| OperationError::Build {
                            package: name,
                            source: Box::new(source),
                        })
                }
                .instrument(span)
            })?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Build));
            task
        } else if let Some(git) = repository.downcast_ref::<GitRepository>() {
            let git = git.clone();
            let version = selected.version().clone();
            let source = graph.task((), vec![(shared, 1), (checkout_slot, 1)], move |()| {
                async move {
                    let version_string = version.to_string();
                    operations::checkout(git, name.clone(), version)
                        .await
                        .map_err(|source| OperationError::Checkout {
                            package: name,
                            version: version_string,
                            source,
                        })
                }
                .instrument(span)
            })?;
            tasks.insert(source.id(), (package.clone(), TaskKind::Checkout));
            let name = package.clone();
            let span = context.span.clone();
            let task = graph.task(source, vec![(shared, 1), (r, 1)], move |input| {
                async move {
                    operations::build(input)
                        .await
                        .map_err(|source| OperationError::Build {
                            package: name,
                            source: Box::new(source),
                        })
                }
                .instrument(span)
            })?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Build));
            task
        } else {
            let selected = selected.clone();
            let r_version = context.r_version.clone();
            let task = graph.task((), vec![(shared, 1)], move |()| {
                async move {
                    let version = selected.version().to_string();
                    operations::download_package_artifact(name.clone(), selected, r_version)
                        .await
                        .map_err(|source| OperationError::Download {
                            package: name,
                            version,
                            source,
                        })
                }
                .instrument(span)
            })?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Download));
            task
        };
        let name = package.clone();
        let version = selected.version().clone();
        let context = context.clone();
        let span = context.span.clone();
        graph.define(
            install,
            (artifact, prerequisites),
            vec![(shared, 1), (r, 1)],
            move |(artifact, _completed_dependencies)| {
                async move {
                    operations::install_package(
                        &context.installer,
                        &context.project_library,
                        &name,
                        &version,
                        context.r_version.as_ref(),
                        &dependencies,
                        artifact,
                    )
                    .await
                    .map_err(|source| OperationError::Install {
                        package: name,
                        source: Box::new(source),
                    })
                }
                .instrument(span)
            },
        )?;
    }
    for name in installed
        .keys()
        .filter(|name| !required.contains_key(*name))
    {
        let package = name.clone();
        let installer = context.installer.clone();
        let library = context.project_library.clone();
        let span = context.span.clone();
        let task = graph.task((), vec![(shared, 1)], move |()| {
            async move {
                operations::remove_package(installer, library, package.clone())
                    .await
                    .map_err(|source| OperationError::Remove { package, source })
            }
            .instrument(span)
        })?;
        tasks.insert(task.id(), (name.clone(), TaskKind::Remove));
    }
    let graph = graph.finish().map_err(|error| match error {
        BuildError::Cycle { blocked } => PlanError::DependencyCycle {
            packages: blocked
                .into_iter()
                .filter_map(|node| {
                    let (package, kind) = &tasks[&node];
                    (*kind == TaskKind::Install).then(|| CycleBlockedPackage {
                        package: package.clone(),
                    })
                })
                .collect(),
        },
        other => PlanError::Graph(other),
    })?;
    Ok(SyncPlan {
        graph,
        tasks,
        install_count: installs.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{PackageRepository, built_in_repository};
    use r_description::Description;
    use r_metadata::Remote;
    use r_package_installer::Installer;
    use std::path::PathBuf;

    #[test]
    fn package_requires_install_respects_source_and_version() {
        let version = |value: &str| value.parse().expect("version fixture should parse");
        let registry = PackageVersion::new(version("1.0.0"), built_in_repository());
        let same = PackageVersion::new(version("1.0.0"), built_in_repository());
        let old = PackageVersion::new(version("0.9.0"), built_in_repository());
        assert!(package_requires_install(&registry, None));
        assert!(!package_requires_install(&registry, Some(&same)));
        assert!(package_requires_install(&registry, Some(&old)));
        let local: Arc<dyn PackageRepository> =
            Arc::new(LocalRepository::new(PathBuf::from("vendor/selected")));
        let git: Arc<dyn PackageRepository> = Arc::new(
            GitRepository::new("github::owner/repository".parse::<Remote>().unwrap()).unwrap(),
        );
        assert!(package_requires_install(
            &PackageVersion::new(version("1.0.0"), local),
            Some(&same)
        ));
        assert!(package_requires_install(
            &PackageVersion::new(version("1.0.0"), git),
            Some(&same)
        ));
    }

    fn required_packages(packages: &[(&str, &str)]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, fields)| {
                let description =
                    Description::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}"));
                (
                    (*name).to_string(),
                    (
                        PackageVersion::new("1.0.0".parse().unwrap(), built_in_repository()),
                        Arc::new(description),
                    ),
                )
            })
            .collect()
    }

    fn test_context() -> SyncTaskContext {
        SyncTaskContext {
            installer: Installer::new(PathBuf::from("unused-cache")),
            project_library: PathBuf::from("unused-library"),
            r_version: Arc::new(semver::Version::new(4, 5, 0)),
            span: tracing::Span::none(),
        }
    }

    #[test]
    fn sync_plan_rejects_cycles_before_executing_operations() {
        let packages = required_packages(&[
            ("a", "Imports: b\n"),
            ("b", "Imports: a\n"),
            ("blocked", "Imports: b\n"),
        ]);
        let Err(PlanError::DependencyCycle { packages }) =
            sync_plan(&packages, &BTreeMap::new(), test_context())
        else {
            panic!("expected cycle error")
        };
        assert_eq!(
            packages
                .iter()
                .map(|p| p.package.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "blocked"]
        );
    }

    #[test]
    fn sync_tasks_schedule_extra_packages_for_removal() {
        let required = required_packages(&[("required", "")]);
        let installed = BTreeMap::from([
            (
                "required".to_string(),
                PackageVersion::new("1.0.0".parse().unwrap(), built_in_repository()),
            ),
            (
                "extra".to_string(),
                PackageVersion::new("2.0.0".parse().unwrap(), built_in_repository()),
            ),
        ]);
        let plan = sync_plan(&required, &installed, test_context()).unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.install_count, 0);
        assert!(
            plan.tasks
                .values()
                .any(|task| task == &("extra".to_string(), TaskKind::Remove))
        );
    }

    #[test]
    fn malformed_dependencies_are_planning_errors_with_positioned_metadata() {
        let packages = required_packages(&[("example", "Imports: cli (>= invalid)\n")]);
        let Err(error) = sync_plan(&packages, &BTreeMap::new(), test_context()) else {
            panic!("expected malformed metadata error")
        };
        let PlanError::Dependencies(source) = &error else {
            panic!("expected metadata error")
        };
        assert!(!source.messages().is_empty());
        // Sync forwards the positioned metadata diagnostic, not a stringified graph error.
        let outer = super::super::SyncError::from(error);
        assert_eq!(
            outer.code().unwrap().to_string(),
            "rpx::description::parse_failed"
        );
        assert!(outer.source_code().is_some());
        assert!(outer.related().unwrap().next().is_some());
    }

    #[test]
    fn graph_validation_errors_retain_the_task_runner_error() {
        use std::error::Error;
        let mut graph = GraphBuilder::<OperationError>::new();
        let task = graph.reserve::<()>();
        let error = PlanError::from(match graph.finish() {
            Err(error) => error,
            Ok(_) => panic!("undefined task should fail validation"),
        });
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<BuildError>()
            .unwrap();
        assert!(matches!(source, BuildError::Undefined(node) if *node == task.id()));
    }
}

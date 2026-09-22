//! Project setup, task execution, and progress reporting for synchronization.

mod operations;
mod plan;

use crate::{
    cache::installer_cache_path,
    description::{DescriptionParseError, ProjectType, project_type, root_package},
    project::{Project, ProjectLibraryError, ProjectResolution, ensure_project_library},
    r::{InstalledPackagesError, installed_packages},
    repository::LocalRepository,
    resolver::PackageVersion,
    ui::progress_count_style,
};
use miette::Diagnostic;
use operations::OperationError;
use plan::{PlanError, TaskId, TaskKind};
use r_package_installer::Installer;
use rpx_task::{ExecutionError, ExecutionEvent, NodeId};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use thiserror::Error;
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

/// Errors at the sync boundary. Individual package operations and graph planning
/// have their own error types and retain their original source chains.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum SyncError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLibrary(#[from] ProjectLibraryError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Operation(#[from] OperationError),
    #[error("failed to join {kind} task for package {package}: {source}")]
    #[diagnostic(code(rpx::sync::task_join_failed))]
    TaskJoin {
        package: String,
        kind: TaskKind,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("sync task engine invariant failed: {message}")]
    #[diagnostic(code(rpx::sync::task_engine_invariant))]
    TaskInvariant { message: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectPackageMode {
    Install,
    Omit,
}

impl From<bool> for ProjectPackageMode {
    fn from(no_install: bool) -> Self {
        if no_install {
            Self::Omit
        } else {
            Self::Install
        }
    }
}

#[derive(Clone)]
struct SyncTaskContext {
    installer: Installer,
    project_library: PathBuf,
    r_version: Arc<semver::Version>,
    span: tracing::Span,
}

pub(crate) async fn sync_resolved_project(
    project: &Project,
    resolution: ProjectResolution,
    project_package: ProjectPackageMode,
) -> Result<(), SyncError> {
    let mut required = resolution.packages;
    let (root_name, root_version) = root_package(&project.root, &project.description)?;
    required.remove(&root_name);
    match (project_type(&project.description), project_package) {
        (ProjectType::Package, ProjectPackageMode::Install) => {
            let root = Arc::new(
                LocalRepository::new(project.root.clone())
                    .with_description(project.description.clone()),
            );
            required.insert(
                root_name,
                (
                    PackageVersion::new(root_version, root),
                    Arc::new(project.description.clone()),
                ),
            );
        }
        (ProjectType::Package, ProjectPackageMode::Omit) | (ProjectType::Project, _) => {}
    }

    let project_library = ensure_project_library(&project.root)?;
    let installer = Installer::new(installer_cache_path());
    let installed = installed_packages(&project_library).await?;
    let sync_span = tracing::info_span!(
        "sync_packages",
        total = tracing::field::Empty,
        completed = 0_u64,
        running = 0_u64,
        pending = tracing::field::Empty,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    let plan = plan::sync_plan(
        &required,
        &installed,
        SyncTaskContext {
            installer,
            project_library,
            r_version: Arc::new(resolution.r_version),
            span: sync_span.clone(),
        },
    )?;
    let total_packages = plan.install_count as u64;
    sync_span.record("total", total_packages);
    sync_span.record("pending", total_packages);
    sync_span.pb_set_style(&progress_count_style());
    sync_span.pb_set_message("sync packages");
    sync_span.pb_set_length(total_packages);
    sync_span.pb_start();

    let mut completed = 0_u64;
    let mut running = 0_u64;
    let result = plan
        .graph
        .execute(|event| {
            match event {
                ExecutionEvent::Started(_) => running += 1,
                ExecutionEvent::Succeeded(node) => {
                    running -= 1;
                    if plan.tasks[&node].1 == TaskKind::Install {
                        completed += 1;
                        sync_span.pb_inc(1);
                    }
                }
                ExecutionEvent::Failed(_) => running -= 1,
            }
            sync_span.record("running", running);
            sync_span.record("completed", completed);
            sync_span.record("pending", total_packages - completed);
        })
        .instrument(sync_span.clone())
        .await
        .map_err(|error| execution_error(error, &plan.tasks));

    sync_span.record("completed", completed);
    sync_span.record("running", 0_u64);
    sync_span.record("pending", total_packages - completed);
    sync_span.record("stage", if result.is_ok() { "done" } else { "failed" });
    sync_span.pb_set_finish_message(&format!("sync packages {completed}/{total_packages}"));
    result
}

fn execution_error(
    error: ExecutionError<OperationError>,
    tasks: &BTreeMap<NodeId, TaskId>,
) -> SyncError {
    match error {
        ExecutionError::Operation { source, .. } => source.into(),
        ExecutionError::Join { node, source } => {
            let (package, kind) = &tasks[&node];
            SyncError::TaskJoin {
                package: package.clone(),
                kind: *kind,
                source,
            }
        }
        ExecutionError::Invariant(message) => SyncError::TaskInvariant { message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use operations::{DownloadPackageArtifactError, InstallPackageError};
    use rpx_task::GraphBuilder;
    use std::error::Error;

    #[tokio::test]
    async fn operation_failure_keeps_package_context_diagnostic_and_io_cause() {
        let mut graph = GraphBuilder::new();
        graph
            .task((), vec![], |()| async {
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
        let error = execution_error(
            graph.finish().unwrap().execute(|_| {}).await.unwrap_err(),
            &BTreeMap::new(),
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
        let io = install
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn task_panic_keeps_join_error_and_package_operation_identity() {
        let mut graph = GraphBuilder::<OperationError>::new();
        let task = graph
            .task((), vec![], |()| async {
                panic!("operation panicked");
                #[allow(unreachable_code)]
                Ok(())
            })
            .unwrap();
        let tasks = BTreeMap::from([(task.id(), ("example".into(), TaskKind::Checkout))]);
        let error = execution_error(
            graph.finish().unwrap().execute(|_| {}).await.unwrap_err(),
            &tasks,
        );
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
    }

    #[test]
    fn download_diagnostic_remains_visible_at_the_sync_boundary() {
        let error = SyncError::from(OperationError::Download {
            package: "example".into(),
            version: "1.2.3".into(),
            source: DownloadPackageArtifactError::UnsupportedRepository,
        });
        assert!(error.to_string().contains("example 1.2.3"));
        assert_eq!(
            error.code().unwrap().to_string(),
            "rpx::sync::package_artifact_download_failed"
        );
        assert!(error.source().unwrap().is::<DownloadPackageArtifactError>());
    }
}

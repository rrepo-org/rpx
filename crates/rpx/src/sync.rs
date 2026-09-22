//! Adapt resolved project state to a sync plan and render its progress.

mod operations;
mod plan;

use crate::{
    description::{DescriptionParseError, ProjectType, project_type, root_package},
    project::{
        Project, ProjectLibraryError, ProjectResolution, RequiredPackages, ensure_project_library,
    },
    r::InstalledPackagesError,
    repository::LocalRepository,
    resolver::PackageVersion,
    ui::progress_count_style,
};
use miette::Diagnostic;
use plan::{PlanError, RunError, SyncPlan, SyncTarget};
use std::sync::Arc;
use thiserror::Error;
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

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
    Run(#[from] RunError),
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

/// Use the supplied DESCRIPTION for the desired root identity and dependencies.
/// The build operation still reads the source tree when it executes.
fn desired_packages(
    project: &Project,
    resolved: RequiredPackages,
    project_package: ProjectPackageMode,
) -> Result<RequiredPackages, DescriptionParseError> {
    let (root_name, root_version) = root_package(&project.root, &project.description)?;
    let root = match (project_type(&project.description), project_package) {
        (ProjectType::Package, ProjectPackageMode::Install) => Some((
            root_name.clone(),
            (
                PackageVersion::new(
                    root_version,
                    Arc::new(
                        LocalRepository::new(project.root.clone())
                            .with_description(project.description.clone()),
                    ),
                ),
                Arc::new(project.description.clone()),
            ),
        )),
        (ProjectType::Package, ProjectPackageMode::Omit) | (ProjectType::Project, _) => None,
    };
    Ok(resolved
        .into_iter()
        .filter(|(name, _)| name != &root_name)
        .chain(root)
        .collect())
}

pub(crate) async fn sync_resolved_project(
    project: &Project,
    resolution: ProjectResolution,
    project_package: ProjectPackageMode,
) -> Result<(), SyncError> {
    let desired = desired_packages(project, resolution.packages, project_package)?;
    let library = ensure_project_library(&project.root)?;
    let target = SyncTarget::inspect(library, resolution.r_version).await?;
    let span = tracing::info_span!(
        "sync_packages",
        total = tracing::field::Empty,
        completed = 0_u64,
        running = 0_u64,
        pending = tracing::field::Empty,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    let plan = span.in_scope(|| SyncPlan::prepare(desired, target))?;
    let total = plan.install_count() as u64;
    span.record("total", total);
    span.record("pending", total);
    span.pb_set_style(&progress_count_style());
    span.pb_set_message("sync packages");
    span.pb_set_length(total);
    span.pb_start();

    let mut completed = 0;
    let result = plan
        .run(|progress| {
            completed = progress.installed_packages as u64;
            span.record("running", progress.running_operations as u64);
            span.record("completed", completed);
            span.record("pending", total - completed);
            span.pb_set_position(completed);
        })
        .instrument(span.clone())
        .await;

    span.record("stage", if result.is_ok() { "done" } else { "failed" });
    span.pb_set_finish_message(&format!("sync packages {completed}/{total}"));
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::built_in_repository;
    use r_description::Description;
    use std::{collections::BTreeMap, path::PathBuf};

    fn project(kind: &str) -> Project {
        Project {
            root: PathBuf::from("unused-project"),
            description: Description::parse(&format!(
                "Package: root\nVersion: 2.0.0\nConfig/rpx/type: {kind}\n"
            )),
        }
    }

    fn resolved() -> RequiredPackages {
        ["root", "dependency"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    (
                        PackageVersion::new("1.0.0".parse().unwrap(), built_in_repository()),
                        Arc::new(Description::parse(&format!(
                            "Package: {name}\nVersion: 1.0.0\n"
                        ))),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn root_policy_uses_supplied_description_without_changing_dependencies() {
        let desired =
            desired_packages(&project("package"), resolved(), ProjectPackageMode::Install).unwrap();
        assert_eq!(desired["root"].0.version().to_string(), "2.0.0");
        let local = desired["root"]
            .0
            .repository()
            .downcast_ref::<LocalRepository>()
            .unwrap();
        assert_eq!(local.path(), std::path::Path::new("unused-project"));
        assert_eq!(desired["dependency"].0.version().to_string(), "1.0.0");
        assert!(
            desired["dependency"]
                .0
                .repository()
                .equals(built_in_repository().as_ref())
        );
    }

    #[test]
    fn omission_and_dependency_only_project_policy_exclude_the_root() {
        let omitted =
            desired_packages(&project("package"), resolved(), ProjectPackageMode::Omit).unwrap();
        let dependency_only =
            desired_packages(&project("project"), resolved(), ProjectPackageMode::Install).unwrap();
        assert_eq!(omitted.keys().collect::<Vec<_>>(), ["dependency"]);
        assert_eq!(dependency_only.keys().collect::<Vec<_>>(), ["dependency"]);
        assert!(
            desired_packages(
                &project("project"),
                BTreeMap::new(),
                ProjectPackageMode::Install
            )
            .unwrap()
            .is_empty()
        );
    }
}

//! Package-specific graph construction. Operations return values; scheduling
//! and result delivery are handled by rpx-task.

use super::*;
use r_metadata::Version;
use rpx_task::{BuildError, ExecutableGraph, GraphBuilder, NodeId, TaskRef};

pub(super) struct SyncPlan {
    pub graph: ExecutableGraph<SyncError>,
    pub tasks: BTreeMap<NodeId, TaskId>,
    pub install_count: usize,
}

struct BuildInput {
    package_root: PathBuf,
    archive_path: PathBuf,
    package: String,
    version: Version,
}

async fn build(input: Arc<BuildInput>) -> Result<PreparedArtifact, SyncError> {
    build_package_archive(
        &input.package_root,
        &input.package,
        input.version.as_ref(),
        &input.archive_path,
    )
    .await
    .map_err(|source| SyncError::PackageBuild {
        package: input.package.clone(),
        source: Box::new(source),
    })?;
    Ok(PreparedArtifact::Source {
        path: input.archive_path.clone(),
    })
}

async fn checkout(
    repository: GitRepository,
    package: String,
    version: Version,
) -> Result<BuildInput, SyncError> {
    let checkout =
        repository
            .checkout()
            .await
            .map_err(|error| SyncError::DownloadArtifactsFailed {
                details: format!("failed to checkout {package}: {error}"),
            })?;
    let commit = repository
        .commit()
        .await
        .map_err(|error| SyncError::DownloadArtifactsFailed {
            details: format!("failed to resolve Git commit for {package}: {error}"),
        })?;
    let package_root = repository
        .subdirectory()
        .map_or_else(|| checkout.clone(), |path| checkout.join(path));
    let archive_path = source_artifact_cache_path(&SourceArtifactCacheKey::new(
        SourceArtifactIdentity::Git {
            remote: repository.remote().clone(),
            commit,
            subdirectory: repository.subdirectory().map(Path::to_path_buf),
        },
        &package,
        version.clone(),
    ));
    Ok(BuildInput {
        package_root,
        archive_path,
        package,
        version,
    })
}

fn engine_error(error: BuildError) -> SyncError {
    SyncError::TaskEngine {
        details: error.to_string(),
    }
}

pub(super) fn sync_plan(
    required: &RequiredPackages,
    installed: &BTreeMap<String, PackageVersion>,
    context: SyncTaskContext,
) -> Result<SyncPlan, SyncError> {
    let mut graph = GraphBuilder::<SyncError>::new();
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
        let (package_version, description) = &required[package];
        let dependency_names: BTreeSet<_> = required_dependencies(
            format!("{package} {}", package_version.version()),
            description,
        )?
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
        let repository = package_version.repository().as_ref();
        let span = context.span.clone();
        let artifact = if let Some(local) = repository.downcast_ref::<LocalRepository>() {
            let input = Arc::new(BuildInput {
                package_root: local.path().to_path_buf(),
                archive_path: source_artifact_cache_path(&SourceArtifactCacheKey::new(
                    SourceArtifactIdentity::Local(local.path().to_path_buf()),
                    package,
                    package_version.version().clone(),
                )),
                package: package.clone(),
                version: package_version.version().clone(),
            });
            let task = graph
                .task((), vec![(shared, 1), (r, 1)], move |()| {
                    build(input).instrument(span)
                })
                .map_err(engine_error)?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Build));
            task
        } else if let Some(git) = repository.downcast_ref::<GitRepository>() {
            let git = git.clone();
            let name = package.clone();
            let version = package_version.version().clone();
            let source = graph
                .task((), vec![(shared, 1), (checkout_slot, 1)], move |()| {
                    checkout(git, name, version).instrument(span)
                })
                .map_err(engine_error)?;
            tasks.insert(source.id(), (package.clone(), TaskKind::Checkout));
            let span = context.span.clone();
            let task = graph
                .task(source, vec![(shared, 1), (r, 1)], move |input| {
                    build(input).instrument(span)
                })
                .map_err(engine_error)?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Build));
            task
        } else {
            let name = package.clone();
            let selected = package_version.clone();
            let r_version = context.r_version.clone();
            let task = graph
                .task((), vec![(shared, 1)], move |()| {
                    async move {
                        let version = selected.version().to_string();
                        download_package_artifact(name.clone(), selected, r_version)
                            .await
                            .map_err(|source| SyncError::DownloadPackageArtifact {
                                package: name,
                                version,
                                source,
                            })
                    }
                    .instrument(span)
                })
                .map_err(engine_error)?;
            tasks.insert(task.id(), (package.clone(), TaskKind::Download));
            task
        };
        let name = package.clone();
        let version = package_version.version().clone();
        let context = context.clone();
        let span = context.span.clone();
        graph
            .define(
                install,
                (artifact, prerequisites),
                vec![(shared, 1), (r, 1)],
                move |(artifact, _completed_dependencies)| {
                    async move {
                        install_package(
                            &context.installer,
                            &context.project_library,
                            &name,
                            &version,
                            context.r_version.as_ref(),
                            &dependencies,
                            artifact,
                        )
                        .await
                        .map_err(|source| SyncError::PackageInstall {
                            package: name,
                            source: Box::new(source),
                        })
                    }
                    .instrument(span)
                },
            )
            .map_err(engine_error)?;
    }
    for name in installed
        .keys()
        .filter(|name| !required.contains_key(*name))
    {
        let package = name.clone();
        let context = context.clone();
        let span = context.span.clone();
        let task = graph
            .task((), vec![(shared, 1)], move |()| {
                remove_package(package, context).instrument(span)
            })
            .map_err(engine_error)?;
        tasks.insert(task.id(), (name.clone(), TaskKind::Remove));
    }
    let graph = graph.finish().map_err(|error| match error {
        BuildError::Graph(rpx_task::GraphError::Cycle { blocked }) => {
            let packages = blocked
                .into_iter()
                .filter_map(|node| {
                    let (package, kind) = &tasks[&node];
                    (*kind == TaskKind::Install).then(|| CycleBlockedPackage {
                        package: package.clone(),
                    })
                })
                .collect();
            DependencyCycleError { packages }.into()
        }
        other => engine_error(other),
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
    use crate::git::{
        GitUrl,
        tests::{commit_file, source_repository},
    };

    #[tokio::test]
    async fn checkout_output_carries_the_pinned_tree_and_archive_destination() {
        let (path, source, pinned) = source_repository("sync-checkout-output");
        let remote = GitUrl::from_local_path(&path);
        let repository = GitRepository::from_parts(remote.clone(), None, None).with_commit(pinned);
        commit_file(
            &source,
            "Package: example\nVersion: 2.0.0\n",
            "advance branch",
        );
        let input = checkout(repository, "example".into(), "1.0.0".parse().unwrap())
            .await
            .unwrap();
        let description = fs::read_to_string(input.package_root.join("DESCRIPTION")).unwrap();
        assert!(description.contains("Version: 1.0.0"));
        assert_eq!(
            input.archive_path,
            source_artifact_cache_path(&SourceArtifactCacheKey::new(
                SourceArtifactIdentity::Git {
                    remote,
                    commit: pinned,
                    subdirectory: None
                },
                "example",
                "1.0.0".parse().unwrap(),
            ))
        );
        fs::remove_dir_all(path).unwrap();
    }
}

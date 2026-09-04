use crate::{
    cache::{
        BinaryArtifactCacheKey, INSTALLER_CACHE_VERSION, RegistryIdentity, SourceArtifactCacheKey,
        SourceArtifactIdentity, binary_artifact_cache_path, installer_cache_path,
        source_artifact_cache_path,
    },
    description::{
        DescriptionParseError, ProjectType, project_type, required_dependencies, root_package,
    },
    http,
    project::{
        Project, ProjectLibraryError, ProjectResolution, RequiredPackages, ensure_project_library,
    },
    r::{self, build_package_archive, installed_packages},
    repository::{
        CranRepository, GitRepository, LocalRepository, RepositoryError, RrepoRepository,
    },
    resolver::PackageVersion,
    ui::{progress_bar_style, progress_count_style, progress_spinner_style},
};
use futures_util::StreamExt;
use miette::Diagnostic;
use r_package_installer::{
    Artifact, BinaryArtifact, BinaryFormat, CacheKey, Digest as InstallerDigest, ExpectedPackage,
    InstallOutcome, Installer, PrepareRequest, RemovalOutcome, SourceArtifact, SourceOptions,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use target_lexicon::{HOST, OperatingSystem};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
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
    InstalledPackages(#[from] r::InstalledPackagesError),
    #[error("failed to remove package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_remove_failed))]
    RemovePackage {
        package: String,
        #[source]
        source: r_package_installer::Error,
    },
    #[error("failed to join blocking package operation: {source}")]
    #[diagnostic(code(rpx::sync::blocking_task_failed))]
    BlockingTask {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("failed to prepare source artifacts: {details}")]
    #[diagnostic(code(rpx::sync::download_failed))]
    DownloadArtifactsFailed { details: String },
    #[error("failed to download artifact for {package} {version}: {source}")]
    #[diagnostic(code(rpx::sync::package_artifact_download_failed))]
    DownloadPackageArtifact {
        package: String,
        version: String,
        #[source]
        source: DownloadPackageArtifactError,
    },
    #[error(transparent)]
    #[diagnostic(transparent)]
    DependencyCycle(#[from] DependencyCycleError),
    #[error("failed to build package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_build_failed))]
    PackageBuild {
        package: String,
        #[source]
        source: Box<r::PackageBuildError>,
    },
    #[error("failed to install package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_install_failed))]
    PackageInstall {
        package: String,
        #[source]
        source: Box<InstallPackageError>,
    },
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
    let mut tasks = sync_tasks(&required, &installed)?;
    let total_packages = pending_package_count(&tasks) as u64;
    let sync_span = tracing::info_span!(
        "sync_packages",
        total = total_packages,
        completed = 0_u64,
        running = 0_u64,
        pending = total_packages,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    sync_span.pb_set_style(&progress_count_style());
    sync_span.pb_set_message("sync packages");
    sync_span.pb_set_length(total_packages);
    sync_span.pb_start();

    let context = SyncTaskContext {
        installer,
        project_library,
        r_version: Arc::new(resolution.r_version),
    };
    let mut resources = ResourcePool::new();
    let mut running = tokio::task::JoinSet::<(TaskRow, Result<(), SyncError>)>::new();
    let mut completed = 0_u64;

    let result = loop {
        while let Some(row) = pop_startable(&mut tasks, &resources) {
            resources.reserve(row.task.1);
            let task = row.task.clone();
            let version = row.version.clone();
            let dependencies = row.dependencies.clone();
            let context = context.clone();
            running.spawn(
                async move {
                    let result = run_sync_task(task, version, dependencies, context).await;
                    (row, result)
                }
                .instrument(sync_span.clone()),
            );
        }

        sync_span.record("running", running.len() as u64);
        sync_span.record("pending", pending_package_count(&tasks) as u64);

        if running.is_empty() && tasks.is_empty() {
            break Ok(());
        }
        if running.is_empty() {
            break Err(DependencyCycleError {
                packages: tasks
                    .iter()
                    .filter(|row| row.task.1 == TaskKind::Install)
                    .map(|row| CycleBlockedPackage {
                        package: row.task.0.clone(),
                    })
                    .collect(),
            }
            .into());
        }

        match running
            .join_next()
            .await
            .expect("running task set should not be empty")
        {
            Ok((row, Ok(()))) => {
                resources.release(row.task.1);
                if row.task.1 == TaskKind::Install {
                    completed += 1;
                    sync_span.pb_inc(1);
                }
                complete_task(&mut tasks, row);
            }
            Ok((row, Err(error))) => {
                resources.release(row.task.1);
                break Err(error);
            }
            Err(error) => {
                break Err(SyncError::DownloadArtifactsFailed {
                    details: format!("sync task failed to join: {error}"),
                });
            }
        }

        sync_span.record("completed", completed);
        sync_span.record("running", running.len() as u64);
        sync_span.record("pending", pending_package_count(&tasks) as u64);
    };

    if result.is_err() {
        while running.join_next().await.is_some() {}
    }

    sync_span.record("completed", completed);
    sync_span.record("running", 0_u64);
    sync_span.record("pending", 0_u64);
    sync_span.record("stage", "done");
    sync_span.pb_set_finish_message(&format!("sync packages {completed}/{total_packages}"));
    result?;

    Ok(())
}

fn package_requires_install(required: &PackageVersion, installed: Option<&PackageVersion>) -> bool {
    let repository = required.repository().as_ref();

    // Git and local sources can change without changing their package version.
    repository.downcast_ref::<GitRepository>().is_some()
        || repository.downcast_ref::<LocalRepository>().is_some()
        || installed != Some(required)
}

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_CHECKOUT_WORKERS: usize = 1;
const SYNC_R_WORKERS: usize = 8;

#[derive(Clone)]
struct SyncTaskContext {
    installer: Installer,
    project_library: PathBuf,
    r_version: Arc<semver::Version>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TaskKind {
    Remove,
    Download,
    Checkout,
    Build,
    Install,
}

type TaskId = (String, TaskKind);

#[derive(Clone)]
struct DependencyInput {
    name: String,
    version: Option<String>,
}

#[derive(Clone)]
struct TaskRow {
    blockers: usize,
    task: TaskId,
    version: PackageVersion,
    dependencies: Vec<DependencyInput>,
    dependents: BTreeSet<TaskId>,
}

impl Ord for TaskRow {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.blockers, &self.task).cmp(&(&other.blockers, &other.task))
    }
}

impl PartialOrd for TaskRow {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for TaskRow {
    fn eq(&self, other: &Self) -> bool {
        self.blockers == other.blockers && self.task == other.task
    }
}

impl Eq for TaskRow {}

struct ResourcePool {
    shared: usize,
    checkout: usize,
    r: usize,
}

impl ResourcePool {
    fn new() -> Self {
        Self {
            shared: 0,
            checkout: 0,
            r: 0,
        }
    }

    fn can_reserve(&self, kind: TaskKind) -> bool {
        self.shared < SYNC_SHARED_WORKERS
            && (kind != TaskKind::Checkout || self.checkout < SYNC_CHECKOUT_WORKERS)
            && (!matches!(kind, TaskKind::Build | TaskKind::Install) || self.r < SYNC_R_WORKERS)
    }

    fn reserve(&mut self, kind: TaskKind) {
        debug_assert!(self.can_reserve(kind));
        self.shared += 1;
        if kind == TaskKind::Checkout {
            self.checkout += 1;
        }
        if matches!(kind, TaskKind::Build | TaskKind::Install) {
            self.r += 1;
        }
    }

    fn release(&mut self, kind: TaskKind) {
        self.shared -= 1;
        if kind == TaskKind::Checkout {
            self.checkout -= 1;
        }
        if matches!(kind, TaskKind::Build | TaskKind::Install) {
            self.r -= 1;
        }
    }
}

fn sync_tasks(
    required: &RequiredPackages,
    installed: &BTreeMap<String, PackageVersion>,
) -> Result<BTreeSet<TaskRow>, SyncError> {
    let package_names = required
        .iter()
        .filter(|(name, (version, _))| package_requires_install(version, installed.get(*name)))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let packages = required
        .iter()
        .filter(|(package, _)| package_names.contains(*package))
        .map(|(package, (version, description))| {
            let dependencies =
                required_dependencies(format!("{package} {}", version.version()), description)?
                    .into_iter()
                    .map(|relation| relation.package().to_string())
                    .collect::<BTreeSet<_>>();
            Ok((package.clone(), version.clone(), dependencies))
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    let install_tasks = packages
        .iter()
        .flat_map(|(package, package_version, dependencies)| {
            let install = (package.clone(), TaskKind::Install);
            let install_blockers = 1 + dependencies
                .iter()
                .filter(|dependency| package_names.contains(*dependency))
                .count();
            let install_dependents = packages
                .iter()
                .filter(|(_, _, dependencies)| dependencies.contains(package))
                .map(|(dependent, _, _)| (dependent.clone(), TaskKind::Install))
                .collect::<BTreeSet<_>>();
            let dependency_inputs = dependencies
                .iter()
                .map(|dependency| DependencyInput {
                    name: dependency.clone(),
                    version: required
                        .get(dependency)
                        .map(|(version, _)| version.version().to_string()),
                })
                .collect::<Vec<_>>();
            let repository = package_version.repository();

            match (
                repository.as_ref().downcast_ref::<LocalRepository>(),
                repository.as_ref().downcast_ref::<GitRepository>(),
            ) {
                (Some(_), _) => vec![
                    TaskRow {
                        blockers: 0,
                        task: (package.clone(), TaskKind::Build),
                        version: package_version.clone(),
                        dependencies: Vec::new(),
                        dependents: BTreeSet::from([install.clone()]),
                    },
                    TaskRow {
                        blockers: install_blockers,
                        task: install,
                        version: package_version.clone(),
                        dependencies: dependency_inputs,
                        dependents: install_dependents,
                    },
                ],
                (_, Some(_)) => {
                    let build = (package.clone(), TaskKind::Build);
                    vec![
                        TaskRow {
                            blockers: 0,
                            task: (package.clone(), TaskKind::Checkout),
                            version: package_version.clone(),
                            dependencies: Vec::new(),
                            dependents: BTreeSet::from([build.clone()]),
                        },
                        TaskRow {
                            blockers: 1,
                            task: build,
                            version: package_version.clone(),
                            dependencies: Vec::new(),
                            dependents: BTreeSet::from([install.clone()]),
                        },
                        TaskRow {
                            blockers: install_blockers,
                            task: install,
                            version: package_version.clone(),
                            dependencies: dependency_inputs,
                            dependents: install_dependents,
                        },
                    ]
                }
                (None, None) => vec![
                    TaskRow {
                        blockers: 0,
                        task: (package.clone(), TaskKind::Download),
                        version: package_version.clone(),
                        dependencies: Vec::new(),
                        dependents: BTreeSet::from([install.clone()]),
                    },
                    TaskRow {
                        blockers: install_blockers,
                        task: install,
                        version: package_version.clone(),
                        dependencies: dependency_inputs,
                        dependents: install_dependents,
                    },
                ],
            }
        });
    let remove_tasks = installed
        .iter()
        .filter(|(package, _)| !required.contains_key(*package))
        .map(|(package, version)| TaskRow {
            blockers: 0,
            task: (package.clone(), TaskKind::Remove),
            version: version.clone(),
            dependencies: Vec::new(),
            dependents: BTreeSet::new(),
        });
    Ok(install_tasks.chain(remove_tasks).collect())
}

fn pop_startable(tasks: &mut BTreeSet<TaskRow>, resources: &ResourcePool) -> Option<TaskRow> {
    let task = tasks
        .iter()
        .take_while(|row| row.blockers == 0)
        .find(|row| resources.can_reserve(row.task.1))?
        .clone();
    tasks.take(&task)
}

fn complete_task(tasks: &mut BTreeSet<TaskRow>, completed: TaskRow) {
    for dependent in completed.dependents {
        let mut row = tasks
            .iter()
            .find(|row| row.task == dependent)
            .cloned()
            .expect("dependent task should exist");
        tasks.take(&row);
        row.blockers -= 1;
        tasks.insert(row);
    }
}

fn pending_package_count(tasks: &BTreeSet<TaskRow>) -> usize {
    tasks
        .iter()
        .filter(|row| row.task.1 == TaskKind::Install)
        .count()
}

#[derive(Debug, Error, Diagnostic)]
#[error("cannot determine package installation order")]
#[diagnostic(
    code(rpx::sync::dependency_cycle),
    help("Update the package requirements to break the dependency cycle before syncing.")
)]
pub(crate) struct DependencyCycleError {
    #[related]
    packages: Vec<CycleBlockedPackage>,
}

#[derive(Debug, Error, Diagnostic)]
#[error("package `{package}` is blocked by a dependency cycle")]
pub(crate) struct CycleBlockedPackage {
    package: String,
}

async fn run_sync_task(
    (package, kind): TaskId,
    package_version: PackageVersion,
    dependencies: Vec<DependencyInput>,
    context: SyncTaskContext,
) -> Result<(), SyncError> {
    match kind {
        TaskKind::Remove => {
            let installer = context.installer;
            let project_library = context.project_library;
            let package_for_remove = package.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                installer.remove(&project_library, &package_for_remove)
            })
            .await
            .map_err(|source| SyncError::BlockingTask { source })?
            .map_err(|source| SyncError::RemovePackage {
                package: package.clone(),
                source,
            })?;
            if let RemovalOutcome::CommittedCleanupPending { lock } = outcome {
                tracing::warn!(package, path = %lock.display(), "package removal committed but cleanup remains pending");
            }
            Ok(())
        }
        TaskKind::Download => {
            let version = package_version.version().to_string();
            download_package_artifact(package.clone(), package_version, context.r_version)
                .await
                .map_err(|source| SyncError::DownloadPackageArtifact {
                    package,
                    version,
                    source,
                })
        }
        TaskKind::Checkout => {
            let repository = package_version
                .repository()
                .as_ref()
                .downcast_ref::<GitRepository>()
                .expect("checkout task should use a Git repository")
                .clone();
            repository
                .checkout()
                .await
                .map_err(|error| SyncError::DownloadArtifactsFailed {
                    details: format!("failed to checkout {package}: {error}"),
                })?;
            Ok(())
        }
        TaskKind::Build => {
            let repository = package_version.repository().as_ref();
            let (package_root, source) =
                if let Some(repository) = repository.downcast_ref::<LocalRepository>() {
                    (
                        repository.path().to_path_buf(),
                        SourceArtifactIdentity::Local(repository.path().to_path_buf()),
                    )
                } else {
                    let repository = repository
                        .downcast_ref::<GitRepository>()
                        .expect("build task should use a local or Git repository");
                    let checkout = repository.checkout_path().await.map_err(|error| {
                        SyncError::DownloadArtifactsFailed {
                            details: format!("failed to locate checkout for {package}: {error}"),
                        }
                    })?;
                    let package_root = repository
                        .subdirectory()
                        .map_or(checkout.clone(), |subdirectory| checkout.join(subdirectory));
                    let commit = repository.commit().await.map_err(|error| {
                        SyncError::DownloadArtifactsFailed {
                            details: format!("failed to resolve Git commit for {package}: {error}"),
                        }
                    })?;
                    (
                        package_root,
                        SourceArtifactIdentity::Git {
                            remote: repository.remote().clone(),
                            commit,
                            subdirectory: repository.subdirectory().map(Path::to_path_buf),
                        },
                    )
                };
            let archive = source_artifact_cache_path(&SourceArtifactCacheKey::new(
                source,
                &package,
                package_version.version().clone(),
            ));
            build_package_archive(
                &package_root,
                &package,
                package_version.version().as_ref(),
                &archive,
            )
            .await
            .map_err(|source| SyncError::PackageBuild {
                package,
                source: Box::new(source),
            })
        }
        TaskKind::Install => install_package(
            &context.installer,
            &context.project_library,
            &package,
            &package_version,
            context.r_version.as_ref(),
            &dependencies,
        )
        .await
        .map_err(|source| SyncError::PackageInstall {
            package,
            source: Box::new(source),
        }),
    }
}

#[derive(Debug, Error)]
pub(crate) enum DownloadPackageArtifactError {
    #[error("unsupported remote package repository")]
    UnsupportedRepository,
    #[error("artifact cache entry is not a file: {}", path.display())]
    InvalidArtifact { path: PathBuf },
    #[error("failed to request binary artifact: {source}")]
    BinaryRequest {
        #[source]
        source: http::BinaryArtifactRequestError,
    },
    #[error("failed to request {artifact} artifact: {source}")]
    Request {
        artifact: &'static str,
        #[source]
        source: reqwest_middleware::Error,
    },
    #[error("{artifact} artifact response failed: {source}")]
    Response {
        artifact: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to create artifact cache directory {}: {source}", path.display())]
    CreateCacheDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create temporary artifact in {}: {source}", path.display())]
    CreateTemporaryArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open temporary artifact {}: {source}", path.display())]
    OpenTemporaryArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read artifact response: {source}")]
    ReadResponse {
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to write temporary artifact {}: {source}", path.display())]
    WriteArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("artifact response was incomplete: expected {expected} bytes, received {actual}")]
    ContentLengthMismatch { expected: u64, actual: u64 },
    #[error("failed to flush temporary artifact {}: {source}", path.display())]
    FlushArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to sync temporary artifact {}: {source}", path.display())]
    SyncArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to publish artifact {}: {source}", path.display())]
    PublishArtifact {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

async fn download_package_artifact(
    package: String,
    package_version: PackageVersion,
    r_version: Arc<semver::Version>,
) -> Result<(), DownloadPackageArtifactError> {
    let version = package_version.version().to_string();
    let span = tracing::info_span!(
        "download_package_artifact",
        package = %package,
        version = %version,
        repository = tracing::field::Empty,
        stage = tracing::field::Empty,
        artifact_kind = tracing::field::Empty,
        bytes = tracing::field::Empty,
        total_bytes = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    span.pb_set_message(&format!("{package} {version} preparing"));
    span.pb_start();

    async {
        let repository = registry_identity(&package_version)
            .ok_or(DownloadPackageArtifactError::UnsupportedRepository)?;
        span.record(
            "repository",
            match &repository {
                RegistryIdentity::Cran(url) | RegistryIdentity::Rrepo(url) => url.as_str(),
            },
        );
        span.record("stage", "downloading binary");
        span.pb_set_message(&format!("{package} {version} downloading binary"));
        match registry_binary_artifact(&repository, &package, &package_version, r_version.as_ref()) {
            Ok(Some(binary)) => {
                let binary_result = async {
                    match artifact_cache_entry(&binary.path) {
                        ArtifactCacheEntry::File => return Ok(()),
                        ArtifactCacheEntry::Invalid => {
                            return Err(DownloadPackageArtifactError::InvalidArtifact {
                                path: binary.path,
                            });
                        }
                        ArtifactCacheEntry::Missing => {}
                    }
                    let response = match &repository {
                        RegistryIdentity::Rrepo(url) => http::rrepo_binary(
                            url,
                            &package,
                            &version,
                            &HOST,
                            r_version.as_ref(),
                        )
                        .await,
                        RegistryIdentity::Cran(url) => http::cran_binary(
                            url,
                            &package,
                            &version,
                            &HOST,
                            r_version.as_ref(),
                        )
                        .await,
                    }
                    .map_err(|source| DownloadPackageArtifactError::BinaryRequest { source })?
                    .error_for_status()
                    .map_err(|source| DownloadPackageArtifactError::Response {
                        artifact: "binary",
                        source,
                    })?;
                    span.record("artifact_kind", "binary");
                    publish_artifact_response(binary.path, response, &span).await
                }
                .await;

                match binary_result {
                    Ok(()) => {
                        span.record("stage", "prepared");
                        span.pb_set_message(&format!("{package} {version} prepared"));
                        return Ok(());
                    }
                    Err(error @ DownloadPackageArtifactError::InvalidArtifact { .. }) => {
                        return Err(error);
                    }
                    Err(error) => tracing::debug!(
                        package = %package,
                        version = %version,
                        %error,
                        "binary artifact unavailable; falling back to source"
                    ),
                }
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(
                package = %package,
                version = %version,
                %error,
                "binary artifact unavailable; falling back to source"
            ),
        }

        span.pb_set_style(&progress_spinner_style());
        span.record("stage", "falling back to source");
        span.pb_set_message(&format!("{package} {version} falling back to source"));
        span.record("stage", "downloading source");
        span.pb_set_message(&format!("{package} {version} downloading source"));
        let source = registry_source_artifact(&repository, &package, &package_version);
        let path = source.path;
        match artifact_cache_entry(&path) {
            ArtifactCacheEntry::File => {
                span.record("stage", "prepared");
                span.pb_set_message(&format!("{package} {version} prepared"));
                return Ok(());
            }
            ArtifactCacheEntry::Invalid => {
                return Err(DownloadPackageArtifactError::InvalidArtifact { path });
            }
            ArtifactCacheEntry::Missing => {}
        }

        let response = match &repository {
            RegistryIdentity::Rrepo(url) => http::rrepo_source_artifact(url, &package, &version)
                .await
                .map_err(|source| DownloadPackageArtifactError::Request {
                    artifact: "source",
                    source,
                })?
                .error_for_status()
                .map_err(|source| DownloadPackageArtifactError::Response {
                    artifact: "source",
                    source,
                })?,
            RegistryIdentity::Cran(url) => {
                let current = http::cran_current_source_tarball(url, &package, &version)
                    .await
                    .map_err(|source| DownloadPackageArtifactError::Request {
                        artifact: "current source",
                        source,
                    })
                    .and_then(|response| {
                        response.error_for_status().map_err(|source| {
                            DownloadPackageArtifactError::Response {
                                artifact: "current source",
                                source,
                            }
                        })
                    });
                match current {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::debug!(%error, "current source artifact unavailable; trying archive");
                        http::cran_archive_source_tarball(url, &package, &version)
                            .await
                            .map_err(|source| DownloadPackageArtifactError::Request {
                                artifact: "archived source",
                                source,
                            })?
                            .error_for_status()
                            .map_err(|source| DownloadPackageArtifactError::Response {
                                artifact: "archived source",
                                source,
                            })?
                    }
                }
            }
        };
        span.record("artifact_kind", "source");
        publish_artifact_response(path, response, &span).await?;
        span.record("stage", "prepared");
        span.pb_set_message(&format!("{package} {version} prepared"));
        Ok(())
    }
    .instrument(span.clone())
    .await
}

#[derive(Debug, Error)]
pub(crate) enum InstallPackageError {
    #[error("unsupported package repository")]
    UnsupportedRepository,
    #[error("failed to resolve Git commit for {package}: {source}")]
    GitCommit {
        package: String,
        #[source]
        source: RepositoryError,
    },
    #[error("failed to determine the macOS binary package type: {source}")]
    MacBinaryType {
        #[source]
        source: http::BinaryArtifactRequestError,
    },
    #[error("no installable artifact exists for {package} {version}")]
    MissingArtifact { package: String, version: String },
    #[error("artifact cache entry is not a file: {}", path.display())]
    InvalidArtifact { path: PathBuf },
    #[error("failed to determine the artifact digest at {}: {source}", path.display())]
    ArtifactDigest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("package installer failed: {source}")]
    Installer {
        #[source]
        source: r_package_installer::Error,
    },
    #[error("failed to join package installer task: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactKind {
    Binary(BinaryFormat),
    Source,
}

struct InstallArtifact {
    path: PathBuf,
    kind: ArtifactKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactCacheEntry {
    File,
    Missing,
    Invalid,
}

fn artifact_cache_entry(path: &Path) -> ArtifactCacheEntry {
    if path.is_file() {
        ArtifactCacheEntry::File
    } else if path.exists() {
        ArtifactCacheEntry::Invalid
    } else {
        ArtifactCacheEntry::Missing
    }
}

impl InstallArtifact {
    fn trace_kind(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Binary(_) => "binary",
            ArtifactKind::Source => "source",
        }
    }

    fn progress_action(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Binary(_) => "extracting",
            ArtifactKind::Source => "installing",
        }
    }

    fn into_installer_artifact(self, project_library: PathBuf) -> Artifact {
        match self.kind {
            ArtifactKind::Binary(format) => Artifact::Binary(BinaryArtifact {
                path: self.path,
                format,
            }),
            ArtifactKind::Source => Artifact::Source(SourceArtifact {
                path: self.path,
                options: SourceOptions {
                    dependency_libraries: vec![project_library],
                    allow_non_staged: true,
                    ..SourceOptions::default()
                },
            }),
        }
    }
}

fn registry_identity(package_version: &PackageVersion) -> Option<RegistryIdentity> {
    let repository = package_version.repository();
    let repository = repository.as_ref();
    if let Some(repository) = repository.downcast_ref::<RrepoRepository>() {
        Some(RegistryIdentity::Rrepo(repository.url().clone()))
    } else {
        repository
            .downcast_ref::<CranRepository>()
            .map(|repository| RegistryIdentity::Cran(repository.url().clone()))
    }
}

fn registry_binary_artifact(
    registry: &RegistryIdentity,
    package: &str,
    package_version: &PackageVersion,
    r_version: &semver::Version,
) -> Result<Option<InstallArtifact>, http::BinaryArtifactRequestError> {
    let format = match HOST.operating_system {
        OperatingSystem::Windows => BinaryFormat::Zip,
        OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => {
            http::r_macos_binary_target(&HOST)?;
            BinaryFormat::TarGz
        }
        _ => return Ok(None),
    };
    let path = binary_artifact_cache_path(&BinaryArtifactCacheKey::new(
        registry.clone(),
        package,
        package_version.version().clone(),
        HOST.clone(),
        r_version.clone(),
    ));
    Ok(Some(InstallArtifact {
        path,
        kind: ArtifactKind::Binary(format),
    }))
}

fn registry_source_artifact(
    registry: &RegistryIdentity,
    package: &str,
    package_version: &PackageVersion,
) -> InstallArtifact {
    InstallArtifact {
        path: source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(registry.clone()),
            package,
            package_version.version().clone(),
        )),
        kind: ArtifactKind::Source,
    }
}

async fn select_install_artifact(
    package: &str,
    package_version: &PackageVersion,
    r_version: &semver::Version,
) -> Result<InstallArtifact, InstallPackageError> {
    let repository = package_version.repository();
    let repository = repository.as_ref();
    let artifact = if let Some(registry) = registry_identity(package_version) {
        if let Some(binary) =
            registry_binary_artifact(&registry, package, package_version, r_version)
                .map_err(|source| InstallPackageError::MacBinaryType { source })?
        {
            match artifact_cache_entry(&binary.path) {
                ArtifactCacheEntry::File => binary,
                ArtifactCacheEntry::Invalid => {
                    return Err(InstallPackageError::InvalidArtifact { path: binary.path });
                }
                ArtifactCacheEntry::Missing => {
                    registry_source_artifact(&registry, package, package_version)
                }
            }
        } else {
            registry_source_artifact(&registry, package, package_version)
        }
    } else if let Some(repository) = repository.downcast_ref::<LocalRepository>() {
        InstallArtifact {
            path: source_artifact_cache_path(&SourceArtifactCacheKey::new(
                SourceArtifactIdentity::Local(repository.path().to_path_buf()),
                package,
                package_version.version().clone(),
            )),
            kind: ArtifactKind::Source,
        }
    } else if let Some(repository) = repository.downcast_ref::<GitRepository>() {
        let commit =
            repository
                .commit()
                .await
                .map_err(|source| InstallPackageError::GitCommit {
                    package: package.to_string(),
                    source,
                })?;
        InstallArtifact {
            path: source_artifact_cache_path(&SourceArtifactCacheKey::new(
                SourceArtifactIdentity::Git {
                    remote: repository.remote().clone(),
                    commit,
                    subdirectory: repository.subdirectory().map(Path::to_path_buf),
                },
                package,
                package_version.version().clone(),
            )),
            kind: ArtifactKind::Source,
        }
    } else {
        return Err(InstallPackageError::UnsupportedRepository);
    };

    match artifact_cache_entry(&artifact.path) {
        ArtifactCacheEntry::File => Ok(artifact),
        ArtifactCacheEntry::Invalid => Err(InstallPackageError::InvalidArtifact {
            path: artifact.path,
        }),
        ArtifactCacheEntry::Missing => Err(InstallPackageError::MissingArtifact {
            package: package.to_string(),
            version: package_version.version().to_string(),
        }),
    }
}

async fn install_package(
    installer: &Installer,
    project_library: &Path,
    package: &str,
    package_version: &PackageVersion,
    r_version: &semver::Version,
    dependencies: &[DependencyInput],
) -> Result<(), InstallPackageError> {
    let version = package_version.version().to_string();
    let span = tracing::info_span!(
        "install_package",
        package = %package,
        version = %version,
        stage = tracing::field::Empty,
        artifact_kind = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    span.pb_set_message(&format!("{package} {version} preparing"));
    span.pb_start();

    async {
        let artifact = select_install_artifact(package, package_version, r_version).await?;
        span.record("artifact_kind", artifact.trace_kind());

        let dependency_inputs = dependencies
            .iter()
            .map(|dependency| (dependency.name.clone(), dependency.version.clone()))
            .collect::<Vec<_>>();
        let prepare_installer = installer.clone();
        let artifact_path = artifact.path.clone();
        let artifact_kind = artifact.kind;
        let project_library_for_prepare = project_library.to_path_buf();
        let package_for_prepare = package.to_string();
        let version_for_prepare = version.clone();
        let r_version_for_prepare = r_version.clone();

        span.record("stage", "preparing cache");
        span.pb_set_message(&format!(
            "{package} {version} {}",
            artifact.progress_action()
        ));
        let entry = tokio::task::spawn_blocking(move || {
            let artifact_digest = artifact_digest(&artifact_path).map_err(|source| {
                InstallPackageError::ArtifactDigest {
                    path: artifact_path.clone(),
                    source,
                }
            })?;
            let key = installer_build_key(
                artifact_kind,
                artifact_digest,
                &package_for_prepare,
                &version_for_prepare,
                &r_version_for_prepare,
                &dependency_inputs,
            );
            let expected = ExpectedPackage {
                name: package_for_prepare,
                version: version_for_prepare,
                r_major_minor: Some(format!(
                    "{}.{}",
                    r_version_for_prepare.major, r_version_for_prepare.minor
                )),
                platform: None,
                architecture: None,
            };
            let artifact = artifact.into_installer_artifact(project_library_for_prepare);
            prepare_installer
                .prepare(&PrepareRequest {
                    key,
                    artifact_digest,
                    expected,
                    artifact,
                })
                .map_err(|source| InstallPackageError::Installer { source })
        })
        .await
        .map_err(|source| InstallPackageError::Join { source })??;

        span.record("stage", "updating project library");
        span.pb_set_message(&format!("{package} {version} publishing"));
        let installer = installer.clone();
        let project_library = project_library.to_path_buf();
        let outcome =
            tokio::task::spawn_blocking(move || installer.materialize(&entry, &project_library))
                .await
                .map_err(|source| InstallPackageError::Join { source })?
                .map_err(|source| InstallPackageError::Installer { source })?;
        if let InstallOutcome::CommittedCleanupPending { lock, .. } = outcome {
            tracing::warn!(package, path = %lock.display(), "package installation committed but cleanup remains pending");
        }

        span.record("stage", "done");
        span.pb_set_message(&format!("{package} {version} done"));
        Ok(())
    }
    .instrument(span.clone())
    .await
}

fn artifact_digest(path: &Path) -> Result<InstallerDigest, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(InstallerDigest::from_bytes(hasher.finalize().into()))
}

fn installer_build_key(
    artifact_kind: ArtifactKind,
    artifact_digest: InstallerDigest,
    package: &str,
    version: &str,
    r_version: &semver::Version,
    dependencies: &[(String, Option<String>)],
) -> CacheKey {
    let mut hasher = Sha256::new();
    let mut field = |value: &[u8]| {
        hasher.update(value.len().to_le_bytes());
        hasher.update(value);
    };
    field(INSTALLER_CACHE_VERSION.as_bytes());
    field(match artifact_kind {
        ArtifactKind::Binary(BinaryFormat::Zip) => b"binary-zip",
        ArtifactKind::Binary(BinaryFormat::TarGz) => b"binary-tar-gz",
        ArtifactKind::Source => b"source",
    });
    field(artifact_digest.as_bytes());
    field(package.as_bytes());
    field(version.as_bytes());
    field(r_version.to_string().as_bytes());
    field(HOST.to_string().as_bytes());
    if matches!(artifact_kind, ArtifactKind::Source) {
        field(b"allow-non-staged=true");
        let mut dependencies = dependencies.to_vec();
        dependencies.sort();
        for (name, version) in &dependencies {
            field(name.as_bytes());
            field(version.as_deref().unwrap_or("").as_bytes());
        }
    }
    CacheKey::from_digest(InstallerDigest::from_bytes(hasher.finalize().into()))
}

async fn publish_artifact_response(
    path: PathBuf,
    response: reqwest::Response,
    span: &tracing::Span,
) -> Result<(), DownloadPackageArtifactError> {
    let content_length = response.content_length();
    let mut stream = response.bytes_stream();

    if let Some(total) = content_length {
        span.record("total_bytes", total);
        span.pb_set_style(&progress_bar_style());
        span.pb_set_length(total);
        span.pb_set_position(0);
    }

    let parent = path
        .parent()
        .ok_or_else(|| DownloadPackageArtifactError::PublishArtifact {
            path: path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artifact cache path has no parent",
            ),
        })?;
    tokio::fs::create_dir_all(parent).await.map_err(|source| {
        DownloadPackageArtifactError::CreateCacheDirectory {
            path: parent.to_path_buf(),
            source,
        }
    })?;
    let temporary_path = tempfile::Builder::new()
        .prefix(".rpx-artifact-")
        .tempfile_in(parent)
        .map_err(
            |source| DownloadPackageArtifactError::CreateTemporaryArtifact {
                path: parent.to_path_buf(),
                source,
            },
        )?
        .into_temp_path();
    let mut file = tokio::fs::File::create(&temporary_path)
        .await
        .map_err(
            |source| DownloadPackageArtifactError::OpenTemporaryArtifact {
                path: temporary_path.to_path_buf(),
                source,
            },
        )?;

    let mut written = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|source| DownloadPackageArtifactError::ReadResponse { source })?;
        let chunk_len = chunk.len() as u64;

        file.write_all(&chunk).await.map_err(|source| {
            DownloadPackageArtifactError::WriteArtifact {
                path: temporary_path.to_path_buf(),
                source,
            }
        })?;

        written += chunk_len;

        span.record("bytes", written);

        if content_length.is_some() {
            span.pb_inc(chunk_len);
        }
    }

    if let Some(expected) = content_length
        && written != expected
    {
        return Err(DownloadPackageArtifactError::ContentLengthMismatch {
            expected,
            actual: written,
        });
    }

    file.flush()
        .await
        .map_err(|source| DownloadPackageArtifactError::FlushArtifact {
            path: temporary_path.to_path_buf(),
            source,
        })?;
    file.sync_all()
        .await
        .map_err(|source| DownloadPackageArtifactError::SyncArtifact {
            path: temporary_path.to_path_buf(),
            source,
        })?;
    drop(file);
    tokio::fs::rename(&temporary_path, &path)
        .await
        .map_err(|source| DownloadPackageArtifactError::PublishArtifact {
            path: path.clone(),
            source,
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{PackageRepository, built_in_repository};
    use r_description::Description;
    use r_metadata::Remote;

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
                        PackageVersion::new(
                            "1.0.0".parse().expect("version fixture should parse"),
                            built_in_repository(),
                        ),
                        Arc::new(description),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn sync_tasks_release_by_kind_and_package_dependency() {
        let packages =
            required_packages(&[("dependency", ""), ("dependent", "Imports: dependency\n")]);
        let mut tasks = sync_tasks(&packages, &BTreeMap::new()).unwrap();
        let resources = ResourcePool::new();

        let dependency_download = pop_startable(&mut tasks, &resources).unwrap();
        assert_eq!(dependency_download.task.0, "dependency");
        assert_eq!(dependency_download.task.1, TaskKind::Download);
        complete_task(&mut tasks, dependency_download);

        let dependency_install = pop_startable(&mut tasks, &resources).unwrap();
        assert_eq!(dependency_install.task.0, "dependency");
        assert_eq!(dependency_install.task.1, TaskKind::Install);
        complete_task(&mut tasks, dependency_install);

        let dependent_download = pop_startable(&mut tasks, &resources).unwrap();
        assert_eq!(dependent_download.task.0, "dependent");
        assert_eq!(dependent_download.task.1, TaskKind::Download);
        complete_task(&mut tasks, dependent_download);

        let dependent_install = pop_startable(&mut tasks, &resources).unwrap();
        assert_eq!(dependent_install.task.0, "dependent");
        assert_eq!(dependent_install.task.1, TaskKind::Install);
        assert_eq!(dependent_install.dependencies.len(), 1);
        assert_eq!(dependent_install.dependencies[0].name, "dependency");
        assert_eq!(
            dependent_install.dependencies[0].version.as_deref(),
            Some("1.0.0")
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

        let tasks = sync_tasks(&required, &installed).unwrap();

        assert_eq!(tasks.len(), 1);
        assert!(tasks.iter().any(|row| {
            row.task == ("extra".to_string(), TaskKind::Remove) && row.blockers == 0
        }));
    }

    #[test]
    fn resource_pool_enforces_shared_and_subset_limits() {
        let mut resources = ResourcePool::new();
        resources.reserve(TaskKind::Checkout);
        assert!(!resources.can_reserve(TaskKind::Checkout));
        assert!(resources.can_reserve(TaskKind::Build));

        resources.release(TaskKind::Checkout);
        for _ in 0..SYNC_R_WORKERS {
            resources.reserve(TaskKind::Build);
        }
        assert!(!resources.can_reserve(TaskKind::Install));
        assert!(resources.can_reserve(TaskKind::Download));

        for _ in SYNC_R_WORKERS..SYNC_SHARED_WORKERS {
            resources.reserve(TaskKind::Download);
        }
        assert!(!resources.can_reserve(TaskKind::Download));
    }

    #[test]
    fn artifact_digest_tracks_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("artifact.tar.gz");
        fs::write(&artifact, "package artifact").unwrap();
        let first = artifact_digest(&artifact).unwrap();
        fs::write(&artifact, "changed package artifact").unwrap();

        assert_ne!(artifact_digest(&artifact).unwrap(), first);
    }

    #[test]
    fn artifact_cache_entries_distinguish_files_directories_and_missing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("artifact.tar.gz");
        let nested_directory = directory.path().join("artifact-directory");
        fs::write(&file, "artifact").unwrap();
        fs::create_dir(&nested_directory).unwrap();

        assert_eq!(artifact_cache_entry(&file), ArtifactCacheEntry::File);
        assert_eq!(
            artifact_cache_entry(&nested_directory),
            ArtifactCacheEntry::Invalid
        );
        assert_eq!(
            artifact_cache_entry(&directory.path().join("missing")),
            ArtifactCacheEntry::Missing
        );
    }

    #[test]
    fn install_artifact_maps_source_options_for_the_installer() {
        let artifact_path = PathBuf::from("package.tar.gz");
        let project_library = PathBuf::from("project-library");
        let artifact = InstallArtifact {
            path: artifact_path.clone(),
            kind: ArtifactKind::Source,
        }
        .into_installer_artifact(project_library.clone());

        let Artifact::Source(source) = artifact else {
            panic!("source selection should produce a source artifact");
        };
        assert_eq!(source.path, artifact_path);
        assert_eq!(source.options.dependency_libraries, vec![project_library]);
        assert!(source.options.allow_non_staged);
    }

    #[test]
    fn install_artifact_preserves_binary_format() {
        let artifact_path = PathBuf::from("package.zip");
        let artifact = InstallArtifact {
            path: artifact_path.clone(),
            kind: ArtifactKind::Binary(BinaryFormat::Zip),
        }
        .into_installer_artifact(PathBuf::from("unused-library"));

        let Artifact::Binary(binary) = artifact else {
            panic!("binary selection should produce a binary artifact");
        };
        assert_eq!(binary.path, artifact_path);
        assert_eq!(binary.format, BinaryFormat::Zip);
    }

    #[test]
    fn installer_build_key_tracks_build_inputs() {
        fn key(
            kind: ArtifactKind,
            digest: u8,
            package: &str,
            version: &str,
            r_version: &str,
            dependencies: &[(String, Option<String>)],
        ) -> String {
            installer_build_key(
                kind,
                InstallerDigest::from_bytes([digest; 32]),
                package,
                version,
                &semver::Version::parse(r_version).unwrap(),
                dependencies,
            )
            .to_string()
        }
        let dependencies = vec![("dependency".into(), Some("1.0.0".into()))];
        let baseline = key(
            ArtifactKind::Source,
            1,
            "package",
            "1.0.0",
            "4.5.1",
            &dependencies,
        );

        assert_ne!(
            baseline,
            key(
                ArtifactKind::Binary(BinaryFormat::Zip),
                1,
                "package",
                "1.0.0",
                "4.5.1",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                2,
                "package",
                "1.0.0",
                "4.5.1",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                1,
                "other",
                "1.0.0",
                "4.5.1",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                1,
                "package",
                "2.0.0",
                "4.5.1",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                1,
                "package",
                "1.0.0",
                "4.4.2",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                1,
                "package",
                "1.0.0",
                "4.5.1",
                &[("other".into(), Some("1.0.0".into()))]
            )
        );
        assert_ne!(
            baseline,
            key(
                ArtifactKind::Source,
                1,
                "package",
                "1.0.0",
                "4.5.1",
                &[("dependency".into(), Some("2.0.0".into()))]
            )
        );
    }

    #[test]
    fn installer_build_key_sorts_dependencies() {
        let digest = InstallerDigest::from_bytes([1; 32]);
        let key = |dependencies: &[(String, Option<String>)]| {
            installer_build_key(
                ArtifactKind::Source,
                digest,
                "package",
                "1.0.0",
                &semver::Version::new(4, 5, 1),
                dependencies,
            )
            .to_string()
        };

        assert_eq!(
            key(&[("a".into(), Some("1.0.0".into())), ("b".into(), None)]),
            key(&[("b".into(), None), ("a".into(), Some("1.0.0".into()))])
        );
    }
}

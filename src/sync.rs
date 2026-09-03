use crate::{
    cache::{
        BinaryArtifactCacheKey, CompiledPackageCacheKey, RegistryIdentity, SourceArtifactCacheKey,
        SourceArtifactIdentity, binary_artifact_cache_path, compiled_package_cache_path,
        source_artifact_cache_path,
    },
    description::{DescriptionParseError, project_type, required_dependencies, root_package},
    http,
    project::{
        Project, ProjectLibraryError, ProjectResolution, RequiredPackages, cache_dir_path,
        ensure_project_library,
    },
    r::{
        self, BasePackagesError, base_packages, build_package_archive, install_package_artifact,
        installed_packages, remove_packages_from_venv,
    },
    repository::{
        CranRepository, GitRepository, LocalRepository, RepositoryError, RrepoRepository,
    },
    resolver::PackageVersion,
    ui::{progress_bar_style, progress_spinner_style},
};
use futures_util::StreamExt;
use miette::Diagnostic;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
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
    BasePackages(#[from] BasePackagesError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] r::InstalledPackagesError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    RemovePackages(#[from] r::PackageRemovalError),
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
    PackageGraph(#[from] PackageGraphError),
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

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SyncReport {
    pub(crate) installed_before: BTreeSet<String>,
}

pub(crate) async fn sync_resolved_project(
    project: &Project,
    resolution: ProjectResolution,
    project_package: ProjectPackageMode,
) -> Result<SyncReport, SyncError> {
    let mut required = resolution.packages;
    let (root_name, root_version) = root_package(&project.root, &project.description)?;
    required.remove(&root_name);
    if project_type(&project.root, &project.description)?.is_installable()
        && project_package == ProjectPackageMode::Install
    {
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

    validate_package_graph(&required).await?;

    let project_library = ensure_project_library(&project.root)?;
    let installed = installed_packages(&project_library).await?;
    let installed_before = installed.keys().cloned().collect();
    let removed = installed
        .iter()
        .filter(|(name, installed_version)| {
            required.get(*name).is_none_or(|(required_version, _)| {
                package_requires_install(required_version, Some(installed_version))
            })
        })
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let packages_to_install = required
        .into_iter()
        .filter(|(name, (required_version, _))| {
            package_requires_install(required_version, installed.get(name))
        })
        .collect::<RequiredPackages>();
    remove_packages_from_venv(&project_library, &removed)?;
    install_required_packages(&project_library, packages_to_install, &resolution.r_version).await?;

    Ok(SyncReport { installed_before })
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
    project_library: PathBuf,
    r_version: Arc<semver::Version>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TaskKind {
    Download,
    Checkout,
    Build,
    Install,
}

type TaskId = (String, TaskKind);

#[derive(Clone)]
struct TaskRow {
    blockers: usize,
    task: TaskId,
    version: PackageVersion,
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

fn install_tasks(packages: RequiredPackages) -> Result<BTreeSet<TaskRow>, SyncError> {
    let package_names = packages.keys().cloned().collect::<BTreeSet<_>>();
    let packages = packages
        .into_iter()
        .map(|(package, (version, description))| {
            let dependencies =
                required_dependencies(format!("{package} {}", version.version()), &description)?
                    .into_iter()
                    .map(|relation| relation.package().to_string())
                    .collect::<BTreeSet<_>>();
            Ok((package, version, dependencies))
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    Ok(packages
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
                        dependents: BTreeSet::from([install.clone()]),
                    },
                    TaskRow {
                        blockers: install_blockers,
                        task: install,
                        version: package_version.clone(),
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
                            dependents: BTreeSet::from([build.clone()]),
                        },
                        TaskRow {
                            blockers: 1,
                            task: build,
                            version: package_version.clone(),
                            dependents: BTreeSet::from([install.clone()]),
                        },
                        TaskRow {
                            blockers: install_blockers,
                            task: install,
                            version: package_version.clone(),
                            dependents: install_dependents,
                        },
                    ]
                }
                (None, None) => vec![
                    TaskRow {
                        blockers: 0,
                        task: (package.clone(), TaskKind::Download),
                        version: package_version.clone(),
                        dependents: BTreeSet::from([install.clone()]),
                    },
                    TaskRow {
                        blockers: install_blockers,
                        task: install,
                        version: package_version.clone(),
                        dependents: install_dependents,
                    },
                ],
            }
        })
        .collect())
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
pub(crate) enum PackageGraphError {
    #[error("package dependency metadata for `{package}` is invalid: {details}")]
    #[diagnostic(code(rpx::sync::invalid_package_dependencies))]
    InvalidDependencies { package: String, details: String },

    #[error("package dependency graph is incomplete")]
    #[diagnostic(
        code(rpx::sync::missing_package_dependencies),
        help("Include the missing packages in the sync or update the package requirements.")
    )]
    MissingDependencies {
        #[related]
        dependencies: Vec<MissingPackageDependency>,
    },

    #[error("cannot determine package installation order")]
    #[diagnostic(
        code(rpx::sync::dependency_cycle),
        help("Update the package requirements to break the dependency cycle before syncing.")
    )]
    DependencyCycle {
        #[related]
        packages: Vec<CycleBlockedPackage>,
    },
}

#[derive(Debug, Error, Diagnostic)]
#[error(
    "package `{package}` requires `{dependency}`, but `{dependency}` is not included in the sync"
)]
pub(crate) struct MissingPackageDependency {
    package: String,
    dependency: String,
}

#[derive(Debug, Error, Diagnostic)]
#[error("package `{package}` is blocked by a dependency cycle")]
pub(crate) struct CycleBlockedPackage {
    package: String,
}

async fn install_required_packages(
    project_library: &Path,
    packages: RequiredPackages,
    r_version: &semver::Version,
) -> Result<(), SyncError> {
    let total_packages = packages.len() as u64;
    let sync_span = tracing::info_span!(
        "sync_packages",
        total = total_packages,
        completed = 0_u64,
        running = 0_u64,
        pending = total_packages,
        stage = tracing::field::Empty,
        indicatif.pb_show = true,
    );
    sync_span.pb_set_style(&progress_spinner_style());
    sync_span.pb_set_message(&format!("sync packages 0/{total_packages}"));
    sync_span.pb_set_length(total_packages);
    sync_span.pb_start();

    if packages.is_empty() {
        sync_span.record("stage", "done");
        sync_span.pb_set_finish_message("sync packages 0/0");
        return Ok(());
    }

    let context = SyncTaskContext {
        project_library: project_library.to_path_buf(),
        r_version: Arc::new(r_version.clone()),
    };
    let mut tasks = install_tasks(packages)?;
    let mut resources = ResourcePool::new();
    let mut running = tokio::task::JoinSet::<(TaskRow, Result<(), SyncError>)>::new();
    let mut completed = 0_u64;

    let result = loop {
        while let Some(row) = pop_startable(&mut tasks, &resources) {
            resources.reserve(row.task.1);
            let task = row.task.clone();
            let version = row.version.clone();
            let context = context.clone();
            running.spawn(
                async move {
                    let result = run_sync_task(task, version, context).await;
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
            break Err(SyncError::DownloadArtifactsFailed {
                details: "package task graph stalled with no runnable tasks".to_string(),
            });
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
        sync_span.pb_set_position(completed);
        sync_span.pb_set_message(&format!("sync packages {completed}/{total_packages}"));
    };

    if result.is_err() {
        while running.join_next().await.is_some() {}
    }

    sync_span.record("completed", completed);
    sync_span.record("running", 0_u64);
    sync_span.record("pending", 0_u64);
    sync_span.record("stage", "done");
    sync_span.pb_set_finish_message(&format!("sync packages {completed}/{total_packages}"));
    result
}

async fn run_sync_task(
    (package, kind): TaskId,
    package_version: PackageVersion,
    context: SyncTaskContext,
) -> Result<(), SyncError> {
    match kind {
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
            &context.project_library,
            &package,
            &package_version,
            context.r_version.as_ref(),
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
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&format!("{package} {version} preparing"));
    span.pb_start();

    async {
        let repository = package_version.repository();
        let repository =
            if let Some(repository) = repository.as_ref().downcast_ref::<RrepoRepository>() {
                RegistryIdentity::Rrepo(repository.url().clone())
            } else if let Some(repository) = repository.as_ref().downcast_ref::<CranRepository>() {
                RegistryIdentity::Cran(repository.url().clone())
            } else {
                return Err(DownloadPackageArtifactError::UnsupportedRepository);
            };
        span.record(
            "repository",
            match &repository {
                RegistryIdentity::Cran(url) | RegistryIdentity::Rrepo(url) => url.as_str(),
            },
        );
        let compiled = compiled_package_cache_path(&CompiledPackageCacheKey::new(
            repository.clone(),
            &package,
            package_version.version().clone(),
            HOST.clone(),
            r_version.as_ref().clone(),
        ));
        if compiled.is_dir() {
            span.record("artifact_kind", "compiled-cache");
            span.record("stage", "prepared");
            span.pb_set_message(&format!("{package} {version} prepared"));
            span.pb_tick();
            return Ok(());
        }

        span.record("stage", "downloading binary");
        span.pb_set_message(&format!("{package} {version} downloading binary"));
        span.pb_tick();
        let binary = async {
            let key = BinaryArtifactCacheKey::new(
                repository.clone(),
                &package,
                package_version.version().clone(),
                HOST.clone(),
                r_version.as_ref().clone(),
            );
            let path = binary_artifact_cache_path(&key);
            if path.exists() {
                return Ok(());
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
            publish_artifact_response(path, response, &span).await
        }
        .await;

        let binary_error = match binary {
            Ok(()) => {
                span.record("stage", "prepared");
                span.pb_set_message(&format!("{package} {version} prepared"));
                span.pb_tick();
                return Ok(());
            }
            Err(error) => error,
        };
        tracing::debug!(
            package = %package,
            version = %version,
            error = %binary_error,
            "binary artifact unavailable; falling back to source"
        );

        span.pb_set_style(&progress_spinner_style());
        span.record("stage", "falling back to source");
        span.pb_set_message(&format!("{package} {version} falling back to source"));
        span.pb_tick();
        span.record("stage", "downloading source");
        span.pb_set_message(&format!("{package} {version} downloading source"));
        span.pb_tick();
        let key = SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Registry(repository.clone()),
            &package,
            package_version.version().clone(),
        );
        let path = source_artifact_cache_path(&key);
        if path.exists() {
            span.record("stage", "prepared");
            span.pb_set_message(&format!("{package} {version} prepared"));
            span.pb_tick();
            return Ok(());
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
        span.pb_tick();
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
    #[error("failed to prepare package install workspace root at {}: {source}", path.display())]
    WorkspaceRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create package install workspace in {}: {source}", path.display())]
    Workspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create temporary package library at {}: {source}", path.display())]
    TemporaryLibrary {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Command(#[from] r::PackageInstallError),
    #[error("installed package directory is missing at {}", path.display())]
    InstalledPackageMissing { path: PathBuf },
    #[error("failed to inspect installed package directory at {}: {source}", path.display())]
    InspectInstalledPackage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to prepare compiled package cache directory at {}: {source}", path.display())]
    CompiledCacheDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("compiled package cache path is not a directory: {}", path.display())]
    InvalidCompiledCache { path: PathBuf },
    #[error("failed to publish compiled package at {}: {source}", path.display())]
    PublishCompiledPackage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    PublishProject(#[from] PublishInstalledPackageError),
    #[error("failed to clean package install workspace at {}: {source}", path.display())]
    Cleanup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub(crate) enum PublishInstalledPackageError {
    #[error("failed to publish package because the destination already exists: {}", path.display())]
    DestinationExists { path: PathBuf },
    #[error("failed to create package staging directory in {}: {source}", path.display())]
    CreateStagingDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to stage installed package from {}: {source}", path.display())]
    StagePackage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("staged package metadata is missing at {}", path.display())]
    MissingMetadata { path: PathBuf },
    #[error("failed to publish installed package at {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to clean package staging directory at {}: {source}", path.display())]
    Cleanup {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to join package publication task: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

async fn install_package(
    project_library: &Path,
    package: &str,
    package_version: &PackageVersion,
    r_version: &semver::Version,
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
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&format!("{package} {version} installing"));
    span.pb_start();

    async {
        let repository = package_version.repository();
        let repository = repository.as_ref();
        let registry = if let Some(repository) = repository.downcast_ref::<RrepoRepository>() {
            Some(RegistryIdentity::Rrepo(repository.url().clone()))
        } else {
            repository
                .downcast_ref::<CranRepository>()
                .map(|repository| RegistryIdentity::Cran(repository.url().clone()))
        };
        let compiled = registry.as_ref().map(|registry| {
            compiled_package_cache_path(&CompiledPackageCacheKey::new(
                registry.clone(),
                package,
                package_version.version().clone(),
                HOST.clone(),
                r_version.clone(),
            ))
        });

        if let Some(compiled) = &compiled
            && compiled.is_dir()
        {
            span.record("artifact_kind", "compiled-cache");
            span.record("stage", "restoring project library");
            span.pb_set_message(&format!("{package} {version} restoring project library"));
            span.pb_tick();
            publish_installed_package(compiled, project_library, package).await?;
            span.record("stage", "done");
            span.pb_set_message(&format!("{package} {version} done"));
            span.pb_tick();
            return Ok(());
        }

        let (artifact, install_type) = if let Some(registry) = &registry {
            let binary = match HOST.operating_system {
                OperatingSystem::Windows => Some("win.binary".to_string()),
                OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_) => Some(format!(
                    "mac.binary.{}",
                    http::r_macos_binary_target(&HOST)
                        .map_err(|source| InstallPackageError::MacBinaryType { source })?
                )),
                _ => None,
            }
            .map(|install_type| {
                let path = binary_artifact_cache_path(&BinaryArtifactCacheKey::new(
                    registry.clone(),
                    package,
                    package_version.version().clone(),
                    HOST.clone(),
                    r_version.clone(),
                ));
                (path, install_type)
            });
            if let Some((path, install_type)) = binary
                && path.is_file()
            {
                (path, install_type)
            } else {
                (
                    source_artifact_cache_path(&SourceArtifactCacheKey::new(
                        SourceArtifactIdentity::Registry(registry.clone()),
                        package,
                        package_version.version().clone(),
                    )),
                    "source".to_string(),
                )
            }
        } else if let Some(repository) = repository.downcast_ref::<LocalRepository>() {
            (
                source_artifact_cache_path(&SourceArtifactCacheKey::new(
                    SourceArtifactIdentity::Local(repository.path().to_path_buf()),
                    package,
                    package_version.version().clone(),
                )),
                "source".to_string(),
            )
        } else if let Some(repository) = repository.downcast_ref::<GitRepository>() {
            let commit = repository.commit().await.map_err(|source| {
                InstallPackageError::GitCommit {
                    package: package.to_string(),
                    source,
                }
            })?;
            (
                source_artifact_cache_path(&SourceArtifactCacheKey::new(
                    SourceArtifactIdentity::Git {
                        remote: repository.remote().clone(),
                        commit,
                        subdirectory: repository.subdirectory().map(Path::to_path_buf),
                    },
                    package,
                    package_version.version().clone(),
                )),
                "source".to_string(),
            )
        } else {
            return Err(InstallPackageError::UnsupportedRepository);
        };
        if !artifact.is_file() {
            return Err(InstallPackageError::MissingArtifact {
                package: package.to_string(),
                version,
            });
        }
        span.record("artifact_kind", install_type.as_str());

        let workspace_parent = cache_dir_path().join("build-temp");
        tokio::fs::create_dir_all(&workspace_parent)
            .await
            .map_err(|source| InstallPackageError::WorkspaceRoot {
                path: workspace_parent.clone(),
                source,
            })?;
        let workspace = tempfile::Builder::new()
            .prefix("rpx-install-")
            .tempdir_in(&workspace_parent)
            .map_err(|source| InstallPackageError::Workspace {
                path: workspace_parent,
                source,
            })?;
        let workspace_path = workspace.path().to_path_buf();
        let temporary_library = workspace.path().join("library");
        tokio::fs::create_dir(&temporary_library)
            .await
            .map_err(|source| InstallPackageError::TemporaryLibrary {
                path: temporary_library.clone(),
                source,
            })?;

        let result = async {
            span.record("stage", "installing");
            span.pb_set_message(&format!("{package} {version} installing"));
            span.pb_tick();
            install_package_artifact(
                project_library,
                &artifact,
                package,
                &version,
                &install_type,
                &temporary_library,
            )
            .await?;

            let installed = temporary_library.join(package);
            match tokio::fs::metadata(&installed).await {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(InstallPackageError::InstalledPackageMissing { path: installed });
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Err(InstallPackageError::InstalledPackageMissing { path: installed });
                }
                Err(source) => {
                    return Err(InstallPackageError::InspectInstalledPackage {
                        path: installed,
                        source,
                    });
                }
            }

            let installed = if let Some(compiled) = &compiled {
                span.record("stage", "storing cache");
                span.pb_set_message(&format!("{package} {version} storing cache"));
                span.pb_tick();
                let parent = compiled
                    .parent()
                    .expect("compiled package cache path should have a parent");
                tokio::fs::create_dir_all(parent).await.map_err(|source| {
                    InstallPackageError::CompiledCacheDirectory {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;
                if compiled.exists() {
                    if !compiled.is_dir() {
                        return Err(InstallPackageError::InvalidCompiledCache {
                            path: compiled.clone(),
                        });
                    }
                } else {
                    tokio::fs::rename(&installed, compiled).await.map_err(|source| {
                        InstallPackageError::PublishCompiledPackage {
                            path: compiled.clone(),
                            source,
                        }
                    })?;
                }
                compiled.clone()
            } else {
                installed
            };

            span.record("stage", "restoring project library");
            span.pb_set_message(&format!("{package} {version} restoring project library"));
            span.pb_tick();
            publish_installed_package(&installed, project_library, package).await?;
            Ok(())
        }
        .await;

        span.record("stage", "cleaning up");
        span.pb_set_message(&format!("{package} {version} cleaning up"));
        span.pb_tick();
        let cleanup = tokio::task::spawn_blocking(move || workspace.close())
            .await
            .map_err(std::io::Error::other)
            .and_then(|result| result);
        if let Err(error) = &cleanup
            && result.is_err()
        {
            tracing::warn!(%error, path = %workspace_path.display(), "failed to clean package install workspace");
        }
        result?;
        cleanup.map_err(|source| InstallPackageError::Cleanup {
            path: workspace_path,
            source,
        })?;

        span.record("stage", "done");
        span.pb_set_message(&format!("{package} {version} done"));
        span.pb_tick();
        Ok(())
    }
    .instrument(span.clone())
    .await
}

async fn publish_installed_package(
    source: &Path,
    project_library: &Path,
    package: &str,
) -> Result<(), PublishInstalledPackageError> {
    let source = source.to_path_buf();
    let project_library = project_library.to_path_buf();
    let package = package.to_string();
    tokio::task::spawn_blocking(move || {
        let destination = project_library.join(&package);
        if destination.exists() {
            return Err(PublishInstalledPackageError::DestinationExists { path: destination });
        }
        let staging = tempfile::Builder::new()
            .prefix(".rpx-install-")
            .tempdir_in(&project_library)
            .map_err(
                |source| PublishInstalledPackageError::CreateStagingDirectory {
                    path: project_library.clone(),
                    source,
                },
            )?;
        let staging_path = staging.path().to_path_buf();
        let staged_package = staging.path().join(&package);
        copy_directory(&source, &staged_package).map_err(|error| {
            PublishInstalledPackageError::StagePackage {
                path: source,
                source: error,
            }
        })?;
        let metadata = staged_package.join("DESCRIPTION");
        if !metadata.is_file() {
            return Err(PublishInstalledPackageError::MissingMetadata { path: metadata });
        }
        if destination.exists() {
            return Err(PublishInstalledPackageError::DestinationExists { path: destination });
        }
        fs::rename(&staged_package, &destination).map_err(|source| {
            PublishInstalledPackageError::Publish {
                path: destination,
                source,
            }
        })?;
        staging
            .close()
            .map_err(|source| PublishInstalledPackageError::Cleanup {
                path: staging_path,
                source,
            })
    })
    .await
    .map_err(|source| PublishInstalledPackageError::Join { source })?
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&source, &destination)?;
        } else {
            fs::copy(source, destination)?;
        }
    }
    Ok(())
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
        } else {
            span.pb_tick();
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

async fn validate_package_graph(packages: &RequiredPackages) -> Result<(), SyncError> {
    let base_packages = base_packages().await?;
    let required_names = packages.keys().cloned().collect::<BTreeSet<_>>();
    let indegree = required_names
        .iter()
        .map(|name| (name.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    let dependents = required_names
        .iter()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();

    let dependencies = packages
        .iter()
        .map(|(package, (version, description))| {
            required_dependencies(format!("{package} {}", version.version()), description)
                .map(|dependencies| {
                    dependencies
                        .into_iter()
                        .map(|dependency| (package.clone(), dependency.package().to_string()))
                        .collect::<BTreeSet<_>>()
                })
                .map_err(|error| PackageGraphError::InvalidDependencies {
                    package: package.clone(),
                    details: error.to_string(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .filter(|(_, dependency)| !base_packages.contains(dependency));
    let (missing, internal): (Vec<_>, Vec<_>) =
        dependencies.partition(|(_, dependency)| !required_names.contains(dependency));

    if !missing.is_empty() {
        return Err(PackageGraphError::MissingDependencies {
            dependencies: missing
                .into_iter()
                .map(|(package, dependency)| MissingPackageDependency {
                    package,
                    dependency,
                })
                .collect(),
        }
        .into());
    }

    let (mut indegree, dependents) = internal.into_iter().fold(
        (indegree, dependents),
        |(mut indegree, mut dependents), (package, dependency)| {
            *indegree
                .get_mut(&package)
                .expect("required package should have indegree") += 1;
            dependents
                .get_mut(&dependency)
                .expect("required dependency should exist")
                .insert(package);
            (indegree, dependents)
        },
    );

    let mut ready = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(packages.len());

    while let Some(name) = ready.pop_first() {
        ordered.push(name.clone());

        dependents
            .get(&name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .for_each(|dependent| {
                let count = indegree
                    .get_mut(&dependent)
                    .expect("dependent should have indegree entry");
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent);
                }
            });
    }

    if ordered.len() != packages.len() {
        let unresolved = indegree
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        return Err(PackageGraphError::DependencyCycle {
            packages: unresolved
                .into_iter()
                .map(|package| CycleBlockedPackage { package })
                .collect(),
        }
        .into());
    }

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
    fn install_tasks_release_by_kind_and_package_dependency() {
        let packages =
            required_packages(&[("dependency", ""), ("dependent", "Imports: dependency\n")]);
        let mut tasks = install_tasks(packages).unwrap();
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

    #[tokio::test]
    async fn publishes_complete_installed_package() {
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("package");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("DESCRIPTION"),
            "Package: package\nVersion: 1.0.0\n",
        )
        .unwrap();
        fs::write(source.join("contents"), "complete").unwrap();
        let project_library = tempfile::tempdir().unwrap();

        publish_installed_package(&source, project_library.path(), "package")
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(project_library.path().join("package/contents")).unwrap(),
            "complete"
        );
        assert!(fs::read_dir(project_library.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rpx-install-")
        }));
    }

    #[tokio::test]
    async fn package_publication_does_not_replace_existing_destination() {
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("package");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("DESCRIPTION"),
            "Package: package\nVersion: 2.0.0\n",
        )
        .unwrap();
        let project_library = tempfile::tempdir().unwrap();
        let existing = project_library.path().join("package");
        fs::create_dir(&existing).unwrap();
        fs::write(
            existing.join("DESCRIPTION"),
            "Package: package\nVersion: 1.0.0\n",
        )
        .unwrap();

        let error = publish_installed_package(&source, project_library.path(), "package")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            PublishInstalledPackageError::DestinationExists { path } if path == existing
        ));
        assert_eq!(
            fs::read_to_string(existing.join("DESCRIPTION")).unwrap(),
            "Package: package\nVersion: 1.0.0\n"
        );
    }

    #[tokio::test]
    async fn package_graph_validation_accepts_complete_acyclic_graph() {
        let packages = required_packages(&[
            ("dependent", "Imports: dependency, testBasePackage\n"),
            ("dependency", ""),
            ("unrelated", ""),
        ]);
        validate_package_graph(&packages).await.unwrap();
    }

    #[tokio::test]
    async fn package_graph_validation_deduplicates_dependency_constraints() {
        let packages = required_packages(&[
            (
                "dependent",
                "Imports: dependency (>= 1.0.0), dependency (< 2.0.0)\n",
            ),
            ("dependency", ""),
        ]);
        validate_package_graph(&packages).await.unwrap();
    }

    #[tokio::test]
    async fn package_graph_validation_reports_missing_dependencies() {
        let packages = required_packages(&[
            ("a", "Imports: missingB, missingA\n"),
            ("b", "Imports: missingA\n"),
        ]);
        let error = validate_package_graph(&packages).await.unwrap_err();
        let SyncError::PackageGraph(PackageGraphError::MissingDependencies { dependencies }) =
            error
        else {
            panic!("missing dependencies should produce a structured graph error");
        };
        assert_eq!(
            dependencies
                .iter()
                .map(|dependency| { (dependency.package.as_str(), dependency.dependency.as_str()) })
                .collect::<Vec<_>>(),
            vec![("a", "missingA"), ("a", "missingB"), ("b", "missingA")]
        );
    }

    #[tokio::test]
    async fn package_graph_validation_reports_all_blocked_names() {
        let packages = required_packages(&[
            ("a", "Imports: b\n"),
            ("b", "Imports: a\n"),
            ("c", "Imports: a\n"),
        ]);
        let error = validate_package_graph(&packages).await.unwrap_err();
        let SyncError::PackageGraph(PackageGraphError::DependencyCycle { packages }) = error else {
            panic!("cycles should produce a structured graph error");
        };
        assert_eq!(
            packages
                .iter()
                .map(|package| package.package.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[tokio::test]
    async fn package_graph_validation_names_invalid_dependency_metadata() {
        let packages = required_packages(&[("broken", "Imports: dependency (>= invalid)\n")]);
        let error = validate_package_graph(&packages).await.unwrap_err();
        assert!(matches!(
            error,
            SyncError::PackageGraph(PackageGraphError::InvalidDependencies { package, .. })
                if package == "broken"
        ));
    }
}

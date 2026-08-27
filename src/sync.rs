use crate::{
    cache::{self, CompiledPackageCacheKey, artifact_cache_path, build_temp_library_path},
    description::{DescriptionParseError, root_package},
    http,
    project::{
        Project, ProjectLibraryError, ProjectResolution, RequiredPackages, ensure_project_library,
    },
    r::{
        self, BasePackagesError, base_packages, install_local_package, install_package_directory,
        installed_packages, remove_packages_from_venv,
    },
    repository::{CranRepository, GitRepository, LocalRepository, RrepoRepository},
    resolver::PackageVersion,
    ui::{progress_bar_style, progress_spinner_style},
};
use futures_util::StreamExt;
use miette::Diagnostic;
use r_description::RDescription;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore, oneshot, watch},
};
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
    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageGraph(#[from] PackageGraphError),
    #[error("failed to install project package: {source}")]
    #[diagnostic(code(rpx::sync::project_install_failed))]
    ProjectPackageInstall {
        #[source]
        source: Box<r::PackageInstallError>,
    },
    #[error("failed to install package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_install_failed))]
    PackageInstall {
        package: String,
        #[source]
        source: Box<r::PackageInstallError>,
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

    if project_package == ProjectPackageMode::Install {
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
    let retained = installed
        .iter()
        .filter(|(name, installed_version)| {
            required.get(*name).is_some_and(|(required_version, _)| {
                !package_requires_install(required_version, Some(installed_version))
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
    install_required_packages(
        &project_library,
        packages_to_install,
        retained,
        &resolution.r_version,
    )
    .await?;

    Ok(SyncReport { installed_before })
}

fn package_requires_install(required: &PackageVersion, installed: Option<&PackageVersion>) -> bool {
    let repository = required.repository().as_ref();

    // Git and local sources can change without changing their package version.
    repository.downcast_ref::<GitRepository>().is_some()
        || repository.downcast_ref::<LocalRepository>().is_some()
        || installed != Some(required)
}

fn package_dependency_names(description: &RDescription) -> Result<BTreeSet<String>, String> {
    let depends = description.depends().map_err(|error| error.to_string())?;
    let imports = description.imports().map_err(|error| error.to_string())?;
    let linking_to = description
        .linking_to()
        .map_err(|error| error.to_string())?;

    Ok(depends
        .chain(imports)
        .chain(linking_to)
        .map(|relation| relation.package().to_string())
        .filter(|package| package != "R")
        .collect())
}

const SYNC_SHARED_WORKERS: usize = 50;
const SYNC_INSTALL_WORKERS: usize = 8;

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
    retained: BTreeSet<String>,
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

    let project_library = project_library.to_path_buf();
    let r_version = Arc::new(r_version.clone());
    let required_names = Arc::new(
        retained
            .iter()
            .cloned()
            .chain(packages.keys().cloned())
            .collect::<BTreeSet<_>>(),
    );
    let installed_packages = Arc::new(Mutex::new(retained));
    let install_failed = Arc::new(AtomicBool::new(false));
    let shared_pool = Arc::new(Semaphore::new(SYNC_SHARED_WORKERS));
    let install_pool = Arc::new(Semaphore::new(SYNC_INSTALL_WORKERS));
    let (installed_tx, installed_rx) = watch::channel(());
    let mut prepare_tasks = tokio::task::JoinSet::new();
    let mut install_tasks = tokio::task::JoinSet::new();
    let mut completed = 0_u64;

    for (package_name, (package_version, description)) in packages {
        let dependencies = package_dependency_names(&description)
            .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;
        if let Some(repository) = package_version
            .repository()
            .as_ref()
            .downcast_ref::<LocalRepository>()
        {
            let project_path = repository.path().to_path_buf();
            let install_required_names = Arc::clone(&required_names);
            let install_installed_packages = Arc::clone(&installed_packages);
            let install_install_failed = Arc::clone(&install_failed);
            let install_installed_rx = installed_rx.clone();
            let install_installed_tx = installed_tx.clone();
            let install_shared_pool = Arc::clone(&shared_pool);
            let install_pool = Arc::clone(&install_pool);
            let install_project_library = project_library.clone();
            install_tasks.spawn(
                async move {
                    wait_for_package_dependencies(
                        &package_name,
                        &dependencies,
                        install_required_names,
                        Arc::clone(&install_installed_packages),
                        install_install_failed,
                        install_installed_rx,
                    )
                    .await
                    .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                    let _install_permit = install_pool.acquire_owned().await.map_err(|_| {
                        SyncError::DownloadArtifactsFailed {
                            details: "install pool closed before project installation".to_string(),
                        }
                    })?;
                    let _shared_permit =
                        install_shared_pool.acquire_owned().await.map_err(|_| {
                            SyncError::DownloadArtifactsFailed {
                                details: "sync work pool closed before project installation"
                                    .to_string(),
                            }
                        })?;

                    install_package_directory(
                        &project_path,
                        &install_project_library,
                        &package_name,
                        package_version.version().as_ref(),
                        "project package",
                    )
                    .await
                    .map_err(|source| SyncError::ProjectPackageInstall {
                        source: Box::new(source),
                    })?;
                    {
                        let mut installed_packages = install_installed_packages.lock().await;
                        installed_packages.insert(package_name.clone());
                    }
                    let _ = install_installed_tx.send(());

                    Ok::<_, SyncError>(package_name)
                }
                .instrument(sync_span.clone()),
            );
            continue;
        }

        if let Some(repository) = package_version
            .repository()
            .as_ref()
            .downcast_ref::<GitRepository>()
        {
            let repository = repository.clone();
            let install_required_names = Arc::clone(&required_names);
            let install_installed_packages = Arc::clone(&installed_packages);
            let install_install_failed = Arc::clone(&install_failed);
            let install_installed_rx = installed_rx.clone();
            let install_installed_tx = installed_tx.clone();
            let install_shared_pool = Arc::clone(&shared_pool);
            let install_pool = Arc::clone(&install_pool);
            let install_project_library = project_library.clone();
            install_tasks.spawn(
                async move {
                    wait_for_package_dependencies(
                        &package_name,
                        &dependencies,
                        install_required_names,
                        Arc::clone(&install_installed_packages),
                        install_install_failed,
                        install_installed_rx,
                    )
                    .await
                    .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                    let _install_permit = install_pool.acquire_owned().await.map_err(|_| {
                        SyncError::DownloadArtifactsFailed {
                            details: "install pool closed before Git package installation"
                                .to_string(),
                        }
                    })?;
                    let _shared_permit =
                        install_shared_pool.acquire_owned().await.map_err(|_| {
                            SyncError::DownloadArtifactsFailed {
                                details: "sync work pool closed before Git package installation"
                                    .to_string(),
                            }
                        })?;

                    let checkout = repository.checkout().await.map_err(|error| {
                        SyncError::DownloadArtifactsFailed {
                            details: format!("failed to checkout {package_name}: {error}"),
                        }
                    })?;
                    let package_root = repository
                        .subdirectory()
                        .map_or(checkout.clone(), |subdirectory| checkout.join(subdirectory));
                    install_package_directory(
                        &package_root,
                        &install_project_library,
                        &package_name,
                        package_version.version().as_ref(),
                        &format!("{package_name} from Git"),
                    )
                    .await
                    .map_err(|source| SyncError::PackageInstall {
                        package: package_name.clone(),
                        source: Box::new(source),
                    })?;
                    {
                        let mut installed_packages = install_installed_packages.lock().await;
                        installed_packages.insert(package_name.clone());
                    }
                    let _ = install_installed_tx.send(());

                    Ok::<_, SyncError>(package_name)
                }
                .instrument(sync_span.clone()),
            );
            continue;
        }

        let cache_key = CompiledPackageCacheKey::new(
            &package_name,
            package_version.version().as_ref(),
            r_version.as_ref(),
        );
        let (prepared_tx, prepared_rx) = oneshot::channel();

        let prepare_package_name = package_name.clone();
        let prepare_package_version = package_version.clone();
        let prepare_cache_key = cache_key.clone();
        let prepare_r_version = Arc::clone(&r_version);
        let prepare_shared_pool = Arc::clone(&shared_pool);
        prepare_tasks.spawn(
            async move {
                let prepared = match prepare_shared_pool.acquire_owned().await {
                    Ok(_permit) => {
                        prepare_locked_package_artifact(
                            prepare_package_name,
                            prepare_package_version,
                            prepare_cache_key,
                            prepare_r_version,
                        )
                        .await
                    }
                    Err(_) => Err("sync work pool closed before artifact preparation".to_string()),
                };

                let _ = prepared_tx.send(prepared);
            }
            .instrument(sync_span.clone()),
        );

        let install_required_names = Arc::clone(&required_names);
        let install_installed_packages = Arc::clone(&installed_packages);
        let install_install_failed = Arc::clone(&install_failed);
        let install_installed_rx = installed_rx.clone();
        let install_installed_tx = installed_tx.clone();
        let install_shared_pool = Arc::clone(&shared_pool);
        let install_pool = Arc::clone(&install_pool);
        let install_project_library = project_library.clone();
        install_tasks.spawn(
            async move {
                let prepared_artifact = prepared_rx
                    .await
                    .map_err(|_| SyncError::DownloadArtifactsFailed {
                        details: format!(
                            "{package_name} artifact preparation task ended without a result"
                        ),
                    })?
                    .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                // Keep package spans out of the progress UI while blocked on dependency installs.
                wait_for_package_dependencies(
                    &package_name,
                    &dependencies,
                    install_required_names,
                    Arc::clone(&install_installed_packages),
                    install_install_failed,
                    install_installed_rx,
                )
                .await
                .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;

                let _install_permit = install_pool.acquire_owned().await.map_err(|_| {
                    SyncError::DownloadArtifactsFailed {
                        details: "install pool closed before package installation".to_string(),
                    }
                })?;
                let _shared_permit = install_shared_pool.acquire_owned().await.map_err(|_| {
                    SyncError::DownloadArtifactsFailed {
                        details: "sync work pool closed before package installation".to_string(),
                    }
                })?;

                let installed = install_prepared_package(
                    install_project_library,
                    package_name,
                    package_version,
                    cache_key,
                    prepared_artifact,
                )
                .await
                .map_err(|details| SyncError::DownloadArtifactsFailed { details })?;
                {
                    let mut installed_packages = install_installed_packages.lock().await;
                    installed_packages.insert(installed.clone());
                }
                let _ = install_installed_tx.send(());

                Ok::<_, SyncError>(installed)
            }
            .instrument(sync_span.clone()),
        );
    }

    sync_span.record("running", install_tasks.len() as u64);

    let mut first_error = None;
    while let Some(result) = install_tasks.join_next().await {
        let result = result
            .map_err(|error| SyncError::DownloadArtifactsFailed {
                details: format!("install task failed to join: {error}"),
            })
            .and_then(|result| result);

        match result {
            Ok(_) => completed += 1,
            Err(error) if first_error.is_none() => {
                first_error = Some(error);
                install_failed.store(true, Ordering::Relaxed);
                install_pool.close();
                shared_pool.close();
                prepare_tasks.abort_all();
                let _ = installed_tx.send(());
            }
            Err(_) => {}
        }

        sync_span.record("completed", completed);
        sync_span.record("running", install_tasks.len() as u64);
        sync_span.record("pending", total_packages.saturating_sub(completed));
        sync_span.pb_set_position(completed);
        sync_span.pb_set_message(&format!("sync packages {completed}/{total_packages}"));
    }

    drop(prepare_tasks);

    sync_span.record("stage", "done");
    sync_span.pb_set_finish_message(&format!("sync packages {completed}/{total_packages}"));
    first_error.map_or(Ok(()), Err)
}

async fn wait_for_package_dependencies(
    package: &str,
    dependencies: &BTreeSet<String>,
    required_names: Arc<BTreeSet<String>>,
    installed_packages: Arc<Mutex<BTreeSet<String>>>,
    install_failed: Arc<AtomicBool>,
    mut installed_rx: watch::Receiver<()>,
) -> Result<(), String> {
    loop {
        if install_failed.load(Ordering::Relaxed) {
            return Err(format!(
                "stopped waiting for {package} dependencies after an installation failed"
            ));
        }

        {
            let installed_packages = installed_packages.lock().await;
            if dependencies
                .iter()
                .filter(|dependency| required_names.contains(*dependency))
                .all(|dependency| installed_packages.contains(dependency))
            {
                return Ok(());
            }
        }

        installed_rx.changed().await.map_err(|_| {
            format!(
                "dependency notifier closed before {} dependencies were installed",
                package
            )
        })?;
    }
}

async fn prepare_locked_package_artifact(
    package: String,
    package_version: PackageVersion,
    cache_key: CompiledPackageCacheKey,
    r_version: Arc<semver::Version>,
) -> Result<Option<(PathBuf, String)>, String> {
    let version = package_version.version().to_string();
    let span = tracing::info_span!(
        "prepare_package",
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
    span.pb_set_message(&package_stage_message(&package, &version, "preparing"));
    span.pb_start();

    prepare_locked_package_artifact_inner(
        package,
        package_version,
        &cache_key,
        r_version.as_ref(),
        span.clone(),
    )
    .instrument(span)
    .await
}

async fn prepare_locked_package_artifact_inner(
    package: String,
    package_version: PackageVersion,
    cache_key: &CompiledPackageCacheKey,
    r_version: &semver::Version,
    span: tracing::Span,
) -> Result<Option<(PathBuf, String)>, String> {
    fn response_for_status(response: reqwest::Response) -> Result<reqwest::Response, String> {
        response
            .error_for_status()
            .map_err(|error| error.to_string())
    }

    let version = package_version.version().to_string();
    record_package_stage(&span, &package, &version, "checking cache");
    if cache::exists(cache_key).await {
        record_package_stage(&span, &package, &version, "cached");
        return Ok(None);
    }

    let repository = package_version.repository();
    let (base_url, is_rrepo) =
        if let Some(repository) = repository.as_ref().downcast_ref::<RrepoRepository>() {
            (repository.url(), true)
        } else if let Some(repository) = repository.as_ref().downcast_ref::<CranRepository>() {
            (repository.url(), false)
        } else {
            return Err(format!(
                "package {package} uses an unsupported remote repository"
            ));
        };
    span.record("repository", base_url.as_str());

    record_package_stage(&span, &package, &version, "downloading binary");

    let binary = match (std::env::consts::OS, is_rrepo) {
        ("windows", true) => http::rrepo_windows_binary(base_url, &package, &version, r_version)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("windows", false) => http::cran_windows_binary(base_url, r_version, &package, &version)
            .await
            .map_err(|error| error.to_string())
            .and_then(response_for_status)
            .map(|response| (response, "zip", "win.binary".to_string())),

        ("macos", true) => {
            let target = macos_binary_target()?;

            http::rrepo_macos_binary(base_url, &package, &version, &target, r_version)
                .await
                .map_err(|error| error.to_string())
                .and_then(response_for_status)
                .map(|response| (response, "tgz", format!("mac.binary.{target}")))
        }

        ("macos", false) => {
            let target = macos_binary_target()?;

            http::cran_macos_binary(base_url, &target, r_version, &package, &version)
                .await
                .map_err(|error| error.to_string())
                .and_then(response_for_status)
                .map(|response| (response, "tgz", format!("mac.binary.{target}")))
        }

        _ => Err(format!(
            "binary artifacts are not supported on {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    };

    let (response, extension, install_type) = match binary {
        Ok(binary) => {
            span.record("artifact_kind", binary.2.as_str());
            binary
        }

        Err(error) => {
            tracing::debug!(
                package = %package,
                version = %version,
                error = %error,
                "binary artifact unavailable; falling back to source"
            );

            record_package_stage(&span, &package, &version, "falling back to source");
            record_package_stage(&span, &package, &version, "downloading source");

            let response = if is_rrepo {
                http::rrepo_source_artifact(base_url, &package, &version)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(response_for_status)?
            } else {
                let current = http::cran_current_source_tarball(base_url, &package, &version)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(response_for_status);
                match current {
                    Ok(response) => response,
                    Err(_) => http::cran_archive_source_tarball(base_url, &package, &version)
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(response_for_status)?,
                }
            };

            span.record("artifact_kind", "source");

            (response, "tar.gz", "source".to_string())
        }
    };

    let artifact_path =
        write_artifact_response(&package, &version, extension, response, &span).await?;

    record_package_stage(&span, &package, &version, "prepared");

    Ok(Some((artifact_path, install_type)))
}

async fn install_prepared_package(
    project_library: PathBuf,
    package: String,
    package_version: PackageVersion,
    cache_key: CompiledPackageCacheKey,
    prepared_artifact: Option<(PathBuf, String)>,
) -> Result<String, String> {
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
    span.pb_set_message(&package_stage_message(&package, &version, "installing"));
    span.pb_start();

    install_prepared_package_inner(
        project_library,
        package,
        version,
        cache_key,
        prepared_artifact,
        span.clone(),
    )
    .instrument(span)
    .await
}

async fn install_prepared_package_inner(
    project_library: PathBuf,
    package: String,
    version: String,
    cache_key: CompiledPackageCacheKey,
    prepared_artifact: Option<(PathBuf, String)>,
    span: tracing::Span,
) -> Result<String, String> {
    match prepared_artifact {
        None => {
            span.record("artifact_kind", "compiled-cache");
            record_package_stage(&span, &package, &version, "restoring from cache");
            cache::restore(&cache_key, &project_library).await?;
            record_package_stage(&span, &package, &version, "restored from cache");
            Ok(package)
        }

        Some((artifact_path, install_type)) => {
            span.record("artifact_kind", install_type.as_str());
            install_downloaded_package(
                package,
                version,
                cache_key,
                artifact_path,
                install_type,
                project_library,
                span,
            )
            .await
        }
    }
}

async fn install_downloaded_package(
    package: String,
    version: String,
    key: CompiledPackageCacheKey,
    artifact_path: PathBuf,
    install_type: String,
    project_library: PathBuf,
    span: tracing::Span,
) -> Result<String, String> {
    record_package_stage(&span, &package, &version, "installing");

    let temp_library = build_temp_library_path(&package, &unique_build_token());

    install_local_package(
        &project_library,
        &artifact_path,
        &package,
        &version,
        &install_type,
        &temp_library,
    )
    .await
    .map_err(|failure| failure.to_string())?;

    let built_package_path = temp_library.join(&package);

    record_package_stage(&span, &package, &version, "storing cache");
    cache::store(&key, &built_package_path).await?;

    record_package_stage(&span, &package, &version, "restoring project library");
    cache::restore(&key, &project_library).await?;

    record_package_stage(&span, &package, &version, "cleaning up");
    if let Some(temp_root) = temp_library.parent() {
        tokio::fs::remove_dir_all(temp_root)
            .await
            .map_err(|error| format!("failed to clean temporary build directory: {error}"))?;
    }

    record_package_stage(&span, &package, &version, "done");

    Ok(package)
}

fn record_package_stage(span: &tracing::Span, package: &str, version: &str, stage: &'static str) {
    span.record("stage", stage);
    span.pb_set_style(&progress_spinner_style());
    span.pb_set_message(&package_stage_message(package, version, stage));
    span.pb_tick();
}

fn package_stage_message(package: &str, version: &str, stage: &str) -> String {
    format!("{package} {version} {stage}")
}

async fn write_artifact_response(
    package: &str,
    version: &str,
    extension: &str,
    response: reqwest::Response,
    span: &tracing::Span,
) -> Result<PathBuf, String> {
    let file_name = format!("{package}_{version}.{extension}");
    let path = artifact_cache_path(package, version, &file_name);

    if path.exists() {
        if let Ok(metadata) = path.metadata() {
            span.record("bytes", metadata.len());
            span.record("total_bytes", metadata.len());
            span.pb_set_style(&progress_bar_style());
            span.pb_set_length(metadata.len());
            span.pb_set_position(metadata.len());
            span.pb_set_message(&package_stage_message(
                package,
                version,
                "using cached artifact",
            ));
        }

        return Ok(path);
    }

    let content_length = response.content_length();

    if let Some(total) = content_length {
        span.record("total_bytes", total);
        span.pb_set_style(&progress_bar_style());
        span.pb_set_length(total);
        span.pb_set_position(0);
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            format!(
                "failed to create artifact cache directory {}: {error}",
                parent.display()
            )
        })?;
    }

    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| format!("failed to create artifact file {}: {error}", path.display()))?;

    let mut stream = response.bytes_stream();
    let mut written = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("failed to read artifact response: {error}"))?;
        let chunk_len = chunk.len() as u64;

        file.write_all(&chunk).await.map_err(|error| {
            format!("failed to write artifact file {}: {error}", path.display())
        })?;

        written += chunk_len;

        span.record("bytes", written);

        if content_length.is_some() {
            span.pb_inc(chunk_len);
        } else {
            span.pb_tick();
        }
    }

    file.flush()
        .await
        .map_err(|error| format!("failed to flush artifact file {}: {error}", path.display()))?;

    Ok(path)
}

fn macos_binary_target() -> Result<String, String> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("big-sur-arm64".to_string()),
        "x86_64" => Ok("big-sur-x86_64".to_string()),
        arch => Err(format!(
            "unsupported macOS architecture for binary packages: {arch}"
        )),
    }
}

fn unique_build_token() -> String {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("{}-{unique}", std::process::id())
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
        .map(|(package, (_, description))| {
            package_dependency_names(description)
                .map(|dependencies| {
                    dependencies
                        .into_iter()
                        .map(|dependency| (package.clone(), dependency))
                        .collect::<Vec<_>>()
                })
                .map_err(|details| PackageGraphError::InvalidDependencies {
                    package: package.clone(),
                    details,
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
    use r_description::Remote;

    #[tokio::test]
    async fn dependency_wait_stops_after_install_failure() {
        let install_failed = Arc::new(AtomicBool::new(false));
        let (installed_tx, installed_rx) = watch::channel(());
        let wait_install_failed = Arc::clone(&install_failed);
        let waiter = tokio::spawn(async move {
            wait_for_package_dependencies(
                "dependent",
                &BTreeSet::from(["dependency".to_string()]),
                Arc::new(BTreeSet::from(["dependency".to_string()])),
                Arc::new(Mutex::new(BTreeSet::new())),
                wait_install_failed,
                installed_rx,
            )
            .await
        });

        tokio::task::yield_now().await;
        install_failed.store(true, Ordering::Relaxed);
        let _ = installed_tx.send(());

        let error = waiter
            .await
            .expect("dependency waiter should join")
            .expect_err("dependency waiter should stop");
        assert!(error.contains("after an installation failed"));
    }

    #[test]
    fn package_dependency_names_uses_only_hard_dependencies() {
        let description = RDescription::parse(
            "Package: selected\nVersion: 1.0.0\nDepends: R, depends, duplicate\nImports: imports, duplicate\nLinkingTo: linking\nSuggests: suggested\n",
        );
        assert_eq!(
            package_dependency_names(&description).unwrap(),
            BTreeSet::from([
                "depends".into(),
                "duplicate".into(),
                "imports".into(),
                "linking".into()
            ])
        );
        for field in ["Depends", "Imports", "LinkingTo"] {
            let description = RDescription::parse(&format!(
                "Package: selected\nVersion: 1.0.0\n{field}: broken (>= invalid)\n"
            ));
            assert!(package_dependency_names(&description).is_err());
        }
    }

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
                    RDescription::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}"));
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

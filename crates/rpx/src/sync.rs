mod artifact;
mod plan;
use artifact::{ArtifactKind, PreparedArtifact};

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
    repository::{CranRepository, GitRepository, LocalRepository, RrepoRepository},
    resolver::PackageVersion,
    ui::{progress_bar_style, progress_count_style, progress_spinner_style},
};
use futures_util::StreamExt;
use miette::Diagnostic;
use r_package_installer::{
    BinaryFormat, CacheKey, Digest as InstallerDigest, ExpectedPackage, InstallOutcome, Installer,
    PrepareRequest, RemovalOutcome,
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
    #[error("sync task engine failed: {details}")]
    TaskEngine { details: String },
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
            use rpx_task::ExecutionEvent;
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
        .map_err(|error| match error {
            rpx_task::ExecutionError::Operation { source, .. } => source,
            other => SyncError::TaskEngine {
                details: other.to_string(),
            },
        });

    sync_span.record("completed", completed);
    sync_span.record("running", 0_u64);
    sync_span.record("pending", total_packages - completed);
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
    span: tracing::Span,
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

async fn remove_package(package: String, context: SyncTaskContext) -> Result<(), SyncError> {
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
) -> Result<PreparedArtifact, DownloadPackageArtifactError> {
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
        match registry_binary_location(&repository, &package, &package_version, r_version.as_ref()) {
            Ok(Some((path, format))) => {
                let binary_result = async {
                    match artifact_cache_entry(&path) {
                        ArtifactCacheEntry::File => return Ok(()),
                        ArtifactCacheEntry::Invalid => {
                            return Err(DownloadPackageArtifactError::InvalidArtifact {
                                path: path.clone(),
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
                    publish_artifact_response(path.clone(), response, &span).await
                }
                .await;

                match binary_result {
                    Ok(()) => {
                        span.record("stage", "prepared");
                        span.pb_set_message(&format!("{package} {version} prepared"));
                        return Ok(PreparedArtifact::Binary { path, format });
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
        let path = registry_source_path(&repository, &package, &package_version);
        match artifact_cache_entry(&path) {
            ArtifactCacheEntry::File => {
                span.record("stage", "prepared");
                span.pb_set_message(&format!("{package} {version} prepared"));
                return Ok(PreparedArtifact::Source { path });
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
        publish_artifact_response(path.clone(), response, &span).await?;
        span.record("stage", "prepared");
        span.pb_set_message(&format!("{package} {version} prepared"));
        Ok(PreparedArtifact::Source { path })
    }
    .instrument(span.clone())
    .await
}

#[derive(Debug, Error)]
pub(crate) enum InstallPackageError {
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

fn registry_binary_location(
    registry: &RegistryIdentity,
    package: &str,
    package_version: &PackageVersion,
    r_version: &semver::Version,
) -> Result<Option<(PathBuf, BinaryFormat)>, http::BinaryArtifactRequestError> {
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
    Ok(Some((path, format)))
}

fn registry_source_path(
    registry: &RegistryIdentity,
    package: &str,
    package_version: &PackageVersion,
) -> PathBuf {
    source_artifact_cache_path(&SourceArtifactCacheKey::new(
        SourceArtifactIdentity::Registry(registry.clone()),
        package,
        package_version.version().clone(),
    ))
}

async fn install_package(
    installer: &Installer,
    project_library: &Path,
    package: &str,
    package_version: &r_metadata::Version,
    r_version: &semver::Version,
    dependencies: &[DependencyInput],
    artifact: Arc<PreparedArtifact>,
) -> Result<(), InstallPackageError> {
    let version = package_version.to_string();
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
        span.record("artifact_kind", artifact.trace_kind());

        let dependency_inputs = dependencies
            .iter()
            .map(|dependency| (dependency.name.clone(), dependency.version.clone()))
            .collect::<Vec<_>>();
        let prepare_installer = installer.clone();
        let artifact_path = artifact.path().to_path_buf();
        let artifact_kind = artifact.kind();
        let project_library_for_prepare = project_library.to_path_buf();
        let package_for_prepare = package.to_string();
        let version_for_prepare = version.clone();
        let r_version_for_prepare = r_version.clone();

        span.record("stage", "preparing cache");
        let installation_message = format!(
            "{package} {version} {}",
            artifact.installation_action()
        );
        span.pb_set_message(&installation_message);
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
            let artifact = artifact.to_installer_artifact(project_library_for_prepare);
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
        span.pb_set_message(&installation_message);
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
    use r_package_installer::Artifact;

    #[test]
    fn install_artifact_actions_use_r_installation_terms() {
        let binary = PreparedArtifact::Binary {
            path: PathBuf::new(),
            format: BinaryFormat::Zip,
        };
        let source = PreparedArtifact::Source {
            path: PathBuf::new(),
        };

        assert_eq!(binary.installation_action(), "installing binary");
        assert_eq!(source.installation_action(), "installing source");
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
    fn sync_plan_rejects_cycles_before_executing_operations() {
        let packages = required_packages(&[
            ("a", "Imports: b\n"),
            ("b", "Imports: a\n"),
            ("blocked", "Imports: b\n"),
        ]);
        let result = plan::sync_plan(&packages, &BTreeMap::new(), test_context());
        let Err(SyncError::DependencyCycle(error)) = result else {
            panic!("expected cycle error")
        };
        assert_eq!(
            error
                .packages
                .iter()
                .map(|p| p.package.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "blocked"]
        );
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

        let plan = plan::sync_plan(&required, &installed, test_context()).unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.install_count, 0);
        assert!(
            plan.tasks
                .values()
                .any(|task| task == &("extra".to_string(), TaskKind::Remove))
        );
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

    #[tokio::test]
    async fn failed_publication_cleans_up_the_temporary_download() {
        let mut server = mockito::Server::new_async().await;
        let response = server
            .mock("GET", "/archive")
            .with_status(200)
            .with_body("artifact bytes")
            .create_async()
            .await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing-directory");
        fs::create_dir(&destination).unwrap();
        let body = reqwest::get(format!("{}/archive", server.url()))
            .await
            .unwrap();
        let error = publish_artifact_response(destination.clone(), body, &tracing::Span::none())
            .await
            .unwrap_err();
        assert!(
            matches!(error, DownloadPackageArtifactError::PublishArtifact { path, .. } if path == destination)
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
        response.assert_async().await;
    }

    #[tokio::test]
    async fn installation_reports_the_producer_supplied_path_if_it_disappears() {
        let directory = tempfile::tempdir().unwrap();
        let selected_path = directory.path().join("selected-source.tar.gz");
        // Another file must not become an implicit replacement for this result.
        fs::write(directory.path().join("other-binary.zip"), "other artifact").unwrap();
        let error = install_package(
            &Installer::new(directory.path().join("installer")),
            &directory.path().join("library"),
            "example",
            &"1.0.0".parse().unwrap(),
            &semver::Version::new(4, 5, 0),
            &[],
            Arc::new(PreparedArtifact::Source {
                path: selected_path.clone(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, InstallPackageError::ArtifactDigest { path, .. } if path == selected_path)
        );
    }

    #[test]
    fn install_artifact_maps_source_options_for_the_installer() {
        let artifact_path = PathBuf::from("package.tar.gz");
        let project_library = PathBuf::from("project-library");
        let artifact = PreparedArtifact::Source {
            path: artifact_path.clone(),
        }
        .to_installer_artifact(project_library.clone());

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
        let artifact = PreparedArtifact::Binary {
            path: artifact_path.clone(),
            format: BinaryFormat::Zip,
        }
        .to_installer_artifact(PathBuf::from("unused-library"));

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

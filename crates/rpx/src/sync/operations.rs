//! Package operations and their native inputs, outputs, and failures.
//! These functions do not depend on the graph runner or sync orchestration.
use crate::{
    cache::{
        BinaryArtifactCacheKey, INSTALLER_CACHE_VERSION, RegistryCacheKey, SourceArtifactCacheKey,
        SourceArtifactIdentity, binary_artifact_cache_path, source_artifact_cache_path,
    },
    http,
    r::{self, build_package_archive},
    repository::{GitRepository, PackageRepository, RepositoryError},
    resolver::PackageVersion,
    ui::{progress_bar_style, progress_spinner_style},
};
use futures_util::StreamExt;
use miette::Diagnostic;
use r_metadata::Version;
use r_package_installer::{
    Artifact, BinaryArtifact, BinaryFormat, CacheKey, Digest as InstallerDigest, ExpectedPackage,
    InstallOutcome, Installer, PrepareRequest, RemovalOutcome, SourceArtifact, SourceOptions,
};
use sha2::{Digest as _, Sha256};
use std::{
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

/// The result of successful artifact acquisition. Producers publish this value
/// only after building, downloading, or finding the file in the cache.
#[derive(Debug)]
pub(super) enum PreparedArtifact {
    Binary { path: PathBuf, format: BinaryFormat },
    Source { path: PathBuf },
}

impl PreparedArtifact {
    fn path(&self) -> &Path {
        match self {
            Self::Binary { path, .. } | Self::Source { path } => path,
        }
    }

    fn trace_kind(&self) -> &'static str {
        match self {
            Self::Binary { .. } => "binary",
            Self::Source { .. } => "source",
        }
    }

    fn installation_action(&self) -> &'static str {
        match self {
            Self::Binary { .. } => "installing binary",
            Self::Source { .. } => "installing source",
        }
    }

    fn to_installer_artifact(&self, project_library: PathBuf) -> Artifact {
        match self {
            Self::Binary { path, format } => Artifact::Binary(BinaryArtifact {
                path: path.clone(),
                format: *format,
            }),
            Self::Source { path } => Artifact::Source(SourceArtifact {
                path: path.clone(),
                options: SourceOptions {
                    dependency_libraries: vec![project_library],
                    allow_non_staged: true,
                    ..SourceOptions::default()
                },
            }),
        }
    }
}

#[derive(Debug)]
pub(super) struct BuildInput {
    package_root: PathBuf,
    archive_path: PathBuf,
    package: String,
    version: Version,
}

impl BuildInput {
    pub fn local(package_root: PathBuf, package: String, version: Version) -> Self {
        let archive_path = source_artifact_cache_path(&SourceArtifactCacheKey::new(
            SourceArtifactIdentity::Local(package_root.clone()),
            &package,
            version.clone(),
        ));
        Self {
            package_root,
            archive_path,
            package,
            version,
        }
    }
}

pub(super) async fn build(
    input: Arc<BuildInput>,
) -> Result<PreparedArtifact, r::PackageBuildError> {
    build_package_archive(
        &input.package_root,
        &input.package,
        input.version.as_ref(),
        &input.archive_path,
    )
    .await?;
    Ok(PreparedArtifact::Source {
        path: input.archive_path.clone(),
    })
}

#[derive(Debug, Error)]
pub(crate) enum CheckoutError {
    #[error("failed to resolve commit for {repository}: {source}")]
    ResolveCommit {
        repository: String,
        #[source]
        source: RepositoryError,
    },
    #[error("failed to check out {repository}: {source}")]
    Checkout {
        repository: String,
        #[source]
        source: RepositoryError,
    },
}

pub(super) async fn checkout(
    repository: GitRepository,
    package: String,
    version: Version,
) -> Result<BuildInput, CheckoutError> {
    let commit = repository
        .commit()
        .await
        .map_err(|source| CheckoutError::ResolveCommit {
            repository: repository.to_string(),
            source,
        })?;
    let checkout = repository
        .checkout()
        .await
        .map_err(|source| CheckoutError::Checkout {
            repository: repository.to_string(),
            source,
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

/// Common error at the graph boundary. The operations themselves return narrower
/// errors; registration supplies package context and the user-facing diagnostic.
#[derive(Debug, Error, Diagnostic)]
pub(crate) enum OperationError {
    #[error("failed to remove package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_remove_failed))]
    Remove {
        package: String,
        #[source]
        source: RemovePackageError,
    },
    #[error("failed to check out package {package} {version}: {source}")]
    #[diagnostic(code(rpx::sync::package_checkout_failed))]
    Checkout {
        package: String,
        version: String,
        #[source]
        source: CheckoutError,
    },
    #[error("failed to download artifact for {package} {version}: {source}")]
    #[diagnostic(code(rpx::sync::package_artifact_download_failed))]
    Download {
        package: String,
        version: String,
        #[source]
        source: DownloadPackageArtifactError,
    },
    #[error("failed to build package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_build_failed))]
    Build {
        package: String,
        #[source]
        source: Box<r::PackageBuildError>,
    },
    #[error("failed to install package {package}: {source}")]
    #[diagnostic(code(rpx::sync::package_install_failed))]
    Install {
        package: String,
        #[source]
        source: Box<InstallPackageError>,
    },
}

#[derive(Clone)]
pub(super) struct DependencyInput {
    pub name: String,
    pub version: Option<Version>,
}

#[derive(Debug, Error)]
pub(crate) enum RemovePackageError {
    #[error("package removal from {} failed: {source}", library.display())]
    Installer {
        library: PathBuf,
        #[source]
        source: r_package_installer::Error,
    },
    #[error("failed to join package removal task: {source}")]
    Join {
        #[source]
        source: tokio::task::JoinError,
    },
}

pub(super) async fn remove_package(
    installer: Installer,
    project_library: PathBuf,
    package: String,
) -> Result<(), RemovePackageError> {
    let library = project_library.clone();
    let package_for_remove = package.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        installer.remove(&project_library, &package_for_remove)
    })
    .await
    .map_err(|source| RemovePackageError::Join { source })?
    .map_err(|source| RemovePackageError::Installer { library, source })?;
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

pub(super) async fn download_package_artifact(
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
        let registry = RegistryCacheKey::from_repository(package_version.repository())
            .ok_or(DownloadPackageArtifactError::UnsupportedRepository)?;
        let repository = package_version.repository();
        span.record(
            "repository",
            repository.to_string(),
        );
        span.record("stage", "downloading binary");
        span.pb_set_message(&format!("{package} {version} downloading binary"));
        match registry_binary_location(&registry, &package, &package_version, r_version.as_ref()) {
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
                    let response = match repository {
                        PackageRepository::Rrepo(repo) => repo.binary(&package, package_version.version(), &HOST, &r_version).await,
                        PackageRepository::Cran(repo) => repo.binary(&package, package_version.version(), &HOST, &r_version).await,
                        PackageRepository::Git(_) | PackageRepository::Local(_) => return Err(DownloadPackageArtifactError::UnsupportedRepository),
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
        let path = registry_source_path(&registry, &package, &package_version);
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

        let response = match repository {
            PackageRepository::Rrepo(repo) => repo.source(&package, package_version.version())
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
            PackageRepository::Cran(repo) => {
                let current = repo.current_source(&package, package_version.version())
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
                        repo.archive_source(&package, package_version.version())
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
            },
            PackageRepository::Git(_) | PackageRepository::Local(_) => return Err(DownloadPackageArtifactError::UnsupportedRepository),
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
    #[error("failed to prepare package for installation: {source}")]
    Prepare {
        #[source]
        source: r_package_installer::Error,
    },
    #[error("failed to materialize package in the project library: {source}")]
    Materialize {
        #[source]
        source: r_package_installer::Error,
    },
    #[error("failed to join package preparation task: {source}")]
    PrepareJoin {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("failed to join package materialization task: {source}")]
    MaterializeJoin {
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

fn registry_binary_location(
    registry: &RegistryCacheKey,
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
    registry: &RegistryCacheKey,
    package: &str,
    package_version: &PackageVersion,
) -> PathBuf {
    source_artifact_cache_path(&SourceArtifactCacheKey::new(
        SourceArtifactIdentity::Registry(registry.clone()),
        package,
        package_version.version().clone(),
    ))
}

pub(super) async fn install_package(
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
            .map(|dependency| (dependency.name.clone(), dependency.version.as_ref().map(ToString::to_string)))
            .collect::<Vec<_>>();
        let prepare_installer = installer.clone();
        let artifact_path = artifact.path().to_path_buf();
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
                &artifact,
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
                .map_err(|source| InstallPackageError::Prepare { source })
        })
        .await
        .map_err(|source| InstallPackageError::PrepareJoin { source })??;

        span.record("stage", "updating project library");
        span.pb_set_message(&installation_message);
        let installer = installer.clone();
        let project_library = project_library.to_path_buf();
        let outcome =
            tokio::task::spawn_blocking(move || installer.materialize(&entry, &project_library))
                .await
                .map_err(|source| InstallPackageError::MaterializeJoin { source })?
                .map_err(|source| InstallPackageError::Materialize { source })?;
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
    artifact: &PreparedArtifact,
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
    field(match artifact {
        PreparedArtifact::Binary {
            format: BinaryFormat::Zip,
            ..
        } => b"binary-zip",
        PreparedArtifact::Binary {
            format: BinaryFormat::TarGz,
            ..
        } => b"binary-tar-gz",
        PreparedArtifact::Source { .. } => b"source",
    });
    field(artifact_digest.as_bytes());
    field(package.as_bytes());
    field(version.as_bytes());
    field(r_version.to_string().as_bytes());
    field(HOST.to_string().as_bytes());
    if matches!(artifact, PreparedArtifact::Source { .. }) {
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
    use std::collections::BTreeSet;

    #[tokio::test]
    async fn checkout_output_carries_the_pinned_tree_and_archive_destination() {
        use crate::git::{
            GitUrl,
            tests::{commit_file, source_repository},
        };
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

    #[tokio::test]
    async fn checkout_errors_preserve_repository_causes_and_identify_the_stage() {
        use crate::git::GitUrl;
        use std::error::Error;
        let directory = tempfile::tempdir().unwrap();
        let remote = GitUrl::from_local_path(&directory.path().join("missing-repository"));
        let unpinned = GitRepository::from_parts(remote.clone(), None, None);
        let error = checkout(unpinned, "example".into(), "1.0.0".parse().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, CheckoutError::ResolveCommit { .. }));
        assert!(error.source().unwrap().is::<RepositoryError>());
        let pinned = GitRepository::from_parts(remote, None, None)
            .with_commit("1111111111111111111111111111111111111111".parse().unwrap());
        let error = checkout(pinned, "example".into(), "1.0.0".parse().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, CheckoutError::Checkout { .. }));
        assert!(error.source().unwrap().is::<RepositoryError>());
    }

    #[tokio::test]
    async fn build_returns_the_native_build_error() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        fs::write(&file, "occupied").unwrap();
        let input = Arc::new(BuildInput {
            package_root: directory.path().to_path_buf(),
            archive_path: file.join("archive.tar.gz"),
            package: "example".into(),
            version: "1.0.0".parse().unwrap(),
        });
        let error = build(input).await.unwrap_err();
        assert!(matches!(
            error,
            r::PackageBuildError::ArtifactDirectory { .. }
        ));
    }

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
            artifact: &PreparedArtifact,
            digest: u8,
            package: &str,
            version: &str,
            r_version: &str,
            dependencies: &[(String, Option<String>)],
        ) -> String {
            installer_build_key(
                artifact,
                InstallerDigest::from_bytes([digest; 32]),
                package,
                version,
                &semver::Version::parse(r_version).unwrap(),
                dependencies,
            )
            .to_string()
        }
        let dependencies = vec![("dependency".into(), Some("1.0.0".into()))];
        let source = PreparedArtifact::Source {
            path: PathBuf::from("source.tar.gz"),
        };
        let baseline = key(&source, 1, "package", "1.0.0", "4.5.1", &dependencies);

        assert_ne!(
            baseline,
            key(
                &PreparedArtifact::Binary {
                    path: PathBuf::from("binary.zip"),
                    format: BinaryFormat::Zip
                },
                1,
                "package",
                "1.0.0",
                "4.5.1",
                &dependencies
            )
        );
        assert_ne!(
            baseline,
            key(&source, 2, "package", "1.0.0", "4.5.1", &dependencies)
        );
        assert_ne!(
            baseline,
            key(&source, 1, "other", "1.0.0", "4.5.1", &dependencies)
        );
        assert_ne!(
            baseline,
            key(&source, 1, "package", "2.0.0", "4.5.1", &dependencies)
        );
        assert_ne!(
            baseline,
            key(&source, 1, "package", "1.0.0", "4.4.2", &dependencies)
        );
        assert_ne!(
            baseline,
            key(
                &source,
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
                &source,
                1,
                "package",
                "1.0.0",
                "4.5.1",
                &[("dependency".into(), Some("2.0.0".into()))]
            )
        );
    }

    #[test]
    fn installer_build_key_ignores_location_but_distinguishes_artifact_formats() {
        let key = |artifact: &PreparedArtifact| {
            installer_build_key(
                artifact,
                InstallerDigest::from_bytes([1; 32]),
                "package",
                "1.0.0",
                &semver::Version::new(4, 5, 1),
                &[],
            )
            .to_string()
        };
        let artifacts = |path: &str| {
            [
                PreparedArtifact::Source {
                    path: PathBuf::from(path),
                },
                PreparedArtifact::Binary {
                    path: PathBuf::from(path),
                    format: BinaryFormat::Zip,
                },
                PreparedArtifact::Binary {
                    path: PathBuf::from(path),
                    format: BinaryFormat::TarGz,
                },
            ]
        };
        let original = artifacts("cache/original").each_ref().map(key);
        let relocated = artifacts("cache/relocated").each_ref().map(key);
        assert_eq!(original, relocated);
        assert_eq!(original.iter().collect::<BTreeSet<_>>().len(), 3);
    }

    #[test]
    fn installer_build_key_sorts_dependencies() {
        let digest = InstallerDigest::from_bytes([1; 32]);
        let key = |dependencies: &[(String, Option<String>)]| {
            installer_build_key(
                &PreparedArtifact::Source {
                    path: PathBuf::from("source.tar.gz"),
                },
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

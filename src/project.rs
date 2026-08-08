use directories::ProjectDirs;
use miette::Diagnostic;
use r_description::lossless::RDescription;
use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use thiserror::Error;

use crate::lockfile::{LOCKFILE_VERSION, Lockfile};

pub const LOCKFILE_NAME: &str = "rpx.lock";
pub const DESCRIPTION_NAME: &str = "DESCRIPTION";

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectDiscoveryError {
    #[error("failed to get current directory: {source}")]
    #[diagnostic(code(rpx::project::current_dir_failed))]
    CurrentDirectory {
        #[source]
        source: std::io::Error,
    },

    #[error("project root not found in current directory or any parent directory")]
    #[diagnostic(code(rpx::project::not_found))]
    NotFound,
}

#[derive(Debug, Error, Diagnostic)]
pub enum ManifestReadError {
    #[error("failed to read DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::manifest_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::manifest_parse_failed))]
    Parse {
        path: PathBuf,
        #[source]
        source: r_description::lossless::Error,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum LockfileReadError {
    #[error("rpx.lock not found at {}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_not_found))]
    NotFound { path: PathBuf },

    #[error("failed to read rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_parse_failed))]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "unsupported rpx.lock schema version {version} at {} (supported version: {supported})",
        path.display()
    )]
    #[diagnostic(code(rpx::project::lockfile_unsupported_version))]
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
        supported: u32,
    },
}

pub struct Project(PathBuf);

impl Project {
    pub fn discover() -> Result<Self, ProjectDiscoveryError> {
        let current_dir = env::current_dir()
            .map_err(|source| ProjectDiscoveryError::CurrentDirectory { source })?;

        current_dir
            .ancestors()
            .find(|directory| {
                directory.join(".git").exists()
                    || directory.join(DESCRIPTION_NAME).is_file()
                    || directory.join(LOCKFILE_NAME).is_file()
            })
            .map(|directory| Self(directory.to_path_buf()))
            .ok_or(ProjectDiscoveryError::NotFound)
    }

    pub fn read_manifest(&self) -> Result<RDescription, ManifestReadError> {
        let path = self.0.join(DESCRIPTION_NAME);
        let contents = fs::read_to_string(&path).map_err(|source| ManifestReadError::Read {
            path: path.clone(),
            source,
        })?;

        contents
            .parse()
            .map_err(|source| ManifestReadError::Parse { path, source })
    }

    pub fn read_lockfile(&self) -> Result<Lockfile, LockfileReadError> {
        let path = self.0.join(LOCKFILE_NAME);
        let contents = fs::read_to_string(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                LockfileReadError::NotFound { path: path.clone() }
            } else {
                LockfileReadError::Read {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        let lockfile = serde_json::from_str::<Lockfile>(&contents).map_err(|source| {
            LockfileReadError::Parse {
                path: path.clone(),
                source,
            }
        })?;

        if lockfile.version != LOCKFILE_VERSION {
            return Err(LockfileReadError::UnsupportedVersion {
                path,
                version: lockfile.version,
                supported: LOCKFILE_VERSION,
            });
        }

        Ok(lockfile)
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum ProjectPathError {
    #[error("failed to get current directory: {source}")]
    #[diagnostic(code(rpx::project::current_dir_failed))]
    CurrentDirFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("{DESCRIPTION_NAME} not found in current directory or any parent directory")]
    #[diagnostic(code(rpx::project::description_not_found))]
    DescriptionNotFound,
}

pub fn new_project_description_path() -> Result<PathBuf, ProjectPathError> {
    Ok(current_dir()?.join(DESCRIPTION_NAME))
}

pub fn description_path() -> Result<PathBuf, ProjectPathError> {
    Ok(project_root_result()?.join(DESCRIPTION_NAME))
}

pub fn lockfile_path_result() -> Result<PathBuf, String> {
    Ok(project_root_result()
        .map_err(|error| error.to_string())?
        .join(LOCKFILE_NAME))
}

pub fn project_library_path() -> PathBuf {
    let library_path = project_library_root_path().join("library");

    fs::create_dir_all(&library_path).expect("failed to create project library");
    library_path
}

pub fn project_library_root_path() -> PathBuf {
    let project_key = hash_path(&project_root());
    project_dirs()
        .data_dir()
        .join("libraries")
        .join(project_key)
}

pub fn cache_dir_path() -> PathBuf {
    project_dirs().cache_dir().to_path_buf()
}

pub fn artifact_cache_path(package: &str, version: &str, file_name: &str) -> PathBuf {
    let path = project_dirs()
        .cache_dir()
        .join("artifacts")
        .join(package)
        .join(version)
        .join(file_name);
    ensure_parent_dir(&path);
    path
}

pub fn build_temp_library_path(package: &str, unique: &str) -> PathBuf {
    let path = project_dirs()
        .cache_dir()
        .join("build-temp")
        .join(format!("{package}-{unique}"))
        .join("library");
    fs::create_dir_all(&path).expect("failed to create temporary build library");
    path
}

#[deprecated]
pub fn project_root() -> PathBuf {
    project_root_result().unwrap_or_else(|error| panic!("{error}"))
}

#[deprecated]
pub fn project_root_result() -> Result<PathBuf, ProjectPathError> {
    let current_dir = current_dir()?;
    let current_dir = current_dir
        .canonicalize()
        .unwrap_or_else(|_| current_dir.clone());

    for candidate in current_dir.ancestors() {
        if candidate.join(DESCRIPTION_NAME).exists() {
            return Ok(candidate.to_path_buf());
        }
    }

    Err(ProjectPathError::DescriptionNotFound)
}

#[deprecated]
fn current_dir() -> Result<PathBuf, ProjectPathError> {
    env::current_dir().map_err(|source| ProjectPathError::CurrentDirFailed { source })
}

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("de", "scalerail", "rpx").expect("failed to resolve rpx data directory")
}

fn ensure_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create cache directory");
    }
}

fn hash_path(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

//! Source archive cache mechanics. Callers own freshness and reuse policy.
use crate::r::{PackageBuildError, build_package_archive};
use r_metadata::Version;
use r_package_installer::Digest as InstallerDigest;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(super) struct BuildRequest {
    pub package_root: PathBuf,
    pub package: String,
    pub version: Version,
}

#[derive(Clone, Debug)]
pub(super) struct ArchiveEntry {
    path: PathBuf,
}

impl ArchiveEntry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn lookup(&self) -> Option<PathBuf> {
        let path = self.path.clone();
        let valid = tokio::task::spawn_blocking(move || {
            let Ok(expected) = fs::read(path.with_extension("sha256")) else {
                return false;
            };
            artifact_digest(&path).is_ok_and(|digest| expected == digest.as_bytes())
        })
        .await
        .unwrap_or(false);
        valid.then(|| self.path.clone())
    }

    pub async fn lock(&self) -> Result<LockedArchive, PackageBuildError> {
        let lock_path = self.path.with_extension("lock");
        // Keep the lock file in place: unlinking it could let another process
        // lock a different inode. The OS releases the lock when the handle drops.
        let lock = tokio::task::spawn_blocking(move || {
            fs::create_dir_all(lock_path.parent().unwrap())?;
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_path)?;
            lock.lock()?;
            Ok::<_, io::Error>(lock)
        })
        .await
        .map_err(io::Error::other)
        .and_then(|result| result)
        .map_err(|source| self.cache_error(source))?;
        Ok(LockedArchive {
            entry: self.clone(),
            _lock: lock,
        })
    }

    fn cache_error(&self, source: io::Error) -> PackageBuildError {
        PackageBuildError::Cache {
            path: self.path.clone(),
            source,
        }
    }
}

pub(super) struct LockedArchive {
    entry: ArchiveEntry,
    _lock: fs::File,
}

impl LockedArchive {
    pub async fn lookup(&self) -> Option<PathBuf> {
        self.entry.lookup().await
    }

    /// The candidate borrows this lock so it cannot outlive the transaction.
    pub async fn build(
        &self,
        request: &BuildRequest,
    ) -> Result<ArchiveCandidate<'_>, PackageBuildError> {
        let temporary =
            tempfile::TempPath::try_from_path(self.entry.path.with_extension("pending.tar.gz"))
                .map_err(|source| self.entry.cache_error(source))?;
        build_package_archive(
            &request.package_root,
            &request.package,
            request.version.as_ref(),
            &temporary,
        )
        .await?;
        Ok(ArchiveCandidate {
            temporary,
            entry: &self.entry,
        })
    }
}

/// Dropping an unpublished candidate cleans it up, including on validation failure.
pub(super) struct ArchiveCandidate<'a> {
    temporary: tempfile::TempPath,
    entry: &'a ArchiveEntry,
}

impl ArchiveCandidate<'_> {
    pub async fn publish(self) -> Result<PathBuf, PackageBuildError> {
        self.temporary.persist(&self.entry.path).map_err(|error| {
            PackageBuildError::PublishArchive {
                path: self.entry.path.clone(),
                source: error.error,
            }
        })?;
        let path = self.entry.path.clone();
        tokio::task::spawn_blocking(move || {
            let digest = artifact_digest(&path)?;
            fs::write(path.with_extension("sha256"), digest.as_bytes())
        })
        .await
        .map_err(io::Error::other)
        .and_then(|result| result)
        .map_err(|source| self.entry.cache_error(source))?;
        Ok(self.entry.path.clone())
    }
}

pub(super) fn artifact_digest(path: &Path) -> io::Result<InstallerDigest> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(InstallerDigest::from_bytes(hash.finalize().into()))
}

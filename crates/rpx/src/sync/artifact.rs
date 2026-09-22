use r_package_installer::{Artifact, BinaryArtifact, BinaryFormat, SourceArtifact, SourceOptions};
use std::path::{Path, PathBuf};

/// The result of successful artifact acquisition. Producers publish this value
/// only after building, downloading, or finding the file in the cache.
#[derive(Debug)]
pub(super) enum PreparedArtifact {
    Binary { path: PathBuf, format: BinaryFormat },
    Source { path: PathBuf },
}

impl PreparedArtifact {
    pub fn path(&self) -> &Path {
        match self {
            Self::Binary { path, .. } | Self::Source { path } => path,
        }
    }

    pub fn trace_kind(&self) -> &'static str {
        match self {
            Self::Binary { .. } => "binary",
            Self::Source { .. } => "source",
        }
    }

    pub fn installation_action(&self) -> &'static str {
        match self {
            Self::Binary { .. } => "installing binary",
            Self::Source { .. } => "installing source",
        }
    }

    pub fn to_installer_artifact(&self, project_library: PathBuf) -> Artifact {
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

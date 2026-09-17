use crate::{
    output::status,
    project::{cache_dir_path, libraries_dir_path},
};
use miette::Diagnostic;
use std::{fs, path::Path};
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error("failed to remove {label} at {path}")]
    #[diagnostic(code(rpx::clean::remove_failed))]
    RemoveFailed {
        label: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub(crate) fn run() -> Result<(), Error> {
    let mut removed_any = false;

    removed_any |= remove_dir_if_exists(&libraries_dir_path(), "project libraries")?;
    removed_any |= remove_dir_if_exists(&cache_dir_path(), "cache directory")?;

    if removed_any {
        status("Removed all project libraries and cache directories");
    } else {
        status("Project libraries and cache directories are already clean");
    }
    Ok(())
}

fn remove_dir_if_exists(path: &Path, label: &str) -> Result<bool, Error> {
    if !path.exists() {
        return Ok(false);
    }

    fs::remove_dir_all(path).map_err(|source| Error::RemoveFailed {
        label: label.to_string(),
        path: path.display().to_string(),
        source,
    })?;
    Ok(true)
}

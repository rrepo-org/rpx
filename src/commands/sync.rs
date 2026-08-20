use crate::{
    SyncError,
    description::read_description,
    lockfile::read_lockfile,
    output::status,
    project::{find_project_root, validate_locked_resolution},
    r::r_version_async,
    sync_project,
};

pub(crate) async fn run(
    no_install_project: bool,
    install_system: bool,
    install_only_system: bool,
) -> Result<(), SyncError> {
    let current_dir = find_project_root()?;
    let description = read_description(&current_dir)?;
    let lockfile = read_lockfile(&current_dir)?;
    let r_version = r_version_async().await?;
    validate_locked_resolution(&current_dir, &description, &r_version, &lockfile)?;

    sync_project(
        &current_dir,
        description,
        &lockfile,
        &r_version,
        no_install_project,
        install_system,
        install_only_system,
    )
    .await?;
    status("Synchronized project library");
    Ok(())
}

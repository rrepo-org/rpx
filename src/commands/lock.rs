use crate::{
    LockError,
    description::read_description,
    lockfile::{LockfileReadError, read_lockfile, write_lockfile},
    output::status,
    project::find_project_root,
    resolve_lockfile_for_description,
};

pub(crate) async fn run() -> Result<(), LockError> {
    let current_dir = find_project_root()?;
    let description = read_description(&current_dir)?;
    let old_lockfile = match read_lockfile(&current_dir) {
        Ok(lockfile) => Some(lockfile),
        Err(LockfileReadError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Err(LockfileReadError::OutdatedLockfile { .. }) => None,
        Err(source) => return Err(source.into()),
    };
    let lockfile =
        resolve_lockfile_for_description(&current_dir, &description, old_lockfile.as_ref()).await?;
    let changed = old_lockfile.as_ref() != Some(&lockfile);
    write_lockfile(&current_dir, &lockfile)?;

    if changed {
        status("Updated rpx.lock");
    } else {
        status("rpx.lock is already up to date");
    }
    Ok(())
}

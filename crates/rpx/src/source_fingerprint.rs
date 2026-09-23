//! Conservative content identity for a local package tree.
use sha2::{Digest, Sha256};
use std::{fs, io, io::Read, path::Path};

/// Symlinks and special files may depend on inputs outside the package tree.
/// Return `None` for those trees rather than reuse an incomplete fingerprint.
pub(crate) fn fingerprint(root: &Path) -> io::Result<Option<String>> {
    let mut hash = Sha256::new();
    if !visit(root, root, &mut hash)? {
        return Ok(None);
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn visit(root: &Path, path: &Path, hash: &mut Sha256) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    let kind = metadata.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return Ok(false);
    }
    field(
        hash,
        path.strip_prefix(root)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        field(hash, &metadata.permissions().mode().to_le_bytes());
    }
    #[cfg(not(unix))]
    field(hash, &[u8::from(metadata.permissions().readonly())]);
    if kind.is_dir() {
        field(hash, b"directory");
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            if !visit(root, &entry, hash)? {
                return Ok(false);
            }
        }
    } else {
        field(hash, b"file");
        // A separate file digest makes boundaries independent of read chunking.
        let mut contents = Sha256::new();
        let mut file = fs::File::open(path)?;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            contents.update(&buffer[..count]);
        }
        field(hash, &contents.finalize());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_contents_paths_and_all_files_not_timestamps() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path();
        fs::write(root.join("DESCRIPTION"), "Version: 1.0").unwrap();
        let original = fingerprint(root).unwrap().unwrap();
        fs::write(root.join("DESCRIPTION"), "Version: 1.0").unwrap();
        assert_eq!(fingerprint(root).unwrap().unwrap(), original);
        fs::write(root.join("DESCRIPTION"), "Version: 2.0").unwrap();
        assert_ne!(fingerprint(root).unwrap().unwrap(), original);
        fs::write(root.join("DESCRIPTION"), "Version: 1.0").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        let before = fingerprint(root).unwrap().unwrap();
        fs::write(root.join("ignored"), "untracked").unwrap();
        let added = fingerprint(root).unwrap().unwrap();
        assert_ne!(before, added);
        fs::rename(root.join("ignored"), root.join("renamed")).unwrap();
        assert_ne!(fingerprint(root).unwrap().unwrap(), added);
        fs::remove_file(root.join("renamed")).unwrap();
        assert_eq!(fingerprint(root).unwrap().unwrap(), before);
    }

    #[test]
    fn traversal_order_and_absolute_location_do_not_affect_identity() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            fs::write(first.path().join(name), name).unwrap();
        }
        for name in ["b", "a"] {
            fs::write(second.path().join(name), name).unwrap();
        }
        assert_eq!(
            fingerprint(first.path()).unwrap(),
            fingerprint(second.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn permissions_invalidate_and_symlinks_bypass_reuse() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tree = tempfile::tempdir().unwrap();
        let file = tree.path().join("configure");
        fs::write(&file, "script").unwrap();
        let before = fingerprint(tree.path()).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(fingerprint(tree.path()).unwrap(), before);
        symlink("configure", tree.path().join("link")).unwrap();
        assert_eq!(fingerprint(tree.path()).unwrap(), None);
    }
}

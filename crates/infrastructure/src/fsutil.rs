//! Atomic, owner-only file writes shared by adapters that persist sensitive
//! data (secret key/bundles, workdir `.env`): files are born `0600` (§10,
//! §17) and widened to the caller's requested mode only once their contents
//! are on disk, then replaced via temp + rename so readers never observe a
//! partial write or a permission window.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

fn create_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_temp_private(dir: &Path, prefix: &str, contents: &[u8]) -> std::io::Result<PathBuf> {
    loop {
        let temp_path = dir.join(format!(".{prefix}.{}.tmp", uuid::Uuid::new_v4()));
        match create_private_new(&temp_path) {
            Ok(mut file) => {
                let write_result = file.write_all(contents).and_then(|_| file.sync_all());
                if let Err(e) = write_result {
                    let _ = fs::remove_file(&temp_path);
                    return Err(e);
                }
                return Ok(temp_path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

fn parent_dir(path: &Path) -> std::io::Result<&Path> {
    path.parent().ok_or_else(|| {
        std::io::Error::other(format!("missing parent directory for {}", path.display()))
    })
}

fn temp_prefix(path: &Path, fallback: &'static str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback)
        .to_string()
}

/// Creates `path` with `0600` only when it does not exist yet; returns
/// Ok(false) when another writer got there first (the existing file wins).
pub(crate) fn write_private_exclusive(path: &Path, contents: &[u8]) -> std::io::Result<bool> {
    let dir = parent_dir(path)?;
    let prefix = temp_prefix(path, "secret");
    let temp_path = write_temp_private(dir, &prefix, contents)?;
    let link_result = fs::hard_link(&temp_path, path);
    let _ = fs::remove_file(&temp_path);
    match link_result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Replaces `path` atomically (temp + rename) with `contents` at `mode`.
///
/// The temp file is born `0600` and only widened once its contents are on
/// disk, so no reader can ever observe a partially written file at the final
/// mode. `set_permissions` rather than `OpenOptions::mode` because the latter
/// is masked by the process umask, and the result must not depend on what the
/// unit happens to set.
pub(crate) fn write_private_atomic(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = parent_dir(path)?;
    let prefix = temp_prefix(path, "private");
    let temp_path = write_temp_private(dir, &prefix, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&temp_path, fs::Permissions::from_mode(mode)) {
            let _ = fs::remove_file(&temp_path);
            return Err(e);
        }
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Err(e) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Both tests below are unix-only, so this import is unused (and would
    // warn under `-D warnings`) on non-unix targets without the same gate.
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn writes_with_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        write_private_atomic(&path, b"x", 0o644).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_wider_file_narrows_it_to_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        write_private_atomic(&path, b"new", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}

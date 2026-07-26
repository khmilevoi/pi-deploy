use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi_domain::contracts::SecretsWriter;
use pi_domain::entities::SecretsBundle;
use pi_domain::error::DomainError;
use serde::{Deserialize, Serialize};

use crate::dotenv;
use crate::fsutil;
use crate::secretpath;

/// Name of the bookkeeping file dropped at the workdir root to remember
/// which secret file paths *this writer* created on the previous `write()`
/// call. Never validated as a secret path (it isn't one): it's an internal
/// implementation detail, not something `[secrets].files` can ever target
/// since it never goes through `secretpath::validate_rel_path`.
const MANIFEST_FILE_NAME: &str = ".rpi-secrets-manifest.json";

/// On-disk record of the previous `write()` call's `bundle.files` key set,
/// used to compute which files must be deleted on the next call (whole-
/// bundle replace, secrets spec §2.5). JSON, written via
/// `fsutil::write_private_atomic` (0600, atomic) like everything else here.
#[derive(Serialize, Deserialize, Default)]
struct SecretsManifest {
    #[serde(default)]
    files: Vec<String>,
}

/// Writes the decrypted bundle into `<workdir>`: `.env` (`bundle.env_mode()`,
/// `0600` by default, atomic) plus every secret file at its relative path
/// (`bundle.secret_file_mode()`, `0644` by default; directories created along
/// the way get `0755`). Each directory level between the workdir root and the
/// target is checked for a symlink *before* being created or descended into,
/// so a symlink committed to the repo cannot redirect writes outside the
/// workdir (secrets spec §7). Files stay in place: compose re-reads them on
/// `up`. The manifest (see [`MANIFEST_FILE_NAME`]) is always `0600`: it is
/// this writer's own bookkeeping, not a project secret a container needs to
/// read.
///
/// Whole-bundle replace also applies to files (secrets spec §2.5): a small
/// manifest (see [`MANIFEST_FILE_NAME`]) dropped at the workdir root records
/// which paths this writer created, so a file dropped from `[secrets].files`
/// is deleted from a persistent workdir on the next `write()` instead of
/// lingering forever. A missing or corrupt manifest is treated as "nothing
/// previously written" (best-effort: never deletes a file it has no record
/// of writing itself).
pub struct FsSecretsWriter;

impl FsSecretsWriter {
    pub fn new() -> Arc<FsSecretsWriter> {
        Arc::new(FsSecretsWriter)
    }
}

fn storage_err(context: String, e: impl std::fmt::Display) -> DomainError {
    DomainError::Storage(format!("{context}: {e}"))
}

/// Mode for directories this writer creates on the way to a secret file.
/// Traversable by anyone, like every other directory in the checkout: a
/// directory mode protects nothing here, the file mode does.
///
/// Both uses sit behind `#[cfg(unix)]` (there is nothing to chmod on a
/// non-unix target), so the constant itself is unused there — same idiom as
/// `agent/config.rs:18`.
#[cfg_attr(not(unix), allow(dead_code))]
const DIR_MODE: u32 = 0o755;

fn create_secret_dir(path: &Path) -> std::io::Result<()> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)?;
    // `DirBuilder::mode` is masked by the process umask (same reason
    // `fsutil::write_private_atomic` uses `set_permissions` instead of
    // `OpenOptions::mode` for files) — set it explicitly afterward so the
    // result does not depend on what the unit happens to set.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE))?;
    }
    Ok(())
}

/// Widens a directory this writer created before 0.26.0 (owned by us and at
/// exactly `0700`) so a container can traverse into it. Anything else — a
/// directory from the repository, one with another owner, one with any other
/// mode, or not a directory at all — is left untouched: it is not ours to
/// change. Best-effort; a failure to stat or chmod is not worth failing a
/// deploy over.
///
/// `symlink_metadata` rather than `metadata`: the caller already ruled out a
/// symlink at this exact path via `stat_dir_component` moments ago, but using
/// the non-following stat here too avoids re-opening a TOCTOU window, and —
/// combined with the `is_dir()` check — also refuses a *plain file* that
/// happens to sit at an intermediate path component. `DirStep::Existing`
/// covers "any non-symlink entry", not "any directory", so without this
/// check a 0700 file owned by us would get chmod'd to 0755 before the write
/// itself fails on it not being a directory.
#[cfg(unix)]
fn widen_legacy_secret_dir(path: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.file_type().is_dir() {
        return;
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        return;
    }
    // SAFETY: geteuid has no preconditions and cannot fail. The effective
    // uid, not the real uid, is what the kernel checks when deciding whether
    // this process is allowed to `chmod` a path it doesn't own outright, so
    // it's the right identity to compare against `meta.uid()` here. Same
    // libc-call idiom as `current_uid` in `cloudflared.rs`, which uses
    // `getuid` instead — that call only needs "which uid am I" to label an
    // environment variable, not a permission check, so real vs. effective
    // doesn't matter there.
    if meta.uid() != unsafe { libc::geteuid() } {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE));
}

#[cfg(not(unix))]
fn widen_legacy_secret_dir(_path: &Path) {}

/// Result of checking one intermediate directory-path component while
/// walking from the (already canonicalized) workdir root toward a target
/// path, via `symlink_metadata` — which, unlike `metadata`, never follows
/// the component being checked itself.
enum DirStep {
    /// The path exists and is not a symlink.
    Existing,
    /// Nothing exists at this path yet.
    Missing,
    /// The path exists and is a symlink: never safe to create inside or
    /// descend into, since it can point anywhere on the filesystem.
    Symlink,
}

fn stat_dir_component(dir: &Path) -> std::io::Result<DirStep> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(DirStep::Symlink),
        Ok(_) => Ok(DirStep::Existing),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DirStep::Missing),
        Err(e) => Err(e),
    }
}

fn write_files_blocking(
    workdir: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    mode: u32,
) -> Result<(), DomainError> {
    let root = std::fs::canonicalize(&workdir)
        .map_err(|e| storage_err("canonicalize workdir".into(), e))?;
    for (rel, bytes) in files {
        secretpath::validate_rel_path(&rel)
            .map_err(|e| DomainError::Invalid(format!("secret file '{rel}': {e}")))?;
        let mut components: Vec<Component<'_>> = Path::new(&rel).components().collect();
        let file_component = components
            .pop()
            .ok_or_else(|| DomainError::Invalid(format!("secret file '{rel}': empty name")))?;

        // Walk each intermediate directory level from the (already
        // canonical) workdir root down to the target's parent, refusing to
        // create or step through a symlink at any level. Unlike `mkdir -p`
        // (the previous approach), which follows symlinks for every
        // intermediate path component, this checks each level *before*
        // creating or descending into it, so a symlink committed into the
        // repo can never be followed to create anything outside the root —
        // not even transiently, for a multi-level path under the symlink.
        let mut dir = root.clone();
        for component in &components {
            dir.push(component);
            match stat_dir_component(&dir)
                .map_err(|e| storage_err(format!("stat dir for '{rel}'"), e))?
            {
                DirStep::Symlink => {
                    return Err(DomainError::Invalid(format!(
                        "secret file '{rel}' escapes the workdir (symlinked directory?)"
                    )));
                }
                DirStep::Existing => widen_legacy_secret_dir(&dir),
                DirStep::Missing => {
                    create_secret_dir(&dir)
                        .map_err(|e| storage_err(format!("create dir for '{rel}'"), e))?;
                }
            }
        }

        let target = dir.join(file_component);
        fsutil::write_private_atomic(&target, &bytes, mode)
            .map_err(|e| storage_err(format!("write secret file '{rel}'"), e))?;
    }
    Ok(())
}

/// Walks the intermediate directory components of a stale (previous-bundle)
/// relative path, from the canonicalized workdir `root`, *without* creating
/// or modifying anything. Returns the confirmed-safe target path — the same
/// path `write_files_blocking` would itself have written to — only if every
/// intermediate component already exists as a real, non-symlink entry.
/// Returns `None` if any component is missing, is a symlink, or can't be
/// stat'd: the caller must then leave that file alone rather than delete it.
///
/// This is the delete-path counterpart to `write_files_blocking`'s symlink
/// guard above. `remove_file` follows symlinks for every intermediate
/// directory component (though not the leaf itself — standard POSIX unlink
/// semantics), so without this check a project commit that (a) drops every
/// file under some directory from `[secrets].files` in the same commit that
/// (b) replaces that directory with a symlink could redirect a "stale file"
/// deletion outside the workdir: the write path's guard never runs for that
/// directory in that case, since nothing under it is being written this
/// time, only removed.
fn safe_stale_target(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut components: Vec<Component<'_>> = Path::new(rel).components().collect();
    let file_component = components.pop()?;

    let mut dir = root.to_path_buf();
    for component in &components {
        dir.push(component);
        match stat_dir_component(&dir) {
            Ok(DirStep::Existing) => {}
            // Missing, symlinked, or unstat-able: can't confirm this is the
            // same real directory `write_files_blocking` created, so don't
            // touch it. Best-effort cleanup only ever narrows, never risks
            // an escape.
            _ => return None,
        }
    }
    Some(dir.join(file_component))
}

/// Reads the previous-write manifest, returning the empty set if it's
/// missing, unreadable, or fails to parse. Bookkeeping is best-effort: a
/// corrupt manifest must never fail the deploy, it just means this write
/// won't clean up anything (safe: it only widens what's left alone, never
/// what gets deleted).
fn read_manifest(path: &Path) -> BTreeSet<String> {
    let Ok(contents) = std::fs::read(path) else {
        return BTreeSet::new();
    };
    serde_json::from_slice::<SecretsManifest>(&contents)
        .map(|m| m.files.into_iter().collect())
        .unwrap_or_default()
}

/// Deletes files that were in the previous manifest but are absent from the
/// current bundle, then writes (or, if the bundle has no files, removes) the
/// manifest reflecting the current set. Runs after new/changed files have
/// already been written successfully, so a failure while writing new files
/// never leaves old files deleted without their replacements in place.
///
/// Manifest entries are re-validated with `secretpath::validate_rel_path`
/// before use: the manifest lives inside the project's own git checkout, so
/// treating it as fully trusted would let a crafted `.rpi-secrets-manifest.json`
/// committed to the repo smuggle a `..`-escaping "stale" path through the
/// deletion step. An entry that fails validation is simply skipped, same as
/// a corrupt manifest.
///
/// Each stale path also runs through [`safe_stale_target`] before deletion:
/// a directory a previous `write()` created can be replaced with a symlink
/// by a later commit (see that function's doc comment for the exact
/// scenario), and `remove_file` — like almost every filesystem call —
/// follows symlinks for intermediate path components. A path that fails
/// this check is skipped, not treated as an error: cleanup here is
/// best-effort tidying of a leftover file, not the primary write path, so
/// leaving something suspicious alone for a future cycle is strictly safer
/// than either following a symlink outside the workdir or failing the whole
/// `secrets` deploy stage over it.
fn sync_manifest_blocking(
    workdir: &Path,
    manifest_path: &Path,
    previous: BTreeSet<String>,
    current: &BTreeSet<String>,
) -> Result<(), DomainError> {
    let stale: Vec<&String> = previous.difference(current).collect();
    if !stale.is_empty() {
        // Best-effort: if the workdir itself can't be canonicalized here
        // (it was already confirmed to exist by `write`, and
        // `write_files_blocking` just canonicalized it successfully moments
        // ago), skip cleanup rather than fail the whole deploy over
        // stale-file tidying.
        if let Ok(root) = std::fs::canonicalize(workdir) {
            for rel in stale {
                if secretpath::validate_rel_path(rel).is_err() {
                    continue;
                }
                let Some(target) = safe_stale_target(&root, rel) else {
                    continue;
                };
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(storage_err(format!("remove stale secret file '{rel}'"), e));
                    }
                }
            }
        }
    }

    if current.is_empty() {
        match std::fs::remove_file(manifest_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(storage_err("remove stale secrets manifest".into(), e)),
        }
    } else {
        let manifest = SecretsManifest {
            files: current.iter().cloned().collect(),
        };
        let contents = serde_json::to_vec(&manifest)
            .map_err(|e| storage_err("serialize secrets manifest".into(), e))?;
        fsutil::write_private_atomic(manifest_path, &contents, 0o600)
            .map_err(|e| storage_err("write secrets manifest".into(), e))?;
    }
    Ok(())
}

fn sync_files_blocking(
    workdir: PathBuf,
    files: Vec<(String, Vec<u8>)>,
    current: BTreeSet<String>,
    mode: u32,
) -> Result<(), DomainError> {
    let manifest_path = workdir.join(MANIFEST_FILE_NAME);
    let previous = read_manifest(&manifest_path);

    write_files_blocking(workdir.clone(), files, mode)?;

    sync_manifest_blocking(&workdir, &manifest_path, previous, &current)
}

#[async_trait]
impl SecretsWriter for FsSecretsWriter {
    async fn write(&self, workdir: &Path, bundle: &SecretsBundle) -> Result<(), DomainError> {
        if !workdir.is_dir() {
            return Err(DomainError::NotFound(format!(
                "workdir {} does not exist; deploy the project first",
                workdir.display()
            )));
        }
        let env_path = workdir.join(".env");
        if bundle.vars.is_empty() {
            // whole-bundle replace: a stale .env must not survive a resend
            match tokio::fs::remove_file(&env_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage_err("remove stale .env".into(), e)),
            }
        } else {
            let contents = dotenv::serialize(bundle);
            let env_mode = bundle.env_mode();
            tokio::task::spawn_blocking(move || {
                fsutil::write_private_atomic(&env_path, contents.as_bytes(), env_mode)
            })
            .await
            .map_err(|e| storage_err("write .env".into(), format!("join error: {e}")))?
            .map_err(|e| storage_err("write .env".into(), e))?;
        }
        let root = workdir.to_path_buf();
        let files: Vec<(String, Vec<u8>)> = bundle
            .files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let current: BTreeSet<String> = bundle.files.keys().cloned().collect();
        let file_mode = bundle.secret_file_mode();
        tokio::task::spawn_blocking(move || sync_files_blocking(root, files, current, file_mode))
            .await
            .map_err(|e| storage_err("write secret files".into(), format!("join error: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> SecretsBundle {
        let mut b = SecretsBundle::default();
        b.vars.insert("A".into(), "1".into());
        b
    }

    fn bundle_with_file() -> SecretsBundle {
        let mut b = SecretsBundle::default();
        b.vars.insert("A".into(), "1".into());
        b.files
            .insert("certs/server.pem".into(), vec![0u8, 159, 146, 150]);
        b
    }

    #[tokio::test]
    async fn writes_env_file_into_existing_workdir() {
        let dir = tempfile::tempdir().unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "A=1\n"
        );
    }

    #[tokio::test]
    async fn rewriting_env_file_replaces_contents() {
        let dir = tempfile::tempdir().unwrap();
        let writer = FsSecretsWriter::new();
        writer.write(dir.path(), &bundle()).await.unwrap();
        let mut updated = bundle();
        updated.vars.insert("B".into(), "2".into());
        writer.write(dir.path(), &updated).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "A=1\nB=2\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn env_file_is_0600_even_when_replacing_a_wider_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "OLD=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle())
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn missing_workdir_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = FsSecretsWriter::new()
            .write(&dir.path().join("never-deployed"), &bundle())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
    }

    #[tokio::test]
    async fn writes_secret_files_at_relative_paths_creating_dirs() {
        let dir = tempfile::tempdir().unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("certs").join("server.pem")).unwrap(),
            vec![0u8, 159, 146, 150]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".env")).unwrap(),
            "A=1\n"
        );
    }

    #[tokio::test]
    async fn empty_vars_removes_stale_env_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "OLD=1\n").unwrap();
        let mut b = SecretsBundle::default();
        b.files.insert("secret.txt".into(), b"x".to_vec());
        FsSecretsWriter::new().write(dir.path(), &b).await.unwrap();
        assert!(
            !dir.path().join(".env").exists(),
            "stale .env must not survive"
        );
        assert_eq!(std::fs::read(dir.path().join("secret.txt")).unwrap(), b"x");
    }

    #[tokio::test]
    async fn rejects_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = SecretsBundle::default();
        b.files.insert("../escape.txt".into(), b"x".to_vec());
        let err = FsSecretsWriter::new()
            .write(dir.path(), &b)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_directory_cannot_redirect_writes_outside_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("wd");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workdir.join("link")).unwrap();

        let mut b = SecretsBundle::default();
        b.files.insert("link/leak.txt".into(), b"secret".to_vec());
        let err = FsSecretsWriter::new()
            .write(&workdir, &b)
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
        assert!(
            !outside.join("leak.txt").exists(),
            "write escaped the workdir"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nested_symlinked_directory_cannot_redirect_writes_outside_workdir() {
        // Regression test: a multi-level relative path under a symlinked
        // intermediate directory must not cause any directory to be
        // created outside the workdir, even transiently. The previous
        // `mkdir -p` + canonicalize-after-the-fact approach followed the
        // symlink while creating "nested", planting a real directory in
        // `outside` before the escape check ever ran.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("wd");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workdir.join("certs")).unwrap();

        let mut b = SecretsBundle::default();
        b.files
            .insert("certs/nested/leak.txt".into(), b"secret".to_vec());
        let err = FsSecretsWriter::new()
            .write(&workdir, &b)
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
        assert!(
            !outside.join("nested").exists(),
            "directory must not be created outside the workdir"
        );
        assert!(!outside.join("nested").join("leak.txt").exists());
    }

    #[tokio::test]
    async fn dropped_file_is_removed_while_surviving_file_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let writer = FsSecretsWriter::new();

        let mut first = SecretsBundle::default();
        first.files.insert("keep.txt".into(), b"keep-me".to_vec());
        first
            .files
            .insert("drop/me.txt".into(), b"drop-me".to_vec());
        writer.write(dir.path(), &first).await.unwrap();
        assert!(dir.path().join("keep.txt").exists());
        assert!(dir.path().join("drop/me.txt").exists());

        let mut second = SecretsBundle::default();
        second.files.insert("keep.txt".into(), b"keep-me".to_vec());
        writer.write(dir.path(), &second).await.unwrap();

        assert!(
            !dir.path().join("drop/me.txt").exists(),
            "file dropped from the bundle must be removed from a persistent workdir"
        );
        assert_eq!(
            std::fs::read(dir.path().join("keep.txt")).unwrap(),
            b"keep-me",
            "file present in both writes must survive with unchanged content"
        );
    }

    #[tokio::test]
    async fn first_write_with_no_manifest_does_not_touch_pre_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate an existing checkout from before this fix shipped (or any
        // unrelated project file): nothing this writer has a record of.
        std::fs::write(dir.path().join("unrelated.txt"), b"pre-existing").unwrap();

        let mut b = SecretsBundle::default();
        b.files.insert("secret.txt".into(), b"s".to_vec());
        FsSecretsWriter::new().write(dir.path(), &b).await.unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("unrelated.txt")).unwrap(),
            b"pre-existing",
            "a missing manifest must never cause deletion of files this writer didn't create"
        );
        assert_eq!(std::fs::read(dir.path().join("secret.txt")).unwrap(), b"s");
    }

    #[tokio::test]
    async fn dropping_to_zero_files_removes_all_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let writer = FsSecretsWriter::new();

        let mut with_files = SecretsBundle::default();
        with_files.files.insert("a.txt".into(), b"a".to_vec());
        with_files
            .files
            .insert("nested/b.txt".into(), b"b".to_vec());
        writer.write(dir.path(), &with_files).await.unwrap();
        assert!(dir.path().join(MANIFEST_FILE_NAME).exists());

        let empty = SecretsBundle::default();
        writer.write(dir.path(), &empty).await.unwrap();

        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("nested/b.txt").exists());
        assert!(
            !dir.path().join(MANIFEST_FILE_NAME).exists(),
            "manifest must not linger once there are no secret files left"
        );
    }

    #[tokio::test]
    async fn manifest_file_is_not_a_secret_path() {
        let dir = tempfile::tempdir().unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();

        // The manifest's own name is never a secret path a bundle could
        // target: it must never collide with paths used in these tests, and
        // it's never run through the same validation real secret paths are.
        assert!(!bundle_with_file().files.contains_key(MANIFEST_FILE_NAME));
        assert_ne!(MANIFEST_FILE_NAME, "certs/server.pem");
        assert!(dir.path().join(MANIFEST_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manifest_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();
        let mode = std::fs::metadata(dir.path().join(MANIFEST_FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn secret_files_are_container_readable_and_created_dirs_are_traversable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();
        let file_mode = std::fs::metadata(dir.path().join("certs/server.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o644);
        let dir_mode = std::fs::metadata(dir.path().join("certs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o755);
        let env_mode = std::fs::metadata(dir.path().join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(env_mode & 0o777, 0o600, ".env stays private by default");
    }

    #[cfg(unix)]
    #[test]
    fn created_dir_mode_does_not_depend_on_the_process_umask() {
        // `DirBuilder::mode` is masked by the process umask; a strict umask
        // (e.g. 0027, common for service accounts) would otherwise silently
        // narrow a newly created secret directory below DIR_MODE. This test
        // proves the explicit `set_permissions` call in `create_secret_dir`
        // makes the result independent of whatever umask the unit runs
        // under, matching `write_private_atomic`'s file-side guarantee.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("certs");
        // SAFETY: umask has no preconditions and cannot fail; restored
        // immediately after the single directory-creation call below.
        let old = unsafe { libc::umask(0o027) };
        let result = create_secret_dir(&target);
        unsafe {
            libc::umask(old);
        }
        result.unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_mode_applies_to_secret_files_and_to_env() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut b = bundle_with_file();
        b.file_mode = Some(0o640);
        FsSecretsWriter::new().write(dir.path(), &b).await.unwrap();
        for rel in ["certs/server.pem", ".env"] {
            let mode = std::fs::metadata(dir.path().join(rel))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o640, "{rel}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_this_writer_created_at_0700_is_widened_on_the_next_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Simulate a checkout written by rpi < 0.26.0.
        std::fs::create_dir(dir.path().join("certs")).unwrap();
        std::fs::set_permissions(
            dir.path().join("certs"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();

        let mode = std::fs::metadata(dir.path().join("certs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_with_any_other_mode_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A directory that came from the repository, not from this writer.
        std::fs::create_dir(dir.path().join("certs")).unwrap();
        std::fs::set_permissions(
            dir.path().join("certs"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();

        FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await
            .unwrap();

        let mode = std::fs::metadata(dir.path().join("certs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750, "only an exact 0700 is ours to widen");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_plain_file_at_an_intermediate_path_component_is_never_widened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // `stat_dir_component` reports `DirStep::Existing` for any non-symlink
        // entry, including a plain file that happens to occupy the spot a
        // directory component is expected at.
        std::fs::write(dir.path().join("certs"), b"not a directory").unwrap();
        std::fs::set_permissions(
            dir.path().join("certs"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        let result = FsSecretsWriter::new()
            .write(dir.path(), &bundle_with_file())
            .await;

        assert!(
            result.is_err(),
            "writing 'certs/server.pem' through a plain file 'certs' must fail"
        );
        let mode = std::fs::metadata(dir.path().join("certs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "a non-directory entry at 0700 must never be widened"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_deletion_does_not_follow_a_symlink_planted_over_a_dropped_directory() {
        // Exploit scenario (the delete-path counterpart to the two
        // symlinked-directory write tests above): a directory holding only
        // files that get dropped from `[secrets].files` in the very same
        // commit that replaces that directory with a symlink. Because
        // nothing under `certs/` is written on the second call,
        // `write_files_blocking`'s symlink guard never runs for `certs/` at
        // all — it only walks paths it is actually about to write. Without
        // a matching guard on the delete side, `sync_manifest_blocking`
        // would call `remove_file(workdir.join("certs/a.pem"))` directly,
        // which follows the symlinked `certs` component and deletes a file
        // entirely outside the workdir.
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("wd");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let writer = FsSecretsWriter::new();

        let mut first = SecretsBundle::default();
        first
            .files
            .insert("certs/a.pem".into(), b"first-pem".to_vec());
        writer.write(&workdir, &first).await.unwrap();
        assert!(workdir.join("certs").join("a.pem").exists());

        // The next "commit": certs/a.pem is dropped from the bundle
        // entirely, and (attacker-controlled or compromised) the real
        // certs/ directory is replaced with a symlink pointing outside the
        // workdir, containing an innocent file at the very same relative
        // name the manifest still remembers as stale.
        std::fs::remove_dir_all(workdir.join("certs")).unwrap();
        std::os::unix::fs::symlink(&outside, workdir.join("certs")).unwrap();
        std::fs::write(outside.join("a.pem"), b"innocent-outside-file").unwrap();

        let second = SecretsBundle::default();
        let result = writer.write(&workdir, &second).await;

        assert!(
            result.is_ok(),
            "best-effort stale-file cleanup must not fail the whole deploy: {result:?}"
        );
        assert_eq!(
            std::fs::read(outside.join("a.pem")).unwrap(),
            b"innocent-outside-file",
            "stale-file deletion must not follow the symlink and delete outside the workdir"
        );
        assert!(
            std::fs::symlink_metadata(workdir.join("certs"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself must be left untouched"
        );
    }
}

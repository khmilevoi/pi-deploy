# Secret File Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Secret files materialized by the agent become readable by the container that consumes them, without weakening the protection of plaintext secrets on the Pi.

**Architecture:** The mode travels with the bundle (`SecretsBundle.file_mode`) from `rpi.toml` through the wire and the encrypted store to `FsSecretsWriter`, which applies it at write time. Defaults change to `0644` for `[secrets].files` and `0755` for writer-created directories; `.env`, the manifest and the store stay `0600`. The protection that the old `0600` provided moves to `/var/lib/rpi`, which becomes `0750`.

**Tech Stack:** Rust 1.88 (workspace `pi_domain` / `pi_application` / `pi_infrastructure` / `pi` binary), axum agent API, serde/serde_json, mockall, tokio, bash + Node e2e harness under `tests/e2e`.

**Spec:** `docs/superpowers/specs/2026-07-25-secret-file-modes-design.md`

## Global Constraints

- Every task ends with `rtk cargo fmt --all -- --check`, `rtk cargo clippy --all-targets --locked -- -D warnings`, `rtk cargo test --locked` green. A `fmt` diff is fixed with `rtk cargo fmt --all`, never by hand.
- The workspace must keep compiling on Windows: every use of `PermissionsExt` / `DirBuilderExt` / `MetadataExt` sits behind `#[cfg(unix)]`, and mode-asserting tests are `#[cfg(unix)]`. On non-unix the mode argument is accepted and ignored, exactly as `fsutil.rs` already does with `OpenOptions::mode`.
- All code, comments, commit messages and user-facing strings are in English.
- New capability string is `secret-modes`, `since = "0.26.0"`, `Policy::Required`.
- Default modes, used verbatim: secret files `0o644`, writer-created directories `0o755`, `.env` `0o600`, `.rpi-secrets-manifest.json` `0o600`, `/var/lib/rpi` `0750`.
- Accepted `file_mode` spellings: `^0?[0-7]{3}$`. Accepted bit patterns: owner read required, owner write optional, group/other read optional, nothing else.
- Commit after every task (the repo is on branch `feat/secret-file-modes`).

---

### Task 1: Mode-aware atomic write

**Files:**
- Modify: `crates/infrastructure/src/fsutil.rs:68-77`
- Modify (call sites): `crates/infrastructure/src/secrets.rs:113`, `crates/infrastructure/src/secretsfile.rs:134,257,296`
- Test: `crates/infrastructure/src/fsutil.rs` (new `#[cfg(test)] mod tests` at the end of the file — the module does not exist yet)

**Interfaces:**
- Produces: `pub(crate) fn write_private_atomic(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()>`. `write_private_exclusive` is unchanged and stays `0600` (it only ever writes the agent's age identity key).

- [ ] **Step 1: Write the failing test**

Append to `crates/infrastructure/src/fsutil.rs`:

```rust
#[cfg(test)]
mod tests {
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `rtk cargo test -p pi-infrastructure fsutil`
Expected: FAIL — `write_private_atomic` takes 2 arguments, not 3.

- [ ] **Step 3: Implement**

Replace `write_private_atomic` in `crates/infrastructure/src/fsutil.rs`:

```rust
/// Replaces `path` atomically (temp + rename) with `contents` at `mode`.
///
/// The temp file is born `0600` and only widened once its contents are on
/// disk, so no reader can ever observe a partially written file at the final
/// mode. `set_permissions` rather than `OpenOptions::mode` because the latter
/// is masked by the process umask, and the result must not depend on what the
/// unit happens to set.
pub(crate) fn write_private_atomic(
    path: &Path,
    contents: &[u8],
    mode: u32,
) -> std::io::Result<()> {
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
```

Update the module doc comment on line 1-4 to say files are born `0600` and widened to the caller's mode before the rename.

- [ ] **Step 4: Update the four call sites to pass `0o600` (behaviour unchanged for now)**

- `crates/infrastructure/src/secrets.rs:113` → `fsutil::write_private_atomic(&path, &ciphertext, 0o600)`
- `crates/infrastructure/src/secretsfile.rs:134` → `fsutil::write_private_atomic(&target, &bytes, 0o600)`
- `crates/infrastructure/src/secretsfile.rs:257` → `fsutil::write_private_atomic(manifest_path, &contents, 0o600)`
- `crates/infrastructure/src/secretsfile.rs:296` → `fsutil::write_private_atomic(&env_path, contents.as_bytes(), 0o600)`

- [ ] **Step 5: Run the tests**

Run: `rtk cargo test --locked -p pi-infrastructure`
Expected: PASS, including the pre-existing `secretsfile` mode assertions (they still expect `0600`).

- [ ] **Step 6: Commit**

```bash
rtk git add crates/infrastructure/src/fsutil.rs crates/infrastructure/src/secrets.rs crates/infrastructure/src/secretsfile.rs
rtk git commit -m "refactor(infra): make write_private_atomic take an explicit mode"
```

---

### Task 2: Mode value type, parsing and validation

**Files:**
- Create: `crates/domain/src/secretmode.rs`
- Modify: `crates/domain/src/lib.rs` (add `pub mod secretmode;`)
- Test: inside `crates/domain/src/secretmode.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_SECRET_FILE_MODE: u32 = 0o644;`
  - `pub const DEFAULT_ENV_MODE: u32 = 0o600;`
  - `pub fn parse(text: &str) -> Result<u32, String>` — accepts `^0?[0-7]{3}$`, then applies `validate`.
  - `pub fn validate(mode: u32) -> Result<(), String>` — the bit rule.
- Consumed later by: Task 3 (writer defaults), Task 4 (`rpi.toml` validation), Task 5 (agent-side re-validation).

- [ ] **Step 1: Write the failing test**

Create `crates/domain/src/secretmode.rs` with the test module only (implementation comes in step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_modes() {
        for (text, expected) in [
            ("0600", 0o600),
            ("600", 0o600),
            ("0640", 0o640),
            ("0644", 0o644),
            ("0400", 0o400),
            ("0440", 0o440),
            ("0444", 0o444),
            ("0604", 0o604),
        ] {
            assert_eq!(parse(text), Ok(expected), "{text}");
        }
    }

    #[test]
    fn rejects_execute_bits() {
        let err = parse("0755").unwrap_err();
        assert!(err.contains("execute"), "{err}");
    }

    #[test]
    fn rejects_write_for_group_or_other() {
        assert!(parse("0660").unwrap_err().contains("writable"));
        assert!(parse("0666").unwrap_err().contains("writable"));
    }

    #[test]
    fn rejects_a_mode_the_owner_cannot_read() {
        assert!(parse("0244").unwrap_err().contains("owner"));
    }

    #[test]
    fn rejects_setuid_setgid_and_sticky_by_shape() {
        for text in ["4644", "2644", "1644", "04644"] {
            assert!(parse(text).is_err(), "{text} must be rejected");
        }
    }

    #[test]
    fn rejects_malformed_text() {
        for text in ["", "0", "64", "0648", "0o644", "644 ", "abc"] {
            assert!(parse(text).is_err(), "{text} must be rejected");
        }
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `rtk cargo test -p pi-domain secretmode`
Expected: FAIL — module not registered / `parse` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/domain/src/secretmode.rs`:

```rust
//! The file mode `rpi` gives to materialized secrets.
//!
//! Secret files are consumed by containers whose uid is unrelated to the
//! agent's, so the default has to be readable by others; the exact value is
//! configurable per project via `[secrets].file_mode`. The permitted set is
//! described by a rule rather than a list: the owner reads (and may write),
//! group and others may only read.

/// Mode for files listed in `[secrets].files` when no `file_mode` is set.
pub const DEFAULT_SECRET_FILE_MODE: u32 = 0o644;

/// Mode for the injected `.env` when no `file_mode` is set. Compose reads it
/// as the agent, so nothing needs it wider by default.
pub const DEFAULT_ENV_MODE: u32 = 0o600;

/// Parses `"0644"` / `"644"` into `0o644` and validates it.
pub fn parse(text: &str) -> Result<u32, String> {
    let digits = match text.len() {
        3 => text,
        4 if text.starts_with('0') => &text[1..],
        _ => {
            return Err(format!(
                "'{text}' is not a three-digit octal file mode (e.g. \"0644\")"
            ));
        }
    };
    let mut mode = 0u32;
    for c in digits.chars() {
        let digit = c
            .to_digit(8)
            .ok_or_else(|| format!("'{text}' is not a three-digit octal file mode (e.g. \"0644\")"))?;
        mode = mode * 8 + digit;
    }
    validate(mode)?;
    Ok(mode)
}

/// The bit rule, applied to an already-parsed mode: the owner must be able to
/// read and may write; group and others may only read. Execute bits are
/// refused because a secret is not a program, and write for anyone but the
/// owner because `rpi` overwrites the file on every deploy anyway.
pub fn validate(mode: u32) -> Result<(), String> {
    if mode & !0o777 != 0 {
        return Err(format!(
            "mode {mode:04o} sets setuid/setgid/sticky bits, which are not allowed for secret files"
        ));
    }
    if mode & 0o111 != 0 {
        return Err(format!(
            "mode {mode:04o} sets execute bits, which are not allowed for secret files"
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "mode {mode:04o} is writable by group or others, which is not allowed for secret files"
        ));
    }
    if mode & 0o400 == 0 {
        return Err(format!(
            "mode {mode:04o} is not readable by its owner (the agent), which cannot be right"
        ));
    }
    Ok(())
}
```

Add `pub mod secretmode;` to `crates/domain/src/lib.rs`, keeping the existing module list alphabetical if it already is.

- [ ] **Step 4: Run the tests**

Run: `rtk cargo test --locked -p pi-domain`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/domain/src/secretmode.rs crates/domain/src/lib.rs
rtk git commit -m "feat(domain): add secret file mode parsing and validation"
```

---

### Task 3: Bundle carries the mode; store round-trips it

**Files:**
- Modify: `crates/domain/src/entities.rs:10-31`
- Modify: `crates/infrastructure/src/secrets.rs:22-27,93-141`
- Modify (struct literals that must gain the new field): `crates/bin/src/agent/http.rs:748,824`
- Test: `crates/infrastructure/src/secrets.rs` tests module

**Interfaces:**
- Consumes: `pi_domain::secretmode::{DEFAULT_ENV_MODE, DEFAULT_SECRET_FILE_MODE}` (Task 2).
- Produces:
  - `SecretsBundle.file_mode: Option<u32>` (public field, `Default` = `None`)
  - `SecretsBundle::secret_file_mode(&self) -> u32`
  - `SecretsBundle::env_mode(&self) -> u32`

- [ ] **Step 1: Write the failing test**

Add to the tests module of `crates/infrastructure/src/secrets.rs`:

```rust
    #[tokio::test]
    async fn file_mode_survives_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let mut b = bundle();
        b.file_mode = Some(0o640);
        store.save("rateme", &b).await.unwrap();
        let loaded = store.load("rateme").await.unwrap();
        assert_eq!(loaded.file_mode, Some(0o640));
    }

    #[tokio::test]
    async fn a_bundle_stored_before_modes_existed_loads_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store.save("rateme", &bundle()).await.unwrap();
        let loaded = store.load("rateme").await.unwrap();
        assert_eq!(loaded.file_mode, None);
        assert_eq!(loaded.secret_file_mode(), 0o644);
        assert_eq!(loaded.env_mode(), 0o600);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `rtk cargo test -p pi-infrastructure secrets::`
Expected: FAIL — `SecretsBundle` has no field `file_mode`.

- [ ] **Step 3: Implement the domain change**

In `crates/domain/src/entities.rs`, extend the struct and its impl:

```rust
pub struct SecretsBundle {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> raw file bytes.
    pub files: BTreeMap<String, Vec<u8>>,
    /// `[secrets].file_mode`, when the project set one. `None` means the
    /// defaults in `crate::secretmode` apply.
    pub file_mode: Option<u32>,
}
```

and, inside `impl SecretsBundle`:

```rust
    /// Mode for files from `[secrets].files`.
    pub fn secret_file_mode(&self) -> u32 {
        self.file_mode
            .unwrap_or(crate::secretmode::DEFAULT_SECRET_FILE_MODE)
    }

    /// Mode for the injected `.env`. Only widened when the project asked for
    /// it explicitly.
    pub fn env_mode(&self) -> u32 {
        self.file_mode.unwrap_or(crate::secretmode::DEFAULT_ENV_MODE)
    }
```

Leave the custom `Debug` impl alone: it must keep printing key names and file paths only, never the mode-free values.

- [ ] **Step 4: Implement the store change**

In `crates/infrastructure/src/secrets.rs`:

```rust
struct StoredBundle {
    vars: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Absent in bundles written before 0.26.0 — those load as `None` and get
    /// the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_mode: Option<u32>,
}
```

In `save`, add `file_mode: bundle.file_mode,` to the `StoredBundle` literal. In `load`, add `file_mode: stored.file_mode,` to the `SecretsBundle` literal at `:137`.

- [ ] **Step 5: Fix the two agent-side struct literals**

`crates/bin/src/agent/http.rs:748` (legacy `/env` route — no mode on that wire) and `:824` (secrets route) each gain `file_mode: None,` for now; Task 5 replaces the value at `:824`.

- [ ] **Step 6: Run the tests**

Run: `rtk cargo test --locked`
Expected: PASS. Test helpers that build `SecretsBundle` with `..Default::default()` or field-by-field assignment need no change; any that use an exhaustive literal must gain `file_mode: None`.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/domain/src/entities.rs crates/infrastructure/src/secrets.rs crates/bin/src/agent/http.rs
rtk git commit -m "feat(domain): carry file_mode on SecretsBundle and persist it"
```

---

### Task 4: Writer applies modes and the directory policy

**Files:**
- Modify: `crates/infrastructure/src/secretsfile.rs:32-46,58-67,92-138,213-261,276-313`
- Test: `crates/infrastructure/src/secretsfile.rs` tests module (update `secret_files_and_created_dirs_are_private`, add new tests)

**Interfaces:**
- Consumes: `SecretsBundle::secret_file_mode()`, `SecretsBundle::env_mode()` (Task 3).
- Produces: no new public API — behaviour only.

- [ ] **Step 1: Write the failing tests**

In the tests module of `crates/infrastructure/src/secretsfile.rs`, replace `secret_files_and_created_dirs_are_private` with:

```rust
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
```

Keep `manifest_file_is_0600` and `env_file_is_0600_even_when_replacing_a_wider_one` exactly as they are — both must still pass.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-infrastructure secretsfile`
Expected: FAIL — files are `0600`, directories `0700`, nothing is widened.

- [ ] **Step 3: Implement the directory policy**

In `crates/infrastructure/src/secretsfile.rs`, replace `create_private_dir` with:

```rust
/// Mode for directories this writer creates on the way to a secret file.
/// Traversable by anyone, like every other directory in the checkout: a
/// directory mode protects nothing here, the file mode does.
const DIR_MODE: u32 = 0o755;

fn create_secret_dir(path: &Path) -> std::io::Result<()> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

/// Widens a directory this writer created before 0.26.0 (owned by us and at
/// exactly `0700`) so a container can traverse into it. Anything else — a
/// directory from the repository, one with another owner, one with any other
/// mode — is left untouched: it is not ours to change. Best-effort; a failure
/// to stat or chmod is not worth failing a deploy over.
#[cfg(unix)]
fn widen_legacy_secret_dir(path: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.permissions().mode() & 0o777 != 0o700 {
        return;
    }
    // Same idiom as `cloudflared.rs:104`.
    if meta.uid() != unsafe { libc::geteuid() } {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE));
}

#[cfg(not(unix))]
fn widen_legacy_secret_dir(_path: &Path) {}
```

`libc = "0.2"` is already a dependency of `pi-infrastructure` (`crates/infrastructure/Cargo.toml:29`) and `unsafe { libc::getuid() }` is already used at `crates/infrastructure/src/cloudflared.rs:104`, so this needs no new dependency and matches existing practice.

In `write_files_blocking`, the directory loop becomes:

```rust
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
```

- [ ] **Step 4: Implement the file modes**

`write_files_blocking` gains a `mode: u32` parameter and passes it at `:134`:

```rust
        fsutil::write_private_atomic(&target, &bytes, mode)
```

`sync_files_blocking` gains the same parameter and forwards it. In `SecretsWriter::write`:

- the `.env` write uses `bundle.env_mode()`;
- the files write passes `bundle.secret_file_mode()`;
- `sync_manifest_blocking`'s `write_private_atomic` call at `:257` keeps `0o600` — the manifest is this writer's bookkeeping, not a project secret.

Update the struct-level doc comment at `:32-46` so it states the new modes (files: `file_mode` or `0644`; dirs `0755`; `.env` `file_mode` or `0600`; manifest `0600`).

- [ ] **Step 5: Run the tests**

Run: `rtk cargo test --locked -p pi-infrastructure`
Expected: PASS, including all four pre-existing symlink-escape tests, which must not be modified.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/infrastructure/src/secretsfile.rs
rtk git commit -m "feat(infra): write secret files container-readable and widen legacy dirs"
```

---

### Task 5: `[secrets].file_mode` in rpi.toml and overlays

**Files:**
- Modify: `crates/bin/src/cli/rpitoml.rs:111-121` (`SecretsSection`), `:261-306` (`validate_common`)
- Modify: `crates/bin/src/cli/overlay.rs:145-150` (`OverlaySecrets`), `:383-390` (the `secrets` arm of `apply_overlay`)
- Test: the tests modules of both files

**Interfaces:**
- Consumes: `pi_domain::secretmode::parse` (Task 2).
- Produces: `SecretsSection.file_mode: Option<String>` — the raw text; parsing to `u32` happens in Task 6 at send time.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/cli/rpitoml.rs` tests. The fixture there is `SAMPLE`, whose
`[secrets]` block is `env = ".env"` followed by `files = ["certs/server.pem"]`:

```rust
    #[test]
    fn secrets_file_mode_is_parsed_and_validated() {
        let parsed = RpiToml::parse(&SAMPLE.replace(
            "files = [\"certs/server.pem\"]",
            "files = [\"certs/server.pem\"]\nfile_mode = \"0640\"",
        ))
        .unwrap();
        assert_eq!(parsed.secrets.file_mode.as_deref(), Some("0640"));
    }

    #[test]
    fn secrets_file_mode_defaults_to_absent() {
        assert!(RpiToml::parse(SAMPLE).unwrap().secrets.file_mode.is_none());
    }

    #[test]
    fn invalid_secrets_file_mode_is_rejected() {
        for bad in ["0755", "0666", "abc", "64"] {
            let err = RpiToml::parse(&SAMPLE.replace(
                "files = [\"certs/server.pem\"]",
                &format!("files = [\"certs/server.pem\"]\nfile_mode = \"{bad}\""),
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("[secrets].file_mode"), "{bad}: {err}");
        }
    }
```

In `crates/bin/src/cli/overlay.rs` tests:

```rust
    #[test]
    fn overlay_sets_and_resets_secrets_file_mode() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        apply_overlay(&mut base, overlay("[secrets]\nfile_mode = \"0640\"\n"));
        assert_eq!(base.secrets.file_mode.as_deref(), Some("0640"));
        apply_overlay(&mut base, overlay("[secrets]\nfile_mode = \"\"\n"));
        assert_eq!(base.secrets.file_mode, None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi file_mode`
Expected: FAIL — unknown field `file_mode`.

- [ ] **Step 3: Implement**

`SecretsSection` gains:

```rust
    /// Mode for the materialized secrets, e.g. "0640". None -> 0644 for
    /// `[secrets].files` and 0600 for the injected `.env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<String>,
```

`OverlaySecrets` gains `pub file_mode: Option<String>,`, and the `secrets` arm of `apply_overlay` gains:

```rust
        if let Some(mode) = s.file_mode {
            base.secrets.file_mode = reset_or(mode);
        }
```

`validate_common` gains, right after the `[secrets].files` loop:

```rust
        if let Some(mode) = &self.secrets.file_mode {
            pi_domain::secretmode::parse(mode)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].file_mode: {e}"))?;
        }
```

If `crates/bin` does not already depend on `pi-domain`, it does — `proto.rs` imports `pi_domain::entities`; use the same crate name spelling as the existing imports.

- [ ] **Step 4: Run the tests**

Run: `rtk cargo test --locked -p pi`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/bin/src/cli/rpitoml.rs crates/bin/src/cli/overlay.rs
rtk git commit -m "feat(cli): accept [secrets].file_mode in rpi.toml and overlays"
```

---

### Task 6: Wire the mode to the agent, gated by a capability

**Files:**
- Modify: `crates/bin/src/compat.rs:27-86` (new `Feature` variant)
- Modify: `crates/bin/src/proto.rs:150-158` (`SecretsSendRequest`)
- Modify: `crates/bin/src/cli/api.rs:328-346` (`send_secrets`) and its test at `:725`
- Modify: `crates/bin/src/cli/commands.rs:183-217` (`secrets_send`)
- Modify: `crates/bin/src/agent/http.rs:775-836` (secrets handler)
- Test: `crates/bin/src/compat.rs`, `crates/bin/src/agent/http.rs` tests modules

**Interfaces:**
- Consumes: `SecretsSection.file_mode` (Task 5), `pi_domain::secretmode::{parse, validate}` (Task 2), `SecretsBundle.file_mode` (Task 3).
- Produces: `ApiClient::send_secrets(&self, project: &str, vars: BTreeMap<String, String>, files: BTreeMap<String, String>, file_mode: Option<u32>, apply: bool) -> anyhow::Result<SecretsSendResponse>`.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/agent/http.rs` tests, next to `secrets_send_then_ls_roundtrip`
(`:1900`), reusing that test's helpers verbatim:

```rust
    #[tokio::test]
    async fn secrets_send_rejects_an_invalid_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_with(
            dir.path(),
            Arc::new(ok_source()),
            Arc::new(ok_runtime()),
        ));
        let body = serde_json::json!({
            "vars": { "DB_PASSWORD": "hunter2-long" },
            "files": {},
            "file_mode": 0o755,
            "apply": false
        });
        let (status, _) =
            request(app, put_json("/v1/projects/rateme/secrets", &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn secrets_send_accepts_a_valid_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_with(
            dir.path(),
            Arc::new(ok_source()),
            Arc::new(ok_runtime()),
        ));
        let body = serde_json::json!({
            "vars": { "DB_PASSWORD": "hunter2-long" },
            "files": {},
            "file_mode": 0o640,
            "apply": false
        });
        let (status, json) =
            request(app, put_json("/v1/projects/rateme/secrets", &body)).await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }
```

In `crates/bin/src/compat.rs` tests, extend whatever test enumerates the registry so the new feature is covered; if none exists, add:

```rust
    #[test]
    fn secret_modes_is_registered_and_required() {
        assert!(Feature::ALL.contains(&Feature::SecretModes));
        assert_eq!(Feature::SecretModes.capability(), "secret-modes");
        assert_eq!(Feature::SecretModes.since(), "0.26.0");
        assert!(matches!(Feature::SecretModes.policy(), Policy::Required));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi secret_modes`
Expected: FAIL — no such variant.

- [ ] **Step 3: Register the feature**

In `crates/bin/src/compat.rs`, add `SecretModes` to the enum, to `Feature::ALL`, and to the `capability()` / `label()` / `policy()` / `since()` matches:

- `capability()` → `"secret-modes"`
- `label()` → `"secret file modes"`
- `policy()` → `Policy::Required`
- `since()` → `"0.26.0"`

`Feature::advertised()` derives from `ALL`, so the agent starts advertising it automatically and the `version_advertises_every_registered_feature` drift guard keeps passing.

- [ ] **Step 4: Extend the wire type and the client**

`crates/bin/src/proto.rs`:

```rust
pub struct SecretsSendRequest {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> base64-encoded contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// `[secrets].file_mode`, already parsed. Absent -> agent defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    #[serde(default)]
    pub apply: bool,
}
```

`crates/bin/src/cli/api.rs`: add the `file_mode: Option<u32>` parameter between `files` and `apply`, put it in the request literal, and update the existing call in the test at `:725` to pass `None`.

- [ ] **Step 5: Gate and send from the CLI**

In `crates/bin/src/cli/commands.rs::secrets_send`, after the existing gates:

```rust
    let file_mode = match &rpitoml.secrets.file_mode {
        Some(text) => Some(
            pi_domain::secretmode::parse(text)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].file_mode: {e}"))?,
        ),
        None => None,
    };
    if file_mode.is_some() {
        compat.gate(crate::compat::Feature::SecretModes)?;
    }
```

and pass `file_mode` to `api.send_secrets(...)`.

- [ ] **Step 6: Validate and apply on the agent**

In the secrets handler in `crates/bin/src/agent/http.rs`, before building the bundle:

```rust
    if let Some(mode) = req.file_mode {
        pi_domain::secretmode::validate(mode)
            .map_err(|e| ApiError(DomainError::Invalid(format!("[secrets].file_mode: {e}"))))?;
    }
```

and set `file_mode: req.file_mode,` in the `SecretsBundle` literal at `:824` (the one at `:748`, the legacy `/env` route, keeps `None`).

- [ ] **Step 7: Run the tests**

Run: `rtk cargo test --locked`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
rtk git add crates/bin/src/compat.rs crates/bin/src/proto.rs crates/bin/src/cli/api.rs crates/bin/src/cli/commands.rs crates/bin/src/agent/http.rs
rtk git commit -m "feat(cli): send [secrets].file_mode behind the secret-modes capability"
```

---

### Task 7: `/var/lib/rpi` at 0750, plus a doctor check

**Files:**
- Modify: `crates/bin/src/agent/setup.rs:168-215` (`ensure_dir`), `:410-412` (call sites)
- Modify: `crates/infrastructure/src/probe.rs` (new check next to `rpi-agent group` at `:193`)
- Test: the tests modules of both files

**Interfaces:**
- Produces: `ensure_dir(sys, path, owner_group, mode: Option<&str>, dry, rep, repair)` — `mode` is the `install -m` argument and the target of the repair branch.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/agent/setup.rs` tests, next to
`ensure_dir_repairs_ownership_when_uid_drifted` (`:1865`), using the same
`fresh_sys()` / `SetupOpts` / `setup(&sys, &opts).await` shape:

```rust
    #[tokio::test]
    async fn data_dir_is_created_at_mode_0750() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let _ = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "install -d -m 0750 -o rpi-agent -g rpi-agent /var/lib/rpi"),
            "calls: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "install -d -o rpi-agent -g rpi-agent /var/log/rpi"),
            "only the data dir gets a mode: {calls:?}"
        );
    }

    #[tokio::test]
    async fn existing_data_dir_with_a_wider_mode_is_repaired() {
        let mut sys = fresh_sys();
        sys.paths.insert("/var/lib/rpi".into());
        // Ownership is already correct, so only the mode drifts.
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "755".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(
            sys.calls().iter().any(|c| c == "chmod 0750 /var/lib/rpi"),
            "calls: {:?}",
            sys.calls()
        );
        assert!(
            report.repaired.iter().any(|r| r.contains("/var/lib/rpi (mode)")),
            "mode repair reported: {:?}",
            report.repaired
        );
    }

    #[tokio::test]
    async fn a_data_dir_already_at_0750_is_left_alone() {
        let mut sys = fresh_sys();
        sys.paths.insert("/var/lib/rpi".into());
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "750".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(!sys.calls().iter().any(|c| c.starts_with("chmod 0750")));
        assert!(report.skipped.iter().any(|s| s == "/var/lib/rpi"));
    }
```

Two existing tests in this module break on the new command shape and must be
updated in the same commit:

- `ensure_dir_repairs_ownership_when_uid_drifted` (`:1865`) asserts
  `report.repaired` contains the exact string `"/var/lib/rpi (ownership)"`.
  With the mode unset in its `FakeSys`, the mode would also be repaired and the
  string would become `"/var/lib/rpi (ownership, mode)"`. Add
  `sys.ok.insert(FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]), "750".into());`
  to that test so it stays about ownership only.
- `mkdir_failure_records_error_not_created` (`:2106`) registers the failure
  under the exact key
  `install -d -o rpi-agent -g rpi-agent /var/lib/rpi`; it becomes
  `install -d -m 0750 -o rpi-agent -g rpi-agent /var/lib/rpi`.

In `crates/infrastructure/src/probe.rs` tests:

```rust
    #[test]
    fn data_dir_permissions_check_fails_on_a_world_readable_dir() {
        let check = data_dir_permissions_check(Ok("rpi-agent 755".into()));
        assert!(!check.passed);
        assert!(check.hint.unwrap().contains("rpi agent setup"));
    }

    #[test]
    fn data_dir_permissions_check_passes_at_0750() {
        assert!(data_dir_permissions_check(Ok("rpi-agent 750".into())).passed);
    }

    #[test]
    fn data_dir_permissions_check_passes_when_tighter() {
        assert!(data_dir_permissions_check(Ok("rpi-agent 700".into())).passed);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test --locked -p pi setup:: && rtk cargo test --locked -p pi-infrastructure probe`
Expected: FAIL — no `mode` parameter, no such check function.

- [ ] **Step 3: Implement the setup change**

`ensure_dir` gains a `mode: Option<&str>` parameter, inserted after
`owner_group`. The creation branch builds its args as:

```rust
    let mut args: Vec<&str> = vec!["-d"];
    if let Some(m) = mode {
        args.push("-m");
        args.push(m);
    }
    if let Some(og) = owner_group {
        args.extend(["-o", og, "-g", og]);
    }
    args.push(path);
```

The "already exists" branch is restructured so a mode repair is not skipped by
an ownership repair returning early — today it does `return` the moment it
chowns:

```rust
    if sys.exists(Path::new(path)) {
        let mut repairs: Vec<&str> = Vec::new();
        if let Some(og) = owner_group {
            if !dry {
                let want = format!("{og}:{og}");
                let cur = sys.run("stat", &["-c", "%U:%G", path]).await;
                if cur.ok().as_deref().map(str::trim) != Some(want.as_str())
                    && sys.run("chown", &["-R", &want, path]).await.is_ok()
                {
                    repairs.push("ownership");
                }
            }
        }
        if let Some(m) = mode {
            if !dry {
                // `stat -c %a` prints no leading zero, hence the trim.
                let cur = sys.run("stat", &["-c", "%a", path]).await;
                if cur.ok().as_deref().map(str::trim) != Some(m.trim_start_matches('0'))
                    && sys.run("chmod", &[m, path]).await.is_ok()
                {
                    repairs.push("mode");
                }
            }
        }
        if repairs.is_empty() {
            rep.skipped.push(path.to_string());
        } else {
            rep.repaired.push(format!("{path} ({})", repairs.join(", ")));
        }
        return;
    }
```

Call sites at `:410-412`: `/var/lib/rpi` passes `Some("0750")`; `/var/log/rpi`
and `/etc/rpi` pass `None`.

- [ ] **Step 4: Implement the doctor check**

In `crates/infrastructure/src/probe.rs`, add a pure function plus its call site, mirroring the shape of `memory_cgroup_check` at `:72`:

```rust
/// `/var/lib/rpi` holds every project's plaintext secrets. It must be owned by
/// the agent and no wider than 0750; the file modes inside it are deliberately
/// container-readable, so this directory is what keeps other local users out.
fn data_dir_permissions_check(stat: Result<String, String>) -> DiagnosticCheck {
    let hint = "tighten the agent data dir: sudo rpi agent setup";
    match stat {
        Ok(out) => {
            let mut parts = out.split_whitespace();
            let owner = parts.next().unwrap_or_default().to_string();
            let mode = parts.next().unwrap_or_default().to_string();
            let wide = mode.chars().last().is_some_and(|c| c != '0');
            let passed = owner == "rpi-agent" && !wide;
            DiagnosticCheck {
                name: "data dir permissions".into(),
                passed,
                detail: format!("/var/lib/rpi: owner {owner}, mode {mode}"),
                hint: if passed { None } else { Some(hint.into()) },
            }
        }
        Err(err) => DiagnosticCheck {
            name: "data dir permissions".into(),
            passed: false,
            detail: err,
            hint: Some(hint.into()),
        },
    }
}
```

Wire it in where the other checks are pushed, with `self.runner.run("stat", &["-c", "%U %a", "/var/lib/rpi"]).await`.

- [ ] **Step 5: Run the tests**

Run: `rtk cargo test --locked`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/bin/src/agent/setup.rs crates/infrastructure/src/probe.rs
rtk git commit -m "feat(agent): tighten /var/lib/rpi to 0750 and check it in doctor"
```

---

### Task 8: Report the mode where it can be seen

**Files:**
- Modify: `crates/application/src/deploy.rs:265-275` (injection log line)
- Modify: `crates/application/src/secrets.rs:50-107` (`SendSecrets::execute` log line), `:112-134` (`StoredSecrets`)
- Modify: `crates/bin/src/proto.rs:167-171` (`SecretsListResponse`), `crates/bin/src/agent/http.rs` (list handler), `crates/bin/src/cli/commands.rs:313-340` (`secrets_ls` output)
- Test: the tests modules of `crates/application/src/deploy.rs` and `crates/application/src/secrets.rs`

**Interfaces:**
- Consumes: `SecretsBundle::secret_file_mode()` (Task 3).
- Produces: `StoredSecrets.file_mode: u32` — the effective mode (the bundle's, or the `0644` default).

- [ ] **Step 1: Write the failing tests**

In `crates/application/src/secrets.rs` tests, extend `apply_reinjects_env_and_runs_up_with_masked_logs` with:

```rust
        assert!(
            lines.iter().any(|l| l.contains("mode 0644")),
            "the applied mode must be visible in the log: {lines:?}"
        );
```

and add:

```rust
    #[tokio::test]
    async fn list_secrets_reports_the_effective_file_mode() {
        let mut secrets = MockSecretStore::new();
        secrets.expect_load().returning(|_| Ok(bundle()));
        let stored = ListSecrets::new(Arc::new(secrets))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(stored.file_mode, 0o644);
    }
```

In `crates/application/src/deploy.rs`, the assertion at `:1175` currently reads
`.contains("secrets injected (1 keys, 1 files)")`; change it to
`.contains("secrets injected (1 keys, 1 files, mode 0644)")`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test --locked -p pi-application`
Expected: FAIL — no `file_mode` on `StoredSecrets`, no mode in the log lines.

- [ ] **Step 3: Implement**

`deploy.rs`:

```rust
            log.line(&format!(
                "secrets injected ({} keys, {} files, mode {:04o})",
                bundle.vars.len(),
                bundle.files.len(),
                bundle.secret_file_mode()
            ));
```

`SendSecrets::execute`, right after `self.writer.write(&workdir, &bundle).await?;`:

```rust
        log.line(&format!(
            "secrets applied ({} keys, {} files, mode {:04o})",
            keys,
            files,
            bundle.secret_file_mode()
        ));
```

`StoredSecrets` gains `pub file_mode: u32,`, filled by `ListSecrets::execute`
with `bundle.secret_file_mode()`.

`SecretsListResponse` gains:

```rust
    /// Absent from agents older than 0.26.0 — the CLI then prints no mode
    /// line rather than guessing a value that host never wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
```

`Option` with a serde default is load-bearing here: `rpi secrets ls` runs
against whatever agent is on the Pi, and a bare `u32` would make the response
of every older agent fail to deserialize. The list handler sets
`file_mode: Some(stored.file_mode)`.

`secrets_ls` prints one extra line next to the existing key/file output, before
the per-item listing:

```rust
    if let Some(mode) = resp.file_mode {
        output::info(format!("file mode: {mode:04o}"));
    }
```

- [ ] **Step 4: Run the tests**

Run: `rtk cargo test --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
rtk git add crates/application/src/deploy.rs crates/application/src/secrets.rs crates/bin/src/proto.rs crates/bin/src/agent/http.rs crates/bin/src/cli/commands.rs
rtk git commit -m "feat: report the effective secret file mode in logs and secrets ls"
```

---

### Task 9: End-to-end scenario

**Files:**
- Create: `tests/e2e/scenarios/secret-file-perms/scenario.sh`
- Create: `tests/e2e/scenarios/secret-file-perms/app/rpi.toml`
- Create: `tests/e2e/scenarios/secret-file-perms/app/compose.yaml`

**Interfaces:**
- Consumes: everything above, through the real CLI and agent.
- Scenarios are auto-discovered by folder name (`tests/e2e/run.mjs:49`), and `tests/e2e/Dockerfile:49-52` copies `app.default/` into each scenario's `app/` without clobbering, so only the overridden files are created here. The client container's working directory is the scenario's `app/` dir (`tests/e2e/base.compose.yaml:125`).

- [ ] **Step 1: Write the fixture**

`tests/e2e/scenarios/secret-file-perms/app/rpi.toml`:

```toml
schema = 1

[project]
name = "e2e-fixture"

[source]
repo = "git://git-fixture/fixture.git"
branch = "main"

[build]
compose = "compose.yaml"

[ingress]
service = "web"
port = 8080

[healthcheck]
path = "/health"
expect = "200"
timeout = "30s"

# The file itself is created by scenario.sh at run time, so it never enters
# the fixture git repo: the CLI reads it locally, the agent materializes it.
[secrets]
files = ["app_secret"]

[commands]
read-secret = ["sh", "-c", "cat /run/secrets/app_secret"]
```

`tests/e2e/scenarios/secret-file-perms/app/compose.yaml`:

```yaml
services:
  web:
    build: .
    # The whole point: a service that is not the agent's uid. Before this
    # change the bind-mounted 0600 secret was unreadable here.
    user: "1000:1000"
    expose:
      - "8080"
    secrets:
      - app_secret

secrets:
  app_secret:
    file: ./app_secret
```

- [ ] **Step 2: Write the scenario**

`tests/e2e/scenarios/secret-file-perms/scenario.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Secret-file-modes spec: a container running as a uid other than the agent's
# must be able to read a bind-mounted compose secret. rpi materializes
# [secrets].files itself, so the mode it picks is the only thing that decides
# this — compose silently ignores mode/uid/gid for file-sourced secrets
# outside Swarm.

source /opt/e2e/lib.sh
e2e_bootstrap

# Created after the git fixture was built, so this is a local secret the CLI
# uploads, not a file committed to the repository.
printf 'top-secret-value\n' > app_secret

run_capture send.log rpi secrets send "${CONNECT[@]}"
assert_log send.log 'saved 0 key(s) and 1 file(s)'

run_capture deploy.log rpi deploy "${CONNECT[@]}"
assert_deploy_log deploy.log
assert_log deploy.log 'mode 0644'

# The container reads it as uid 1000. Before the fix this failed with EACCES.
run_capture read.log rpi command read-secret "${CONNECT[@]}"
assert_log read.log 'top-secret-value'

run_capture ls.log rpi secrets ls "${CONNECT[@]}"
assert_log ls.log 'file mode: 0644'

echo 'rpi e2e: PASS'
```

- [ ] **Step 3: Run the scenario**

Run: `node tests/e2e/run.mjs secret-file-perms`
Expected: PASS. (Requires a working Docker engine; if the harness cannot run in this environment, say so explicitly rather than marking the step done — do not silently skip it.)

- [ ] **Step 4: Commit**

```bash
rtk git add tests/e2e/scenarios/secret-file-perms
rtk git commit -m "test(e2e): container reads a bind-mounted secret as a non-agent uid"
```

---

### Task 10: Documentation

**Files:**
- Modify: `docs/architecture/flows/secrets.md` (the two `mode 0600` labels in the diagram, walkthrough item 4, the `secretsfile.rs` source anchor)
- Modify: `README.md` (the `[secrets]` section)
- Modify: `plugins/rpi/skills/rpi-toml/SKILL.md`, `plugins/rpi/skills/rpi-cli/SKILL.md`
- Modify: `docs/superpowers/specs/2026-07-07-secret-files-design.md` (one line marking the file-mode YAGNI item superseded)

- [ ] **Step 1: Update the architecture flow**

Read `docs/architecture/flows/secrets.md` end to end, then, following the `architecture-diagrams` skill's conventions: change both `write .env + secret files ... mode 0600` diagram labels to say `.env 0600, files 0644 (or [secrets].file_mode)`, rewrite walkthrough item 4 to state which artifact gets which mode and why, and update the `crates/infrastructure/src/secretsfile.rs` source anchor line. Add one sentence to item 3 or 4 noting that `/var/lib/rpi` itself is `0750` and is what keeps other local users out.

- [ ] **Step 2: Document `file_mode` for users**

Add `file_mode` to the `[secrets]` documentation in `README.md` and `plugins/rpi/skills/rpi-toml/SKILL.md`: what it defaults to, which values are accepted, and the one-line reason it exists (a container that is not the agent's uid must be able to read the file).

State explicitly that the mode travels with the bundle: changing `file_mode` takes effect on the next `rpi secrets send`, not on a `rpi deploy` that reuses the stored bundle. Without this line the first question every user asks is why their new mode did nothing.

In `plugins/rpi/skills/rpi-cli/SKILL.md`, note that `rpi secrets ls` reports the effective mode and that `file_mode` requires an agent >= 0.26.0.

- [ ] **Step 3: Mark the old decision superseded**

In `docs/superpowers/specs/2026-07-07-secret-files-design.md`, on the YAGNI line "Права/exec-биты файлов. Всё пишется 0600, каталоги 0700.", append a pointer: superseded by `docs/superpowers/specs/2026-07-25-secret-file-modes-design.md`.

- [ ] **Step 4: Full verification**

Run, in order:

```bash
rtk cargo fmt --all -- --check
rtk cargo clippy --all-targets --locked -- -D warnings
rtk cargo test --locked
```

Expected: all three clean.

- [ ] **Step 5: Commit**

```bash
rtk git add docs README.md plugins
rtk git commit -m "docs: secret file modes"
```

---

## Notes for the release that follows

- The version bump to `0.26.0` belongs to the release, not to these tasks — but `Feature::SecretModes.since()` already claims `0.26.0`, so the release must not pick a different number.
- Release-note framing: updating only the CLI changes nothing; the mode is written by the agent, so the fix lands when the Pi is updated.

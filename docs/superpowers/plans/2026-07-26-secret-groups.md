# Secret Groups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store a project's secrets as named, reusable groups on the agent and attach them to a deploy declaratively, so a new branch preview no longer needs its secrets re-uploaded.

**Architecture:** A secret group is a named set of secret objects owned by a base project, stored age-encrypted at `<data_dir>/secrets/groups/<base>/<group>.age` with a monotonic revision. `[secrets].groups` in `rpi.toml` declares which groups a deploy attaches; the deploy stage loads them in declared order, then layers the deploy key's own bundle (`<data_dir>/secrets/<key>.secrets.age`, path unchanged) on top and merges per object. Writes are conditional on the expected revision, and values never leave the agent — only names, sizes and digests.

**Tech Stack:** Rust 2021 workspace (`pi-domain`, `pi-infrastructure`, `pi-application`, `rpi` bin), axum + reqwest, rusqlite + rusqlite_migration, `age` for encryption, `sha2` for digests, mockall for unit tests, bash e2e scenarios under `tests/e2e/scenarios/`.

**Spec:** `docs/superpowers/specs/2026-07-26-secret-groups-design.md`

## Global Constraints

- Every task ends green on all three gates from `CLAUDE.md`: `rtk cargo fmt --all -- --check`, `rtk cargo clippy --all-targets --locked -- -D warnings`, `rtk cargo test --locked`. A `fmt` diff is fixed by running `rtk cargo fmt --all`, never by hand-editing.
- Baseline at plan time: 714 tests passing, 0 failures. No task may reduce that count.
- No secret value may appear in any HTTP response, CLI output, log line or error message. Masking is armed on the **merged** bundle before container output streams.
- Group names match `^[a-z][a-z0-9-]*$`, max 40 characters. The same rule is enforced by the `rpi.toml` parser and by the agent, with the same message on both sides.
- Digest is the first 16 hex characters of SHA-256 over the raw value bytes.
- Limits are unchanged and reused, never redefined: `MAX_SECRET_FILE_BYTES` = 1 MiB per file, `MAX_SECRETS_BUNDLE_BYTES` = 8 MiB per group and also for the merged set.
- Group storage: `<data_dir>/secrets/groups/<base>/<group>.age`, mode `0600`. The per-deploy-key path `<data_dir>/secrets/<key>.secrets.age` does **not** change, and no migration runs.
- `Feature::SecretGroups` minimum agent version is `0.27.0`.
- A declared group that is missing or empty fails the deploy with `DomainError::NotFound`.
- Layer order is exactly the declared order in `groups = [...]`, with the deploy key's implicit group always last.
- Existing behavior is unchanged when `[secrets].groups` is absent, including on-disk paths.
- Architecture docs under `docs/architecture/` are updated in the task that changes the behavior they describe (see the `architecture-diagrams` skill for the code-area→doc map).

## File Structure

**Created:**
- `crates/domain/src/secretgroup.rs` — group identity (`GroupRef`), value types (`SecretGroup`, `GroupHead`, `GroupSummary`), name validation, digest, and the pure layer merge. One responsibility: what a group *is* and how layers combine. No I/O.
- `crates/application/src/secretgroups.rs` — the four group use-cases (`PushSecretGroup`, `ShowSecretGroup`, `ListSecretGroups`, `RemoveSecretGroup`). Separate from `secrets.rs` because those own the per-key path and its `--apply` orchestration; these own group CRUD and the registry join.
- `tests/e2e/scenarios/secret-groups/scenario.sh` + `app/` fixture — end-to-end proof that two branch environments share one pushed group.

**Modified:**
- `crates/domain/src/lib.rs` — register the new module.
- `crates/domain/src/contracts.rs` — `SecretStore` becomes addressable by `GroupRef`.
- `crates/domain/src/entities.rs` — `ProjectConfig.secret_groups`.
- `crates/domain/Cargo.toml`, root `Cargo.toml` — `sha2`.
- `crates/infrastructure/src/secrets.rs` — revision in `StoredBundle`, group paths, CAS, listing, base removal.
- `crates/infrastructure/src/sqlite.rs` — one migration adding `secret_groups`.
- `crates/infrastructure/src/repo.rs` — persist and read `secret_groups`.
- `crates/application/src/deploy.rs` — layered injection.
- `crates/application/src/secrets.rs`, `crates/application/src/remove.rs`, `crates/application/src/logs.rs` — call sites moving to `GroupRef`.
- `crates/application/src/lib.rs` — register `secretgroups`.
- `crates/bin/src/cli/rpitoml.rs`, `crates/bin/src/cli/overlay.rs` — `groups` field, validation, overlay merge.
- `crates/bin/src/proto.rs` — group DTOs, `expected_revision`, `secret_groups` in the config DTO.
- `crates/bin/src/agent/http.rs`, `crates/bin/src/agent/state.rs` — routes, handlers, wiring.
- `crates/bin/src/cli/api.rs` — client methods.
- `crates/bin/src/cli/commands.rs` — `push`, `ls`, `diff`, `group ls`, `group rm`.
- `crates/bin/src/main.rs` — `SecretsCmd` variants and the `group` sub-noun.
- `crates/bin/src/compat.rs` — `Feature::SecretGroups`.
- `docs/architecture/flows/secrets.md`, `docs/architecture/storage.md`, `docs/architecture/flows/environments.md`, `plugins/*/skills/rpi-toml`, `plugins/*/skills/rpi-cli`.

---

### Task 1: Group identity, digest and name validation

**Files:**
- Create: `crates/domain/src/secretgroup.rs`
- Modify: `crates/domain/src/lib.rs`, `crates/domain/Cargo.toml`, `Cargo.toml` (workspace deps)
- Test: inline `#[cfg(test)] mod tests` in `crates/domain/src/secretgroup.rs` (this repo keeps unit tests next to the code)

**Interfaces:**
- Consumes: `crate::entities::SecretsBundle`, `crate::error::DomainError`.
- Produces: `pi_domain::secretgroup::{GroupRef, SecretGroup, GroupHead, FileHead, GroupSummary, MAX_GROUP_NAME_LEN, validate_group_name, digest}`.

- [ ] **Step 1: Add the `sha2` dependency**

In the root `Cargo.toml`, under `[workspace.dependencies]`, add:

```toml
sha2 = "0.10"
```

In `crates/domain/Cargo.toml`, under `[dependencies]`, add:

```toml
sha2 = { workspace = true }
```

`sha2` is already in `Cargo.lock` as a transitive dependency of `age`, so this adds no new vendor surface.

- [ ] **Step 2: Write the failing tests**

Create `crates/domain/src/secretgroup.rs` with only the test module and the module doc comment:

```rust
//! What a secret group *is*: its identity, its metadata projection, and how
//! layers of groups combine at deploy time (secret-groups spec: Data model,
//! Attachment and layering). No I/O — the store lives in
//! `pi-infrastructure`, the orchestration in `pi-application`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_16_hex_chars() {
        let d = digest(b"hunter2-long");
        assert_eq!(d.len(), 16, "got {d}");
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()), "got {d}");
        assert_eq!(d, digest(b"hunter2-long"), "same input must hash the same");
        assert_ne!(d, digest(b"hunter2-longer"));
    }

    #[test]
    fn digest_matches_the_documented_sha256_prefix() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(digest(b""), "e3b0c44298fc1c14");
    }

    #[test]
    fn accepts_lowercase_dashed_names() {
        for name in ["preview", "common", "db-creds", "a", "a1-b2"] {
            assert!(validate_group_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_names_that_could_escape_or_confuse_the_store() {
        for bad in [
            "",            // empty
            "Preview",     // uppercase
            "1st",         // leading digit
            "-lead",       // leading dash
            "under_score", // underscore
            "a/b",         // path separator
            "a\\b",        // windows separator
            "..",          // parent
            ".",           // current
            "a.b",         // dot
            "a b",         // space
        ] {
            assert!(validate_group_name(bad).is_err(), "{bad:?} must be rejected");
        }
        let too_long = "a".repeat(MAX_GROUP_NAME_LEN + 1);
        assert!(validate_group_name(&too_long).is_err());
        assert!(validate_group_name(&"a".repeat(MAX_GROUP_NAME_LEN)).is_ok());
    }

    #[test]
    fn label_names_the_layer_in_logs() {
        assert_eq!(GroupRef::named("myapp", "preview").label(), "preview");
        assert_eq!(GroupRef::key("myapp--branch--x").label(), "key");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-domain secretgroup`
Expected: FAIL — compilation errors, `cannot find function digest`, `cannot find function validate_group_name`, `cannot find type GroupRef`.

- [ ] **Step 4: Write the implementation**

Above the test module in `crates/domain/src/secretgroup.rs`:

```rust
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::entities::SecretsBundle;

/// Long enough for `staging-shared-db-credentials`, short enough that a group
/// name stays readable in a `groups = [...]` line and in a deploy log.
pub const MAX_GROUP_NAME_LEN: usize = 40;

/// Same charset as environment names (`cli/overlay.rs`), so an operator who
/// already knows `--env` names does not learn a second rule. The charset is
/// also what keeps a name safe as a single path component: no separator, no
/// dot, so `.`/`..` and nested paths are unrepresentable rather than filtered.
pub fn validate_group_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("group name is empty".into());
    }
    if name.len() > MAX_GROUP_NAME_LEN {
        return Err(format!(
            "group name is {} characters; max is {MAX_GROUP_NAME_LEN}",
            name.len()
        ));
    }
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'));
    if !ok {
        return Err(format!(
            "group name '{name}' must match ^[a-z][a-z0-9-]*$"
        ));
    }
    Ok(())
}

/// Fingerprint of a secret value for comparison without transmitting it: the
/// first 16 hex characters (64 bits) of SHA-256, which is plenty against
/// accidental collision.
///
/// This is explicitly **not** a hiding mechanism. A low-entropy value
/// (`true`, `production`, a short PIN) is recovered from its digest by trivial
/// brute force, which is why every endpoint and command that exposes digests
/// requires the same authorization as a deploy — not a new concession, since
/// anyone who can deploy can deploy code that prints secrets.
pub fn digest(bytes: &[u8]) -> String {
    let full = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for byte in full.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// What a caller addresses. `Named` is a group owned by a base project;
/// `Key` is the implicit group of one deploy key (the pre-groups bundle,
/// which keeps its on-disk path).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupRef {
    Named { base: String, name: String },
    Key(String),
}

impl GroupRef {
    pub fn named(base: &str, name: &str) -> GroupRef {
        GroupRef::Named {
            base: base.to_string(),
            name: name.to_string(),
        }
    }

    pub fn key(key: &str) -> GroupRef {
        GroupRef::Key(key.to_string())
    }

    /// Short label for logs, provenance and error messages. The implicit
    /// group is always `key`: naming the deploy key there would put a
    /// project key in every line for no added information, since a deploy
    /// only ever has one.
    pub fn label(&self) -> String {
        match self {
            GroupRef::Named { name, .. } => name.clone(),
            GroupRef::Key(_) => "key".to_string(),
        }
    }
}

/// Contents of one group plus the revision they were stored at. Revision 0
/// means the group does not exist; `SecretsBundle` is reused as the content
/// type so masking, limits and the workdir writer keep one code path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SecretGroup {
    pub objects: SecretsBundle,
    pub revision: u64,
}

/// Size and fingerprint of one secret file. Never its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHead {
    pub size: u64,
    pub digest: String,
}

/// Metadata projection of a group: enough to diff against local files,
/// never enough to reconstruct a value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupHead {
    pub revision: u64,
    /// var name -> digest
    pub vars: BTreeMap<String, String>,
    /// file path -> size and digest
    pub files: BTreeMap<String, FileHead>,
    pub file_mode: Option<u32>,
}

impl GroupHead {
    /// Projection of a loaded group — the store computes this from plaintext
    /// it already holds, so there is exactly one definition of "the head of
    /// this group".
    pub fn of(group: &SecretGroup) -> GroupHead {
        GroupHead {
            revision: group.revision,
            vars: group
                .objects
                .vars
                .iter()
                .map(|(k, v)| (k.clone(), digest(v.as_bytes())))
                .collect(),
            files: group
                .objects
                .files
                .iter()
                .map(|(p, b)| {
                    (
                        p.clone(),
                        FileHead {
                            size: b.len() as u64,
                            digest: digest(b),
                        },
                    )
                })
                .collect(),
            file_mode: group.objects.file_mode,
        }
    }
}

/// One row of `rpi secrets group ls`, as the store knows it. Deliberately
/// carries no "attached by" field: the store sees files, not the registry —
/// the `ListSecretGroups` use-case joins that in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub name: String,
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
    pub bytes: u64,
    pub updated_at: i64,
}
```

Register the module in `crates/domain/src/lib.rs`, keeping the existing alphabetical order of `pub mod` lines:

```rust
pub mod secretgroup;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-domain secretgroup`
Expected: PASS — 5 tests.

- [ ] **Step 6: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green, 719 tests passing.

- [ ] **Step 7: Commit**

```bash
rtk git add Cargo.toml Cargo.lock crates/domain/Cargo.toml crates/domain/src/secretgroup.rs crates/domain/src/lib.rs
rtk git commit -m "feat(secrets): add group identity, digests and name validation"
```

---

### Task 2: Pure layer merge

**Files:**
- Modify: `crates/domain/src/secretgroup.rs`
- Test: inline test module in the same file

**Interfaces:**
- Consumes: `GroupRef`, `SecretGroup`, `SecretsBundle`, `DomainError` (Task 1).
- Produces: `pi_domain::secretgroup::{Layer, MergedSecrets, merge_layers}` with signature
  `pub fn merge_layers(layers: &[Layer<'_>], max_bytes: usize) -> Result<MergedSecrets, DomainError>`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `crates/domain/src/secretgroup.rs`:

```rust
    fn group(vars: &[(&str, &str)], files: &[(&str, &[u8])], mode: Option<u32>) -> SecretGroup {
        let mut objects = SecretsBundle {
            file_mode: mode,
            ..SecretsBundle::default()
        };
        for (k, v) in vars {
            objects.vars.insert((*k).into(), (*v).into());
        }
        for (p, b) in files {
            objects.files.insert((*p).into(), b.to_vec());
        }
        SecretGroup {
            objects,
            revision: 1,
        }
    }

    #[test]
    fn later_layer_wins_per_object() {
        let common = group(&[("A", "1"), ("B", "2")], &[], None);
        let preview = group(&[("B", "override")], &[], None);
        let merged = merge_layers(
            &[
                Layer::new("common", &common),
                Layer::new("preview", &preview),
            ],
            1024,
        )
        .unwrap();

        assert_eq!(merged.bundle.vars["A"], "1", "untouched key survives");
        assert_eq!(merged.bundle.vars["B"], "override");
        assert_eq!(merged.var_origin["A"], "common");
        assert_eq!(merged.var_origin["B"], "preview");
        assert_eq!(
            merged.shadowed,
            vec![("common".to_string(), "B".to_string())],
            "the overridden entry is reported, so `secrets ls` can mark it"
        );
    }

    #[test]
    fn later_layer_replaces_a_whole_file_at_the_same_path() {
        let common = group(&[], &[("certs/server.pem", b"OLD")], None);
        let key = group(&[], &[("certs/server.pem", b"NEW")], None);
        let merged =
            merge_layers(&[Layer::new("common", &common), Layer::new("key", &key)], 1024).unwrap();

        assert_eq!(merged.bundle.files["certs/server.pem"], b"NEW".to_vec());
        assert_eq!(merged.file_origin["certs/server.pem"], "key");
    }

    #[test]
    fn file_mode_comes_from_the_last_layer_that_sets_one() {
        let a = group(&[], &[("x", b"1")], Some(0o640));
        let b = group(&[], &[("y", b"2")], None);
        let merged =
            merge_layers(&[Layer::new("a", &a), Layer::new("b", &b)], 1024).unwrap();
        assert_eq!(
            merged.bundle.file_mode,
            Some(0o640),
            "a later layer that sets no mode must not erase an earlier one"
        );

        let c = group(&[], &[("z", b"3")], Some(0o600));
        let merged =
            merge_layers(&[Layer::new("a", &a), Layer::new("c", &c)], 1024).unwrap();
        assert_eq!(merged.bundle.file_mode, Some(0o600), "last setter wins");
    }

    #[test]
    fn merged_set_over_the_ceiling_is_invalid_and_names_the_layers() {
        let big = group(&[], &[("a.bin", &vec![0u8; 600])], None);
        let also_big = group(&[], &[("b.bin", &vec![0u8; 600])], None);
        let err = merge_layers(
            &[Layer::new("common", &big), Layer::new("preview", &also_big)],
            1000,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("common"), "got: {msg}");
        assert!(msg.contains("preview"), "got: {msg}");
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
    }

    #[test]
    fn empty_layer_list_merges_to_an_empty_bundle() {
        let merged = merge_layers(&[], 1024).unwrap();
        assert!(merged.bundle.is_empty());
        assert!(merged.shadowed.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-domain secretgroup`
Expected: FAIL — `cannot find type Layer`, `cannot find function merge_layers`.

- [ ] **Step 3: Write the implementation**

Add to `crates/domain/src/secretgroup.rs`, and extend the top-of-file `use` with `use crate::error::DomainError;`:

```rust
/// One layer of the deploy-time merge: a label for provenance plus the
/// group it came from. Borrowed, because merging must not clone secret
/// bytes it is only going to read.
pub struct Layer<'a> {
    pub label: String,
    pub group: &'a SecretGroup,
}

impl<'a> Layer<'a> {
    pub fn new(label: &str, group: &'a SecretGroup) -> Layer<'a> {
        Layer {
            label: label.to_string(),
            group,
        }
    }
}

/// Result of merging layers: the bundle to inject plus where each surviving
/// object came from and which entries were shadowed. Provenance is what lets
/// `rpi secrets ls` answer "why is this value what it is" without ever
/// printing a value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergedSecrets {
    pub bundle: SecretsBundle,
    /// var name -> label of the layer that supplied the winning value
    pub var_origin: BTreeMap<String, String>,
    /// file path -> label of the layer that supplied the winning file
    pub file_origin: BTreeMap<String, String>,
    /// (layer label, object name) for every entry a later layer overrode,
    /// in layer order.
    pub shadowed: Vec<(String, String)>,
    /// (layer label, revision) in layer order. Filled by the caller that
    /// loaded the layers, so the provenance log line and the effective view
    /// read revisions from one place instead of each re-deriving them.
    pub revisions: Vec<(String, u64)>,
}

/// Merges layers in the given order: a later layer replaces an earlier one
/// entry by entry (by variable name, and by file path for files). Layer order
/// is the caller's declared order — never re-sorted, because "the most
/// recently created group wins" is exactly the ambiguity this design refuses.
///
/// `file_mode` resolves to the last layer that sets one: a later layer that
/// leaves it unset is not asking for the default, it is not asking at all.
///
/// `max_bytes` bounds the merged file payload (the caller passes
/// `MAX_SECRETS_BUNDLE_BYTES`); the error names the contributing layers,
/// since with several layers "8 MiB exceeded" alone does not say where to
/// look.
pub fn merge_layers(
    layers: &[Layer<'_>],
    max_bytes: usize,
) -> Result<MergedSecrets, DomainError> {
    let mut merged = MergedSecrets::default();
    for layer in layers {
        for (key, value) in &layer.group.objects.vars {
            if let Some(previous) = merged.var_origin.get(key) {
                merged.shadowed.push((previous.clone(), key.clone()));
            }
            merged.bundle.vars.insert(key.clone(), value.clone());
            merged.var_origin.insert(key.clone(), layer.label.clone());
        }
        for (path, bytes) in &layer.group.objects.files {
            if let Some(previous) = merged.file_origin.get(path) {
                merged.shadowed.push((previous.clone(), path.clone()));
            }
            merged.bundle.files.insert(path.clone(), bytes.clone());
            merged.file_origin.insert(path.clone(), layer.label.clone());
        }
        if layer.group.objects.file_mode.is_some() {
            merged.bundle.file_mode = layer.group.objects.file_mode;
        }
    }

    let total: usize = merged.bundle.files.values().map(|b| b.len()).sum();
    if total > max_bytes {
        let labels: Vec<&str> = layers.iter().map(|l| l.label.as_str()).collect();
        return Err(DomainError::Invalid(format!(
            "merged secret files are {total} bytes; max is {max_bytes} (layers: {})",
            labels.join(", ")
        )));
    }
    Ok(merged)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-domain secretgroup`
Expected: PASS — 10 tests.

- [ ] **Step 5: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green, 724 tests passing.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/domain/src/secretgroup.rs
rtk git commit -m "feat(secrets): merge secret layers with provenance"
```

---

### Task 3: Addressable store with conditional writes

**Files:**
- Modify: `crates/domain/src/contracts.rs` (`SecretStore` trait), `crates/infrastructure/src/secrets.rs`
- Modify (call sites only): `crates/application/src/secrets.rs`, `crates/application/src/deploy.rs`, `crates/application/src/remove.rs`, `crates/application/src/logs.rs`, `crates/bin/src/agent/http.rs`
- Test: inline test module in `crates/infrastructure/src/secrets.rs`

**Interfaces:**
- Consumes: `GroupRef`, `SecretGroup`, `GroupHead`, `GroupSummary` (Task 1).
- Produces: the `SecretStore` trait below. Every later task calls it; `MockSecretStore` (mockall) regenerates from it, so existing `expect_load`/`expect_save` calls in other crates' tests must be updated to the new argument shapes in this task.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `crates/infrastructure/src/secrets.rs`:

```rust
    #[tokio::test]
    async fn revision_starts_at_one_and_increments_per_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::named("myapp", "preview");

        assert_eq!(
            store.load(&r).await.unwrap().revision,
            0,
            "an absent group reads as revision 0"
        );
        assert_eq!(store.save(&r, &bundle(), Some(0)).await.unwrap(), 1);
        assert_eq!(store.save(&r, &bundle(), Some(1)).await.unwrap(), 2);
        assert_eq!(store.load(&r).await.unwrap().revision, 2);
    }

    #[tokio::test]
    async fn saving_with_a_stale_expected_revision_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::named("myapp", "preview");
        store.save(&r, &bundle(), Some(0)).await.unwrap();

        let err = store.save(&r, &bundle(), Some(0)).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
        assert!(err.to_string().contains('1'), "current revision: {err}");
        assert_eq!(
            store.load(&r).await.unwrap().revision,
            1,
            "a rejected write must not have touched the group"
        );
    }

    #[tokio::test]
    async fn forced_save_bypasses_the_guard_without_resetting_the_counter() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::named("myapp", "preview");
        store.save(&r, &bundle(), Some(0)).await.unwrap();
        store.save(&r, &bundle(), Some(1)).await.unwrap();

        assert_eq!(
            store.save(&r, &bundle(), None).await.unwrap(),
            3,
            "force must continue the counter, not restart it"
        );
    }

    #[tokio::test]
    async fn a_bundle_written_before_revisions_existed_accepts_a_first_guarded_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::key("rateme");
        // Written by the pre-groups code path: no revision field on disk.
        store.save(&r, &bundle(), None).await.unwrap();
        let legacy_path = dir.path().join("secrets").join("rateme.secrets.age");
        let plaintext = serde_json::json!({ "vars": {}, "files": {} });
        let ciphertext = age::encrypt(
            &store.identity.to_public(),
            serde_json::to_vec(&plaintext).unwrap().as_slice(),
        )
        .unwrap();
        std::fs::write(&legacy_path, ciphertext).unwrap();

        assert_eq!(store.load(&r).await.unwrap().revision, 0);
        assert_eq!(store.save(&r, &bundle(), Some(0)).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn head_exposes_digests_and_sizes_but_no_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::named("myapp", "preview");
        store.save(&r, &bundle(), Some(0)).await.unwrap();

        let head = store.head(&r).await.unwrap();
        assert_eq!(head.revision, 1);
        assert_eq!(
            head.vars["DB_PASSWORD"],
            pi_domain::secretgroup::digest(b"super-secret-value")
        );
        assert_eq!(head.files["certs/server.pem"].size, 4);
        let rendered = format!("{head:?}");
        assert!(
            !rendered.contains("super-secret-value"),
            "head must not carry values: {rendered}"
        );
    }

    #[tokio::test]
    async fn groups_are_scoped_per_base_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::named("myapp", "preview"), &bundle(), Some(0))
            .await
            .unwrap();
        store
            .save(&GroupRef::named("other", "preview"), &bundle(), Some(0))
            .await
            .unwrap();

        let mine = store.list("myapp").await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].name, "preview");
        assert_eq!(mine[0].revision, 1);
        assert_eq!(mine[0].keys, 2);
        assert_eq!(mine[0].files, 1);
        assert_eq!(store.list("nobody").await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn a_group_and_a_deploy_key_bundle_never_share_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("myapp"), &bundle(), Some(0))
            .await
            .unwrap();
        store
            .save(&GroupRef::named("myapp", "preview"), &bundle(), Some(0))
            .await
            .unwrap();

        assert!(dir.path().join("secrets/myapp.secrets.age").exists());
        assert!(dir.path().join("secrets/groups/myapp/preview.age").exists());
        assert_eq!(store.list("myapp").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn remove_base_drops_every_group_of_that_project_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        for name in ["preview", "common"] {
            store
                .save(&GroupRef::named("myapp", name), &bundle(), Some(0))
                .await
                .unwrap();
        }
        store
            .save(&GroupRef::named("other", "preview"), &bundle(), Some(0))
            .await
            .unwrap();

        store.remove_base("myapp").await.unwrap();

        assert!(store.list("myapp").await.unwrap().is_empty());
        assert_eq!(store.list("other").await.unwrap().len(), 1);
        // Idempotent: a base with no groups is not an error.
        store.remove_base("myapp").await.unwrap();
    }

    #[tokio::test]
    async fn invalid_group_names_and_bases_are_rejected_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        for name in ["", "../escape", "a/b", "Preview"] {
            let r = GroupRef::named("myapp", name);
            assert!(store.save(&r, &bundle(), None).await.is_err(), "{name:?}");
        }
        for base in ["", "..", "nested/base"] {
            let r = GroupRef::named(base, "preview");
            assert!(store.save(&r, &bundle(), None).await.is_err(), "{base:?}");
        }
        assert!(!dir.path().join("secrets/groups").join("..").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn group_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::named("myapp", "preview"), &bundle(), Some(0))
            .await
            .unwrap();
        let mode = std::fs::metadata(dir.path().join("secrets/groups/myapp/preview.age"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
```

Update the existing tests in that module mechanically: every `store.save("rateme", &bundle())` becomes `store.save(&GroupRef::key("rateme"), &bundle(), None)`, every `store.load("rateme")` becomes `store.load(&GroupRef::key("rateme")).await.unwrap().objects`, and every `store.remove("rateme")` becomes `store.remove(&GroupRef::key("rateme"))`. Add `use pi_domain::secretgroup::GroupRef;` to the test module.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-infrastructure secrets`
Expected: FAIL — compilation errors; `save` takes 2 arguments, `head`/`list`/`remove_base` do not exist.

- [ ] **Step 3: Change the trait**

In `crates/domain/src/contracts.rs`, replace the `SecretStore` trait (keep the `#[cfg_attr(feature = "mocks", automock)]` and `#[async_trait]` attributes exactly as they are) and add the import:

```rust
use crate::secretgroup::{GroupHead, GroupRef, GroupSummary, SecretGroup};

/// The agent's encrypted vault, addressed by group (secret-groups spec: Data
/// model). Implementations are the single source of truth for a group's
/// contents; values never leave the agent except into a project's own
/// workdir at deploy time.
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Empty bundle at revision 0 when the group does not exist.
    async fn load(&self, r: &GroupRef) -> Result<SecretGroup, DomainError>;
    /// Metadata only — never values.
    async fn head(&self, r: &GroupRef) -> Result<GroupHead, DomainError>;
    /// `expected: Some(n)` writes only when the current revision is exactly
    /// `n` (a first write passes `Some(0)`); a mismatch is
    /// `DomainError::Conflict`. `expected: None` is the unconditional write
    /// behind `--force`. Returns the new revision, which is always the
    /// current one plus one — a forced write must not reset the counter.
    async fn save(
        &self,
        r: &GroupRef,
        objects: &SecretsBundle,
        expected: Option<u64>,
    ) -> Result<u64, DomainError>;
    async fn remove(&self, r: &GroupRef) -> Result<(), DomainError>;
    /// Declared groups of one base project. Empty when the base has none.
    async fn list(&self, base: &str) -> Result<Vec<GroupSummary>, DomainError>;
    /// Drops every declared group of a base project (`rpi rm`). A base with
    /// no groups is not an error.
    async fn remove_base(&self, base: &str) -> Result<(), DomainError>;
}
```

- [ ] **Step 4: Implement the store**

In `crates/infrastructure/src/secrets.rs`: add `revision` to `StoredBundle`, add path helpers, and replace the `impl SecretStore for EncryptedFileStore` block.

```rust
use pi_domain::secretgroup::{
    validate_group_name, GroupHead, GroupRef, GroupSummary, SecretGroup,
};
```

`StoredBundle` gains:

```rust
    /// Absent in bundles written before groups existed — those load as 0, so
    /// the first conditional write against them (`expected: Some(0)`)
    /// succeeds and no migration is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
```

Path resolution, next to `bundle_path`/`legacy_path`:

```rust
    /// `<dir>/groups/<base>/<name>.age`. A group name cannot contain a path
    /// separator (`validate_group_name`), and the base goes through the same
    /// `validated_project` check as a deploy key, so the result is always one
    /// file two levels below `self.dir`.
    fn group_path(&self, base: &str, name: &str) -> Result<PathBuf, DomainError> {
        let base = validated_project(base)?;
        validate_group_name(name).map_err(secrets_err)?;
        Ok(self.dir.join("groups").join(base).join(format!("{name}.age")))
    }

    fn groups_dir(&self, base: &str) -> Result<PathBuf, DomainError> {
        let base = validated_project(base)?;
        Ok(self.dir.join("groups").join(base))
    }

    /// The one place that maps a `GroupRef` to a file. `Key` keeps the
    /// pre-groups path so an upgrade reads what the previous version wrote.
    fn path_of(&self, r: &GroupRef) -> Result<PathBuf, DomainError> {
        match r {
            GroupRef::Named { base, name } => self.group_path(base, name),
            GroupRef::Key(key) => self.bundle_path(key),
        }
    }

    /// Decrypts one file into contents plus revision. `None` when the file is
    /// absent.
    async fn read_at(&self, path: &Path) -> Result<Option<SecretGroup>, DomainError> {
        match tokio::fs::read(path).await {
            Ok(ciphertext) => {
                let plaintext = age::decrypt(&self.identity, &ciphertext).map_err(secrets_err)?;
                let stored: StoredBundle =
                    serde_json::from_slice(&plaintext).map_err(secrets_err)?;
                let mut files = BTreeMap::new();
                for (path, b64) in stored.files {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&b64)
                        .map_err(secrets_err)?;
                    files.insert(path, bytes);
                }
                Ok(Some(SecretGroup {
                    objects: SecretsBundle {
                        vars: stored.vars,
                        files,
                        file_mode: stored.file_mode,
                    },
                    revision: stored.revision.unwrap_or(0),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(secrets_err(e)),
        }
    }
```

The trait impl:

```rust
#[async_trait]
impl SecretStore for EncryptedFileStore {
    async fn load(&self, r: &GroupRef) -> Result<SecretGroup, DomainError> {
        let path = self.path_of(r)?;
        if let Some(group) = self.read_at(&path).await? {
            return Ok(group);
        }
        // Only a deploy-key bundle can have a pre-secrets ancestor; a group
        // is newer than that format and never has one.
        if let GroupRef::Key(key) = r {
            match tokio::fs::read(self.legacy_path(key)?).await {
                Ok(ciphertext) => {
                    let plaintext =
                        age::decrypt(&self.identity, &ciphertext).map_err(secrets_err)?;
                    let text = String::from_utf8(plaintext).map_err(secrets_err)?;
                    let objects = dotenv::parse(&text).map_err(secrets_err)?;
                    return Ok(SecretGroup {
                        objects,
                        revision: 0,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(secrets_err(e)),
            }
        }
        Ok(SecretGroup::default())
    }

    async fn head(&self, r: &GroupRef) -> Result<GroupHead, DomainError> {
        Ok(GroupHead::of(&self.load(r).await?))
    }

    async fn save(
        &self,
        r: &GroupRef,
        objects: &SecretsBundle,
        expected: Option<u64>,
    ) -> Result<u64, DomainError> {
        let current = self.load(r).await?.revision;
        if let Some(expected) = expected {
            if expected != current {
                return Err(DomainError::Conflict(format!(
                    "secret group changed since revision {expected} (current is {current}); \
                     re-run to see the difference, or pass --force to overwrite"
                )));
            }
        }
        let next = current.saturating_add(1);
        let stored = StoredBundle {
            vars: objects.vars.clone(),
            files: objects
                .files
                .iter()
                .map(|(p, b)| {
                    (
                        p.clone(),
                        base64::engine::general_purpose::STANDARD.encode(b),
                    )
                })
                .collect(),
            file_mode: objects.file_mode,
            revision: Some(next),
        };
        let plaintext = serde_json::to_vec(&stored).map_err(secrets_err)?;
        let ciphertext =
            age::encrypt(&self.identity.to_public(), &plaintext).map_err(secrets_err)?;
        let path = self.path_of(r)?;
        let legacy = match r {
            GroupRef::Key(key) => Some(self.legacy_path(key)?),
            GroupRef::Named { .. } => None,
        };
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(secrets_err)?;
            }
            fsutil::write_private_atomic(&path, &ciphertext, 0o600).map_err(secrets_err)?;
            if let Some(legacy) = legacy {
                match fs::remove_file(&legacy) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(secrets_err(e)),
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| secrets_err(format!("join error: {e}")))??;
        Ok(next)
    }

    async fn remove(&self, r: &GroupRef) -> Result<(), DomainError> {
        let mut targets = Vec::new();
        if let GroupRef::Key(key) = r {
            // Legacy first: if the primary deletion then fails, `load` still
            // finds the primary file and reports secrets as present rather
            // than silently falling back to an un-deleted legacy file.
            targets.push(self.legacy_path(key)?);
        }
        targets.push(self.path_of(r)?);
        for target in targets {
            match tokio::fs::remove_file(&target).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(secrets_err(e)),
            }
        }
        Ok(())
    }

    async fn list(&self, base: &str) -> Result<Vec<GroupSummary>, DomainError> {
        let dir = self.groups_dir(base)?;
        let mut names: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|n| n.strip_suffix(".age").map(str::to_string))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(secrets_err(e)),
        };
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let path = self.group_path(base, &name)?;
            let Some(group) = self.read_at(&path).await? else {
                continue;
            };
            let updated_at = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(GroupSummary {
                name,
                revision: group.revision,
                keys: group.objects.vars.len(),
                files: group.objects.files.len(),
                bytes: group.objects.files.values().map(|b| b.len() as u64).sum(),
                updated_at,
            });
        }
        Ok(out)
    }

    async fn remove_base(&self, base: &str) -> Result<(), DomainError> {
        let dir = self.groups_dir(base)?;
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(secrets_err(e)),
        }
    }
}
```

- [ ] **Step 5: Update every call site**

These are mechanical; the deliverable is that the workspace compiles and the existing tests still pass.

- `crates/application/src/secrets.rs`: `self.secrets.save(project, &bundle)` becomes
  `self.secrets.save(&GroupRef::key(project), &bundle, None).await?;` and
  `self.secrets.load(project)` becomes `self.secrets.load(&GroupRef::key(project)).await?.objects`.
- `crates/application/src/deploy.rs`: `self.secrets.load(&config.name)` becomes
  `self.secrets.load(&GroupRef::key(&config.name)).await?.objects`.
- `crates/application/src/remove.rs`: `self.secrets.remove(project)` becomes
  `self.secrets.remove(&GroupRef::key(project))`.
- `crates/application/src/logs.rs`: same substitution as `deploy.rs` wherever it loads a bundle.
- Every `MockSecretStore` expectation in those crates' tests: `expect_save().returning(|_, _| Ok(()))`
  becomes `expect_save().returning(|_, _, _| Ok(1))`, and `expect_load().returning(|_| Ok(SecretsBundle::default()))`
  becomes `expect_load().returning(|_| Ok(SecretGroup::default()))`. Where a `withf` matched a project
  name, match on the ref instead: `.withf(|r, _, _| *r == GroupRef::key("rateme"))`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-infrastructure secrets`
Expected: PASS — the ten new tests plus the updated existing ones.

- [ ] **Step 7: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 8: Commit**

```bash
rtk git add crates/domain/src/contracts.rs crates/infrastructure/src/secrets.rs crates/application crates/bin
rtk git commit -m "feat(secrets): address the vault by group with conditional writes"
```

---

### Task 4: `[secrets].groups` in rpi.toml and overlays

**Files:**
- Modify: `crates/bin/src/cli/rpitoml.rs`, `crates/bin/src/cli/overlay.rs`
- Test: inline test modules in both files

**Interfaces:**
- Consumes: `pi_domain::secretgroup::validate_group_name` (Task 1).
- Produces: `SecretsSection.groups: Vec<String>` and `OverlaySecrets.groups: Option<Vec<String>>`.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/cli/rpitoml.rs` test module:

```rust
    #[test]
    fn secrets_groups_are_parsed_and_default_to_empty() {
        let parsed = RpiToml::parse(&SAMPLE.replace(
            "[secrets]",
            "[secrets]\ngroups = [\"common\", \"preview\"]",
        ))
        .unwrap();
        assert_eq!(
            parsed.secrets.groups,
            vec!["common".to_string(), "preview".to_string()]
        );
        assert!(RpiToml::parse(SAMPLE).unwrap().secrets.groups.is_empty());
    }

    #[test]
    fn invalid_group_names_are_rejected_at_parse_time() {
        for bad in ["Preview", "a/b", "1st", "under_score", ""] {
            let err = RpiToml::parse(&SAMPLE.replace(
                "[secrets]",
                &format!("[secrets]\ngroups = [\"{bad}\"]"),
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("[secrets].groups"), "{bad}: {err}");
        }
    }

    #[test]
    fn duplicate_groups_are_rejected() {
        let err = RpiToml::parse(&SAMPLE.replace(
            "[secrets]",
            "[secrets]\ngroups = [\"common\", \"common\"]",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }
```

In `crates/bin/src/cli/overlay.rs` test module:

```rust
    #[test]
    fn overlay_groups_replace_wholesale_and_empty_list_detaches() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(
            &BASE.replace("[secrets]", "[secrets]\ngroups = [\"common\"]"),
        )
        .unwrap();
        apply_overlay(&mut base, overlay("[secrets]\ngroups = [\"preview\"]\n"));
        assert_eq!(
            base.secrets.groups,
            vec!["preview".to_string()],
            "arrays replace, never concatenate"
        );

        apply_overlay(&mut base, overlay("[secrets]\ngroups = []\n"));
        assert!(
            base.secrets.groups.is_empty(),
            "an explicit empty list detaches every group"
        );
    }

    #[test]
    fn interpolation_in_groups_is_rejected() {
        let mut o = overlay("[secrets]\ngroups = [\"${BRANCH_NAME}\"]\n");
        let err = interpolate(&mut o, &branch_vars()).unwrap_err().to_string();
        assert!(err.contains("secrets.groups"), "got: {err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p rpi rpitoml && rtk cargo test -p rpi overlay`
Expected: FAIL — no field `groups` on `SecretsSection` / `OverlaySecrets`.

- [ ] **Step 3: Write the implementation**

In `crates/bin/src/cli/rpitoml.rs`, add to `SecretsSection`:

```rust
    /// Declared secret groups, applied in this order at deploy time before the
    /// deploy key's own bundle (secret-groups spec: Attachment and layering).
    /// An explicit empty list detaches every group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
```

In `validate_common`, next to the existing `[secrets].files` loop:

```rust
        let mut seen_groups = std::collections::BTreeSet::new();
        for name in &self.secrets.groups {
            pi_domain::secretgroup::validate_group_name(name)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].groups: {e}"))?;
            if !seen_groups.insert(name) {
                anyhow::bail!("rpi.toml [secrets].groups: duplicate group '{name}'");
            }
        }
```

In `crates/bin/src/cli/overlay.rs`, add to `OverlaySecrets`:

```rust
    pub groups: Option<Vec<String>>,
```

In `interpolate`, inside the existing `if let Some(s) = &overlay.secrets` block:

```rust
        for g in s.groups.iter().flatten() {
            forbid("secrets.groups", Some(g))?;
        }
```

In `apply_overlay`, inside the existing `if let Some(s) = overlay.secrets` block:

```rust
        if let Some(groups) = s.groups {
            base.secrets.groups = groups;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `rtk cargo test -p rpi rpitoml && rtk cargo test -p rpi overlay`
Expected: PASS.

- [ ] **Step 5: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/bin/src/cli/rpitoml.rs crates/bin/src/cli/overlay.rs
rtk git commit -m "feat(secrets): declare secret groups in rpi.toml and overlays"
```

---

### Task 5: Persist `secret_groups` on the project

**Files:**
- Modify: `crates/domain/src/entities.rs`, `crates/infrastructure/src/sqlite.rs`, `crates/infrastructure/src/repo.rs`, `crates/bin/src/proto.rs`
- Test: inline test module in `crates/infrastructure/src/repo.rs`, plus fixing every `ProjectConfig` literal in the workspace

**Interfaces:**
- Consumes: nothing new.
- Produces: `ProjectConfig.secret_groups: Vec<String>`, persisted in the `projects.secret_groups` column and carried by `ProjectConfigDto.secret_groups`.

- [ ] **Step 1: Write the failing test**

In `crates/infrastructure/src/repo.rs` test module:

```rust
    #[tokio::test]
    async fn secret_groups_round_trip_and_default_to_empty() {
        let (repo, _dir) = repo().await;
        let mut config = config("a");
        config.secret_groups = vec!["common".into(), "preview".into()];
        repo.upsert(&config).await.unwrap();
        let loaded = repo.get("a").await.unwrap().unwrap();
        assert_eq!(
            loaded.config.secret_groups,
            vec!["common".to_string(), "preview".to_string()]
        );

        // Replacing the list wholesale is how an overlay detaches groups.
        config.secret_groups.clear();
        repo.upsert(&config).await.unwrap();
        assert!(repo
            .get("a")
            .await
            .unwrap()
            .unwrap()
            .config
            .secret_groups
            .is_empty());
    }

    #[tokio::test]
    async fn a_row_written_before_the_column_existed_reads_as_no_groups() {
        let (repo, dir) = repo().await;
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.call(|c| {
            c.execute(
                "INSERT INTO projects (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('legacy', 'r', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(crate::sqlite::storage_err)?;
            Ok(())
        })
        .await
        .unwrap();

        let loaded = repo.get("legacy").await.unwrap().unwrap();
        assert!(loaded.config.secret_groups.is_empty());
    }
```

Match the helper names already used in that test module (`repo()`, `config(name)`); if they differ, use the existing ones rather than adding new ones.

- [ ] **Step 2: Run the test to verify it fails**

Run: `rtk cargo test -p pi-infrastructure repo`
Expected: FAIL — no field `secret_groups` on `ProjectConfig`.

- [ ] **Step 3: Write the implementation**

In `crates/domain/src/entities.rs`, add to `ProjectConfig`:

```rust
    /// Declared secret groups, in attachment order (secret-groups spec).
    /// Empty means only the deploy key's own bundle is injected — the
    /// behavior of every version before groups existed.
    pub secret_groups: Vec<String>,
```

In `crates/infrastructure/src/sqlite.rs`, append one migration to the `migrations()` vector (never edit an existing `M::up`, which would break already-migrated hosts):

```rust
        M::up("ALTER TABLE projects ADD COLUMN secret_groups TEXT NOT NULL DEFAULT '[]';"),
```

In `crates/infrastructure/src/repo.rs`:

- extend `SELECT` with `, secret_groups` at the end of the column list (index 19);
- in `row_to_project`, add to the `ProjectConfig` literal:

```rust
            secret_groups: serde_json::from_str(&row.get::<_, String>(19)?).unwrap_or_default(),
```

- in the `UPDATE` statement add `, secret_groups=?16` and pass
  `serde_json::to_string(&config.secret_groups).unwrap_or_else(|_| "[]".into())` as that parameter;
- in the `INSERT` statement add `secret_groups` to the column list and the same JSON string to the values.

The `unwrap_or_default()` on read is what makes a pre-column row (and any row a downgrade wrote) read as no groups instead of failing.

In `crates/bin/src/proto.rs`, add to `ProjectConfigDto` and to both conversion directions:

```rust
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_groups: Vec<String>,
```

- [ ] **Step 4: Fix every `ProjectConfig` literal**

`ProjectConfig` has no `Default`, so adding a field breaks every struct literal. Run `rtk cargo build --locked` and add `secret_groups: Vec::new(),` (or `vec!["preview".into()]` where a test needs groups) to each reported site. There are literals in `crates/application/src/{deploy,secrets,environments,remove,logs}.rs` tests, `crates/infrastructure/src/repo.rs` tests, and `crates/bin/src/{proto.rs,agent/http.rs}`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-infrastructure repo`
Expected: PASS.

- [ ] **Step 6: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/domain/src/entities.rs crates/infrastructure crates/bin crates/application
rtk git commit -m "feat(secrets): persist declared secret groups per project"
```

---

### Task 6: Layered injection at deploy

**Files:**
- Modify: `crates/application/src/deploy.rs`, `docs/architecture/flows/secrets.md`, `docs/architecture/flows/environments.md`
- Test: inline test module in `crates/application/src/deploy.rs`

**Interfaces:**
- Consumes: `merge_layers`, `Layer`, `GroupRef` (Tasks 1–3); `ProjectConfig.secret_groups` (Task 5).
- Produces: the deploy-time contract that later tasks rely on — declared groups load in order, the key group is last, a missing group is `NotFound`.

- [ ] **Step 1: Write the failing tests**

In `crates/application/src/deploy.rs` test module:

```rust
    #[tokio::test]
    async fn declared_groups_are_injected_in_order_under_the_key_bundle() {
        let mut m = mocks();
        let mut config = config_with_groups(vec!["common".into(), "preview".into()]);
        config.name = "myapp--branch--x".into();
        set_env_base(&mut config, "myapp");

        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { base, name } if base == "myapp" && name == "common" => {
                Ok(group_with(&[("A", "from-common"), ("B", "from-common")]))
            }
            GroupRef::Named { base, name } if base == "myapp" && name == "preview" => {
                Ok(group_with(&[("B", "from-preview")]))
            }
            GroupRef::Key(key) if key == "myapp--branch--x" => {
                Ok(group_with(&[("B", "from-key")]))
            }
            other => panic!("unexpected load({other:?})"),
        });
        m.secrets_writer
            .expect_write()
            .withf(|_, b| {
                b.vars["A"] == "from-common" && b.vars["B"] == "from-key"
            })
            .times(1)
            .returning(|_, _| Ok(()));

        let sink = CollectSink::new();
        run_deploy(m, config, sink.clone()).await.unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("groups: common@r1, preview@r1, key@r1")),
            "provenance must be in the log: {lines:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_declared_group_fails_the_deploy_at_the_secrets_stage() {
        let mut m = mocks();
        let mut config = config_with_groups(vec!["preview".into()]);
        set_env_base(&mut config, "myapp");
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().times(0);

        let err = run_deploy(m, config, CollectSink::new()).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
        assert!(err.to_string().contains("preview"), "got: {err}");
    }

    #[tokio::test]
    async fn masking_is_armed_on_values_that_came_from_a_group() {
        let mut m = mocks();
        let mut config = config_with_groups(vec!["preview".into()]);
        set_env_base(&mut config, "myapp");
        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { .. } => Ok(group_with(&[("DB_PASSWORD", "group-secret-value")])),
            GroupRef::Key(_) => Ok(SecretGroup::default()),
        });
        m.secrets_writer.expect_write().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, log| {
            log.line("starting with group-secret-value");
            Ok(())
        });

        let sink = CollectSink::new();
        run_deploy(m, config, sink.clone()).await.unwrap();
        let lines = sink.lines.lock().unwrap();
        assert!(
            !lines.iter().any(|l| l.contains("group-secret-value")),
            "a group's value leaked into container output: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("***DB_PASSWORD***")));
    }

    #[tokio::test]
    async fn a_base_project_resolves_groups_under_its_own_name() {
        let mut m = mocks();
        let mut config = config_with_groups(vec!["common".into()]);
        config.name = "myapp".into();
        config.environment = None;
        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { base, .. } if base == "myapp" => Ok(group_with(&[("A", "1")])),
            GroupRef::Key(_) => Ok(SecretGroup::default()),
            other => panic!("unexpected load({other:?})"),
        });
        m.secrets_writer.expect_write().returning(|_, _| Ok(()));
        run_deploy(m, config, CollectSink::new()).await.unwrap();
    }
```

Add the three helpers next to the module's existing fixtures, reusing whatever `run_deploy`-equivalent the module already has (if the existing tests call the use-case inline, keep doing that rather than introducing a wrapper):

```rust
    fn config_with_groups(groups: Vec<String>) -> ProjectConfig {
        ProjectConfig {
            secret_groups: groups,
            ..base_config()
        }
    }

    fn set_env_base(config: &mut ProjectConfig, base: &str) {
        config.environment = Some(EnvironmentMeta {
            env: "branch".into(),
            base: base.into(),
            slug: Some("x".into()),
            ttl_secs: None,
            on_create: None,
        });
    }

    fn group_with(vars: &[(&str, &str)]) -> SecretGroup {
        let mut objects = SecretsBundle::default();
        for (k, v) in vars {
            objects.vars.insert((*k).into(), (*v).into());
        }
        SecretGroup {
            objects,
            revision: 1,
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-application deploy`
Expected: FAIL — only the key bundle is loaded; no group loads, no provenance line, no `NotFound`.

- [ ] **Step 3: Write the implementation**

In `crates/application/src/lib.rs`, add the ceiling and the one function that resolves a project's layers. It goes here rather than in `deploy.rs` because `rpi secrets push --apply` (Task 7) must resolve layers identically — two copies of this would be two chances for the deploy and the apply to disagree about what a project's secrets are:

```rust
/// Ceiling for the merged secret payload, mirroring
/// `crate::proto::MAX_SECRETS_BUNDLE_BYTES` in the bin crate (8 MiB). Kept
/// here because the merge happens in this crate and `pi-application` must
/// never depend on bin-crate code.
pub const MAX_MERGED_SECRET_BYTES: usize = 8 * 1024 * 1024;

/// Loads a project's effective secrets: every declared group in order, then
/// the deploy key's own bundle on top (secret-groups spec: Attachment and
/// layering). A declared group with no secrets is `NotFound` — an application
/// started without its secrets breaks later and less legibly than a deploy
/// that refuses to start.
pub async fn effective_secrets(
    secrets: &dyn pi_domain::contracts::SecretStore,
    config: &pi_domain::entities::ProjectConfig,
) -> Result<pi_domain::secretgroup::MergedSecrets, pi_domain::error::DomainError> {
    use pi_domain::error::DomainError;
    use pi_domain::secretgroup::{merge_layers, GroupRef, Layer, SecretGroup};

    let base = config
        .environment
        .as_ref()
        .map(|e| e.base.clone())
        .unwrap_or_else(|| config.name.clone());
    let mut loaded: Vec<(String, SecretGroup)> = Vec::new();
    for name in &config.secret_groups {
        let group = secrets.load(&GroupRef::named(&base, name)).await?;
        if group.objects.is_empty() {
            return Err(DomainError::NotFound(format!(
                "secret group '{name}' of project '{base}' has no secrets; \
                 push it with `rpi secrets push --group {name}` before deploying"
            )));
        }
        loaded.push((name.clone(), group));
    }
    loaded.push((
        "key".to_string(),
        secrets.load(&GroupRef::key(&config.name)).await?,
    ));

    let layers: Vec<Layer<'_>> = loaded
        .iter()
        .map(|(label, group)| Layer::new(label, group))
        .collect();
    let mut merged = merge_layers(&layers, MAX_MERGED_SECRET_BYTES)?;
    merged.revisions = loaded
        .iter()
        .map(|(label, g)| (label.clone(), g.revision))
        .collect();
    Ok(merged)
}
```

In `crates/application/src/deploy.rs`, replace the secrets stage with a call to it:

```rust
        // secret-groups spec: declared groups in order, then this deploy
        // key's own bundle on top, then one merged injection.
        let merged = crate::effective_secrets(self.secrets.as_ref(), config).await?;
        // Arm on the merged bundle: a value contributed by any layer must be
        // masked in container output, not only one that arrived in the push.
        masker.arm(&merged.bundle);
        let provenance: Vec<String> = merged
            .revisions
            .iter()
            .map(|(label, r)| format!("{label}@r{r}"))
            .collect();
        let bundle = merged.bundle;
```

Keep the existing `if !bundle.is_empty()` guard around `secrets_writer.write`, and replace the log line with:

```rust
            log.line(&format!(
                "secrets injected ({} keys, {} files, mode {:04o}; groups: {})",
                bundle.vars.len(),
                bundle.files.len(),
                bundle.secret_file_mode(),
                provenance.join(", ")
            ));
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-application deploy`
Expected: PASS.

- [ ] **Step 5: Update the architecture docs**

In `docs/architecture/flows/secrets.md`, add the layer resolution to the deploy-time section: declared groups in order, key bundle last, per-object replacement, `file_mode` from the last setter, masking armed on the merged bundle, and the provenance log line.

In `docs/architecture/flows/environments.md`, correct the claim that a fresh environment has no secrets until they are sent explicitly. Replacement text: a fresh environment inherits every group its overlay declares and starts with an empty implicit bundle; the base project's bundle is still never copied.

In `docs/architecture/storage.md`, add `secrets/groups/<base>/<group>.age` to the vault description.

- [ ] **Step 6: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/application docs/architecture
rtk git commit -m "feat(deploy): inject declared secret groups under the key bundle"
```

---

### Task 7: Group use-cases and agent endpoints

**Files:**
- Create: `crates/application/src/secretgroups.rs`
- Modify: `crates/application/src/lib.rs`, `crates/bin/src/proto.rs`, `crates/bin/src/agent/http.rs`, `crates/bin/src/agent/state.rs`
- Test: inline test module in `crates/application/src/secretgroups.rs`; HTTP tests in `crates/bin/src/agent/http.rs`

**Interfaces:**
- Consumes: `SecretStore` (Task 3), `ProjectRepository`.
- Produces: `PushSecretGroup::execute(base, name, objects, expected, merge) -> Result<u64, DomainError>`,
  `ShowSecretGroup::execute(base, name) -> Result<GroupHead, DomainError>`,
  `ListSecretGroups::execute(base) -> Result<Vec<AttachedGroup>, DomainError>`,
  `RemoveSecretGroup::execute(base, name, force) -> Result<(), DomainError>`,
  and the four `/v1/projects/{base}/secret-groups` routes.

- [ ] **Step 1: Write the failing tests**

Create `crates/application/src/secretgroups.rs` with the module doc and test module:

```rust
//! Group CRUD (secret-groups spec: CLI surface, Wire protocol). Separate from
//! `secrets.rs`, which owns the per-deploy-key path and its `--apply`
//! orchestration: these use-cases never touch containers, and they are the
//! only place that joins the vault with the registry.

#[cfg(test)]
mod tests {
    use super::*;
    use pi_domain::contracts::{MockProjectRepository, MockSecretStore};
    use pi_domain::entities::SecretsBundle;

    fn objects(vars: &[(&str, &str)]) -> SecretsBundle {
        let mut b = SecretsBundle::default();
        for (k, v) in vars {
            b.vars.insert((*k).into(), (*v).into());
        }
        b
    }

    #[tokio::test]
    async fn push_replaces_wholesale_by_default() {
        let mut store = MockSecretStore::new();
        store
            .expect_save()
            .withf(|r, b, expected| {
                *r == GroupRef::named("myapp", "preview")
                    && b.vars.len() == 1
                    && b.vars.contains_key("NEW")
                    && *expected == Some(3)
            })
            .times(1)
            .returning(|_, _, _| Ok(4));

        let revision = PushSecretGroup::new(Arc::new(store))
            .execute("myapp", "preview", objects(&[("NEW", "1")]), Some(3), false)
            .await
            .unwrap();
        assert_eq!(revision, 4);
    }

    #[tokio::test]
    async fn push_with_merge_upserts_onto_the_stored_objects() {
        let mut store = MockSecretStore::new();
        store.expect_load().returning(|_| {
            Ok(SecretGroup {
                objects: objects(&[("OLD", "keep"), ("NEW", "stale")]),
                revision: 2,
            })
        });
        store
            .expect_save()
            .withf(|_, b, _| {
                b.vars["OLD"] == "keep" && b.vars["NEW"] == "fresh" && b.vars.len() == 2
            })
            .times(1)
            .returning(|_, _, _| Ok(3));

        PushSecretGroup::new(Arc::new(store))
            .execute("myapp", "preview", objects(&[("NEW", "fresh")]), Some(2), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_rejects_an_empty_group() {
        let mut store = MockSecretStore::new();
        store.expect_save().times(0);
        let err = PushSecretGroup::new(Arc::new(store))
            .execute("myapp", "preview", SecretsBundle::default(), Some(0), false)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
    }

    #[tokio::test]
    async fn list_joins_the_registry_to_report_who_attaches_each_group() {
        let mut store = MockSecretStore::new();
        store.expect_list().withf(|b| b == "myapp").returning(|_| {
            Ok(vec![
                GroupSummary {
                    name: "common".into(),
                    revision: 2,
                    keys: 3,
                    files: 0,
                    bytes: 0,
                    updated_at: 100,
                },
                GroupSummary {
                    name: "orphan".into(),
                    revision: 1,
                    keys: 1,
                    files: 0,
                    bytes: 0,
                    updated_at: 90,
                },
            ])
        });
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["common"])]));

        let listed = ListSecretGroups::new(Arc::new(store), Arc::new(projects))
            .execute("myapp")
            .await
            .unwrap();
        assert_eq!(listed[0].summary.name, "common");
        assert_eq!(listed[0].attached_by, vec!["myapp--test".to_string()]);
        assert!(
            listed[1].attached_by.is_empty(),
            "a group nobody declares reports no attachments, not an error"
        );
    }

    #[tokio::test]
    async fn remove_refuses_while_a_project_declares_the_group() {
        let mut store = MockSecretStore::new();
        store.expect_remove().times(0);
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["preview"])]));

        let err = RemoveSecretGroup::new(Arc::new(store), Arc::new(projects))
            .execute("myapp", "preview", false)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
        assert!(err.to_string().contains("myapp--test"), "got: {err}");
    }

    #[tokio::test]
    async fn remove_with_force_deletes_an_attached_group() {
        let mut store = MockSecretStore::new();
        store
            .expect_remove()
            .withf(|r| *r == GroupRef::named("myapp", "preview"))
            .times(1)
            .returning(|_| Ok(()));
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["preview"])]));

        RemoveSecretGroup::new(Arc::new(store), Arc::new(projects))
            .execute("myapp", "preview", true)
            .await
            .unwrap();
    }
}
```

Add a `project_declaring(key, groups)` helper to that test module building a `Project` whose `config.secret_groups` is the given list (copy the `Project` literal shape from `crates/application/src/environments.rs` tests and set `secret_groups`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-application secretgroups`
Expected: FAIL — the use-case types do not exist.

- [ ] **Step 3: Write the use-cases**

Above the test module in `crates/application/src/secretgroups.rs`:

```rust
use std::sync::Arc;

use pi_domain::contracts::{ProjectRepository, SecretStore};
use pi_domain::entities::SecretsBundle;
use pi_domain::error::DomainError;
use pi_domain::secretgroup::{GroupHead, GroupRef, GroupSummary, SecretGroup};

/// `PUT /v1/projects/{base}/secret-groups/{group}`.
pub struct PushSecretGroup {
    secrets: Arc<dyn SecretStore>,
}

impl PushSecretGroup {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<PushSecretGroup> {
        Arc::new(PushSecretGroup { secrets })
    }

    /// `merge` upserts the incoming objects onto what is stored; the default
    /// replaces the group wholesale, which is what keeps the group equal to
    /// the local sources it was pushed from. Either way the write is
    /// conditional on `expected` — `merge` weakens what is written, not the
    /// guard.
    pub async fn execute(
        &self,
        base: &str,
        name: &str,
        objects: SecretsBundle,
        expected: Option<u64>,
        merge: bool,
    ) -> Result<u64, DomainError> {
        if objects.is_empty() {
            return Err(DomainError::Invalid(
                "secret group payload is empty".into(),
            ));
        }
        let r = GroupRef::named(base, name);
        let objects = if merge {
            let mut stored = self.secrets.load(&r).await?.objects;
            stored.vars.extend(objects.vars);
            stored.files.extend(objects.files);
            if objects.file_mode.is_some() {
                stored.file_mode = objects.file_mode;
            }
            stored
        } else {
            objects
        };
        self.secrets.save(&r, &objects, expected).await
    }
}

/// `GET /v1/projects/{base}/secret-groups/{group}` — metadata only.
pub struct ShowSecretGroup {
    secrets: Arc<dyn SecretStore>,
}

impl ShowSecretGroup {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<ShowSecretGroup> {
        Arc::new(ShowSecretGroup { secrets })
    }

    pub async fn execute(&self, base: &str, name: &str) -> Result<GroupHead, DomainError> {
        let head = self.secrets.head(&GroupRef::named(base, name)).await?;
        if head.revision == 0 {
            return Err(DomainError::NotFound(format!(
                "secret group '{name}' of project '{base}'"
            )));
        }
        Ok(head)
    }
}

/// One row of `rpi secrets group ls`: the store's summary plus the registry
/// join the store cannot do itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedGroup {
    pub summary: GroupSummary,
    pub attached_by: Vec<String>,
}

/// `GET /v1/projects/{base}/secret-groups`.
pub struct ListSecretGroups {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
}

impl ListSecretGroups {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Arc<ListSecretGroups> {
        Arc::new(ListSecretGroups { secrets, projects })
    }

    pub async fn execute(&self, base: &str) -> Result<Vec<AttachedGroup>, DomainError> {
        let summaries = self.secrets.list(base).await?;
        let projects = self.projects.list().await?;
        Ok(summaries
            .into_iter()
            .map(|summary| {
                let attached_by = attachers(&projects, base, &summary.name);
                AttachedGroup {
                    summary,
                    attached_by,
                }
            })
            .collect())
    }
}

/// `DELETE /v1/projects/{base}/secret-groups/{group}`.
pub struct RemoveSecretGroup {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
}

impl RemoveSecretGroup {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Arc<RemoveSecretGroup> {
        Arc::new(RemoveSecretGroup { secrets, projects })
    }

    pub async fn execute(
        &self,
        base: &str,
        name: &str,
        force: bool,
    ) -> Result<(), DomainError> {
        if !force {
            let attached = attachers(&self.projects.list().await?, base, name);
            if !attached.is_empty() {
                return Err(DomainError::Conflict(format!(
                    "secret group '{name}' is declared by {}; \
                     their next deploy would fail without it (pass --force to delete anyway)",
                    attached.join(", ")
                )));
            }
        }
        self.secrets.remove(&GroupRef::named(base, name)).await
    }
}

/// Registered project keys whose config declares `name` and whose base is
/// `base`. A base project's own key counts: it declares groups too.
fn attachers(
    projects: &[pi_domain::entities::Project],
    base: &str,
    name: &str,
) -> Vec<String> {
    projects
        .iter()
        .filter(|p| {
            let p_base = p
                .config
                .environment
                .as_ref()
                .map(|e| e.base.as_str())
                .unwrap_or(p.config.name.as_str());
            p_base == base && p.config.secret_groups.iter().any(|g| g == name)
        })
        .map(|p| p.config.name.clone())
        .collect()
}
```

Register the module in `crates/application/src/lib.rs`:

```rust
pub mod secretgroups;
```

Then extract the apply path in `crates/application/src/secrets.rs`, because a group push with `--apply` needs exactly what `SendSecrets`'s apply branch already does — re-inject and `up -d` — but for the **merged** set of a deploy key, and there must be one implementation of that, not two:

```rust
/// Re-injects a deploy key's effective secrets (declared groups merged under
/// its own bundle) and runs `up -d` so compose recreates only the affected
/// services. Used by `rpi secrets push --apply` on both the group and the
/// per-key path — `SendSecrets` delegates here after storing, rather than
/// keeping a second copy of this orchestration.
pub struct ApplySecrets {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
    source: Arc<dyn Source>,
    writer: Arc<dyn SecretsWriter>,
    overrides: Arc<dyn OverrideStore>,
    runtime: Arc<dyn ContainerRuntime>,
}

impl ApplySecrets {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
        source: Arc<dyn Source>,
        writer: Arc<dyn SecretsWriter>,
        overrides: Arc<dyn OverrideStore>,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> Arc<ApplySecrets> {
        Arc::new(ApplySecrets {
            secrets,
            projects,
            source,
            writer,
            overrides,
            runtime,
        })
    }

    /// Returns the merged key/file counts actually written.
    pub async fn execute(
        &self,
        project: &str,
        log: Arc<dyn LogSink>,
    ) -> Result<(usize, usize), DomainError> {
        let registered = self.projects.get(project).await?.ok_or_else(|| {
            DomainError::NotFound(format!(
                "project '{project}' is not deployed yet; run `rpi deploy` first"
            ))
        })?;
        let config = &registered.config;
        let merged = crate::effective_secrets(self.secrets.as_ref(), config).await?;

        let masker = MaskingSink::new(log);
        masker.arm(&merged.bundle);
        let log: Arc<dyn LogSink> = masker;

        let workdir = self.source.workdir(project);
        self.writer.write(&workdir, &merged.bundle).await?;
        let override_file = self
            .overrides
            .write(
                project,
                &config.service,
                config.expose.bind_addr(),
                registered.host_port,
                config.container_port,
            )
            .await?;
        let stack = ComposeStack {
            project_name: config.name.clone(),
            workdir: workdir.clone(),
            compose_file: workdir.join(&config.compose_path),
            override_file,
        };
        self.runtime.up(&stack, log).await?;
        Ok((merged.bundle.vars.len(), merged.bundle.files.len()))
    }
}
```

`crate::effective_secrets` is the function Task 6 already added to `crates/application/src/lib.rs`; this use-case calls it rather than repeating the layer loop.

- [ ] **Step 4: Run the use-case tests to verify they pass**

Run: `rtk cargo test -p pi-application secretgroups`
Expected: PASS — 6 tests.

- [ ] **Step 5: Write the failing HTTP tests**

In `crates/bin/src/agent/http.rs` test module:

```rust
    #[tokio::test]
    async fn secret_group_push_then_head_roundtrip_hides_values() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({
            "vars": { "DB_PASSWORD": "super-secret-value" },
            "expected_revision": 0
        });
        let (status, json) = request(
            app.clone(),
            put_json("/v1/projects/myapp/secret-groups/preview", &body),
        )
        .await;
        assert_eq!(status, 200, "{json:?}");
        assert_eq!(json["revision"], 1);

        let (status, json) = request(
            app.clone(),
            get_req("/v1/projects/myapp/secret-groups/preview"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(json["revision"], 1);
        assert!(json["vars"]["DB_PASSWORD"].is_string());
        let rendered = json.to_string();
        assert!(
            !rendered.contains("super-secret-value"),
            "value leaked: {rendered}"
        );
    }

    #[tokio::test]
    async fn secret_group_push_with_a_stale_revision_is_409() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({ "vars": { "A": "1" }, "expected_revision": 0 });
        let (status, _) = request(
            app.clone(),
            put_json("/v1/projects/myapp/secret-groups/preview", &body),
        )
        .await;
        assert_eq!(status, 200);

        let (status, json) = request(
            app.clone(),
            put_json("/v1/projects/myapp/secret-groups/preview", &body),
        )
        .await;
        assert_eq!(status, 409, "{json:?}");
        assert!(
            json["error"].as_str().unwrap().contains("revision"),
            "{json:?}"
        );
    }

    #[tokio::test]
    async fn secret_group_list_is_scoped_to_one_base_project() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        for (base, group) in [("myapp", "preview"), ("other", "preview")] {
            let body = serde_json::json!({ "vars": { "A": "1" }, "expected_revision": 0 });
            let (status, _) = request(
                app.clone(),
                put_json(
                    &format!("/v1/projects/{base}/secret-groups/{group}"),
                    &body,
                ),
            )
            .await;
            assert_eq!(status, 200);
        }

        let (status, json) =
            request(app.clone(), get_req("/v1/projects/myapp/secret-groups")).await;
        assert_eq!(status, 200);
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["name"], "preview");
        assert_eq!(groups[0]["attached_by"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn head_of_an_absent_group_is_404_and_delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let (status, _) = request(
            app.clone(),
            get_req("/v1/projects/myapp/secret-groups/ghost"),
        )
        .await;
        assert_eq!(status, 404);

        let (status, _) = request(
            app.clone(),
            delete_req("/v1/projects/myapp/secret-groups/ghost"),
        )
        .await;
        assert_eq!(status, 200, "deleting an absent group is not an error");
    }

    #[tokio::test]
    async fn secret_group_push_rejects_an_invalid_group_name() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({ "vars": { "A": "1" }, "expected_revision": 0 });
        let (status, json) = request(
            app,
            put_json("/v1/projects/myapp/secret-groups/Bad_Name", &body),
        )
        .await;
        assert_eq!(status, 400, "{json:?}");
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("^[a-z][a-z0-9-]*$"),
            "the message must match the CLI's: {json:?}"
        );
    }
```

Add a `state_with_secret_groups(dir)` builder next to the module's existing `state_with_environments`, wiring the four new use-cases onto a real `EncryptedFileStore` and `SqliteProjectRepo`, and a `delete_req` helper alongside `get_req`/`put_json` if one does not already exist.

- [ ] **Step 6: Run the HTTP tests to verify they fail**

Run: `rtk cargo test -p rpi secret_group`
Expected: FAIL — 404 on every route; the handlers do not exist.

- [ ] **Step 7: Add the DTOs, routes and handlers**

In `crates/bin/src/proto.rs`:

```rust
/// `PUT /v1/projects/{base}/secret-groups/{group}` (secret-groups spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupPushRequest {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> base64-encoded contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    /// Write only if the stored revision equals this. Absent means an
    /// unconditional write (`--force`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Upsert instead of replacing the group wholesale.
    #[serde(default)]
    pub merge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupPushResponse {
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFileHeadDto {
    pub size: u64,
    pub digest: String,
}

/// Metadata projection. Never contains a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupHeadResponse {
    pub revision: u64,
    /// var name -> digest
    pub vars: BTreeMap<String, String>,
    pub files: BTreeMap<String, SecretFileHeadDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupSummaryDto {
    pub name: String,
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
    pub bytes: u64,
    pub updated_at: i64,
    pub attached_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupsListResponse {
    pub groups: Vec<SecretGroupSummaryDto>,
}
```

In `crates/bin/src/agent/http.rs`, add the routes next to the existing secrets route:

```rust
        .route(
            "/v1/projects/{base}/secret-groups",
            get(list_secret_groups_handler),
        )
        .route(
            "/v1/projects/{base}/secret-groups/{group}",
            put(push_secret_group_handler)
                .get(head_secret_group_handler)
                .delete(delete_secret_group_handler),
        )
        // Per-key counterparts, on the deploy-key path rather than the group
        // path: the head is what `rpi secrets diff` compares against when no
        // `--group` is given, and the apply is what `--apply` triggers after a
        // push (the existing `PUT .../secrets` cannot serve it — it rejects an
        // empty payload, and re-uploading the whole bundle just to restart is
        // the wrong shape).
        .route(
            "/v1/projects/{name}/secrets/head",
            get(head_key_secrets_handler),
        )
        .route(
            "/v1/projects/{name}/secrets/apply",
            post(apply_key_secrets_handler),
        )
```

Handlers — the payload validation is the same as `send_secrets_handler`, so extract that into a helper both call rather than duplicating it:

```rust
/// Validates and decodes an incoming secrets payload: env keys, no newlines
/// in values, per-file and total size ceilings, and `file_mode`. Shared by
/// the per-key and group write paths so anything one accepts the other does.
fn decode_secret_payload(
    vars: BTreeMap<String, String>,
    files: &BTreeMap<String, String>,
    file_mode: Option<u32>,
) -> Result<SecretsBundle, ApiError> {
    for (key, value) in &vars {
        if !pi_infrastructure::dotenv::is_valid_key(key) {
            return Err(ApiError(DomainError::Invalid(format!(
                "invalid env key '{key}'"
            ))));
        }
        if value.contains('\n') {
            return Err(ApiError(DomainError::Invalid(format!(
                "value of '{key}' contains a newline (multi-line values are unsupported)"
            ))));
        }
    }
    let mut decoded = std::collections::BTreeMap::new();
    let mut total: usize = 0;
    for (path, b64) in files {
        pi_infrastructure::secretpath::validate_rel_path(path)
            .map_err(|e| ApiError(DomainError::Invalid(format!("secret file '{path}': {e}"))))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| {
                ApiError(DomainError::Invalid(format!(
                    "secret file '{path}': contents are not valid base64"
                )))
            })?;
        if bytes.len() > crate::proto::MAX_SECRET_FILE_BYTES {
            return Err(ApiError(DomainError::Invalid(format!(
                "secret file '{path}' is {} bytes; max is 1 MiB",
                bytes.len()
            ))));
        }
        total += bytes.len();
        if total > crate::proto::MAX_SECRETS_BUNDLE_BYTES {
            return Err(ApiError(DomainError::Invalid(
                "secret files exceed 8 MiB total".into(),
            )));
        }
        decoded.insert(path.clone(), bytes);
    }
    if let Some(mode) = file_mode {
        pi_domain::secretmode::validate(mode)
            .map_err(|e| ApiError(DomainError::Invalid(format!("[secrets].file_mode: {e}"))))?;
    }
    Ok(SecretsBundle {
        vars,
        files: decoded,
        file_mode,
    })
}

/// Both path segments of a group route: the base is a project name, the group
/// name follows the shared `validate_group_name` rule so the agent's message
/// matches the CLI's exactly.
fn valid_group_path(base: &str, group: &str) -> Result<(), ApiError> {
    if !is_valid_name(base) {
        return Err(ApiError(DomainError::Invalid(
            "project name must match ^[a-z0-9][a-z0-9_-]*$".into(),
        )));
    }
    pi_domain::secretgroup::validate_group_name(group)
        .map_err(|e| ApiError(DomainError::Invalid(e)))
}

async fn push_secret_group_handler(
    State(state): State<AppState>,
    Path((base, group)): Path<(String, String)>,
    Json(req): Json<crate::proto::SecretGroupPushRequest>,
) -> Result<Json<crate::proto::SecretGroupPushResponse>, ApiError> {
    valid_group_path(&base, &group)?;
    let bundle = decode_secret_payload(req.vars, &req.files, req.file_mode)?;
    let (keys, files) = (bundle.vars.len(), bundle.files.len());
    let revision = state
        .push_secret_group
        .execute(&base, &group, bundle, req.expected_revision, req.merge)
        .await
        .map_err(ApiError)?;
    Ok(Json(crate::proto::SecretGroupPushResponse {
        revision,
        keys,
        files,
    }))
}

async fn head_secret_group_handler(
    State(state): State<AppState>,
    Path((base, group)): Path<(String, String)>,
) -> Result<Json<crate::proto::SecretGroupHeadResponse>, ApiError> {
    valid_group_path(&base, &group)?;
    let head = state
        .show_secret_group
        .execute(&base, &group)
        .await
        .map_err(ApiError)?;
    Ok(Json(crate::proto::SecretGroupHeadResponse {
        revision: head.revision,
        vars: head.vars,
        files: head
            .files
            .into_iter()
            .map(|(p, f)| {
                (
                    p,
                    crate::proto::SecretFileHeadDto {
                        size: f.size,
                        digest: f.digest,
                    },
                )
            })
            .collect(),
        file_mode: head.file_mode,
    }))
}

async fn list_secret_groups_handler(
    State(state): State<AppState>,
    Path(base): Path<String>,
) -> Result<Json<crate::proto::SecretGroupsListResponse>, ApiError> {
    if !is_valid_name(&base) {
        return Err(ApiError(DomainError::Invalid(
            "project name must match ^[a-z0-9][a-z0-9_-]*$".into(),
        )));
    }
    let listed = state
        .list_secret_groups
        .execute(&base)
        .await
        .map_err(ApiError)?;
    Ok(Json(crate::proto::SecretGroupsListResponse {
        groups: listed
            .into_iter()
            .map(|g| crate::proto::SecretGroupSummaryDto {
                name: g.summary.name,
                revision: g.summary.revision,
                keys: g.summary.keys,
                files: g.summary.files,
                bytes: g.summary.bytes,
                updated_at: g.summary.updated_at,
                attached_by: g.attached_by,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct GroupDeleteQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_secret_group_handler(
    State(state): State<AppState>,
    Path((base, group)): Path<(String, String)>,
    Query(q): Query<GroupDeleteQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    valid_group_path(&base, &group)?;
    state
        .remove_secret_group
        .execute(&base, &group, q.force)
        .await
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "removed": group })))
}
```

The two per-key handlers:

```rust
async fn head_key_secrets_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<crate::proto::SecretGroupHeadResponse>, ApiError> {
    if !is_valid_name(&name) {
        return Err(ApiError(DomainError::Invalid(
            "project name must match ^[a-z0-9][a-z0-9_-]*$".into(),
        )));
    }
    let head = state
        .head_key_secrets
        .execute(&name)
        .await
        .map_err(ApiError)?;
    Ok(Json(crate::proto::SecretGroupHeadResponse {
        revision: head.revision,
        vars: head.vars,
        files: head
            .files
            .into_iter()
            .map(|(p, f)| {
                (
                    p,
                    crate::proto::SecretFileHeadDto {
                        size: f.size,
                        digest: f.digest,
                    },
                )
            })
            .collect(),
        file_mode: head.file_mode,
    }))
}

async fn apply_key_secrets_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<crate::proto::SecretsApplyResponse>, ApiError> {
    if !is_valid_name(&name) {
        return Err(ApiError(DomainError::Invalid(
            "project name must match ^[a-z0-9][a-z0-9_-]*$".into(),
        )));
    }
    let (keys, files) = state
        .apply_secrets
        .execute(&name, Arc::new(TracingSink))
        .await
        .map_err(ApiError)?;
    Ok(Json(crate::proto::SecretsApplyResponse { keys, files }))
}
```

`head_key_secrets` is a one-line use-case in `crates/application/src/secrets.rs` returning `self.secrets.head(&GroupRef::key(project))` — added next to `ListSecrets` so the HTTP layer never touches the store directly; `apply_secrets` is the `ApplySecrets` from Step 3. Add to `crates/bin/src/proto.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsApplyResponse {
    pub keys: usize,
    pub files: usize,
}
```

Rewrite `send_secrets_handler` to call `decode_secret_payload` instead of its inline copy of the same checks, and to delegate its `apply` branch to `ApplySecrets` so there is one apply implementation. Extend `use axum::routing::{get, put}` with `post`.

In `crates/bin/src/agent/state.rs`, add the four use-cases to `AppState` and construct them where `send_secrets`/`list_secrets` are built, passing the shared `secrets` store and `projects` repo.

- [ ] **Step 8: Run the HTTP tests to verify they pass**

Run: `rtk cargo test -p rpi secret_group`
Expected: PASS — 5 tests.

- [ ] **Step 9: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 10: Commit**

```bash
rtk git add crates/application crates/bin
rtk git commit -m "feat(agent): serve secret-group CRUD endpoints"
```

---

### Task 8: CLI `secrets push` and `secrets diff`

**Files:**
- Modify: `crates/bin/src/cli/api.rs`, `crates/bin/src/cli/commands.rs`, `crates/bin/src/main.rs`, `crates/bin/src/compat.rs`
- Test: inline test modules in `commands.rs`, `compat.rs`, `main.rs`

**Interfaces:**
- Consumes: the group endpoints (Task 7); `collect_secrets` (existing, `commands.rs`).
- Produces: `resolve_base(&Resolved) -> String`, `secrets_push(...)`, `secrets_diff(...)`, `Feature::SecretGroups`,
  and `ApiClient::{push_secret_group, head_secret_group, list_secret_groups, delete_secret_group}`.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/compat.rs` test module:

```rust
    #[test]
    fn secret_groups_feature_is_gated_from_0_27_0() {
        assert_eq!(Feature::SecretGroups.min_version(), "0.27.0");
        assert_eq!(Feature::SecretGroups.wire_name(), "secret-groups");
        assert!(Feature::ALL.contains(&Feature::SecretGroups));
    }
```

Use whatever the accessor names already are in that file (`min_version`/`wire_name` are placeholders for the existing ones — read them and match).

In `crates/bin/src/cli/commands.rs` test module:

```rust
    #[test]
    fn base_comes_from_the_environment_not_the_derived_key() {
        // With --env the resolved project name is the derived key; addressing
        // a group under that key would point at a directory no project owns.
        let resolved = crate::cli::overlay::resolve_from(
            SAMPLE_BASE,
            Some((
                "branch",
                "[ingress]\nhostname = \"x.example.com\"\n\n[secrets]\ngroups = [\"preview\"]\n",
            )),
            &[],
        )
        .unwrap();
        assert_eq!(resolved.rpitoml.project.name, "myapp--branch");
        assert_eq!(resolve_base(&resolved), "myapp");

        let plain = crate::cli::overlay::resolve_from(SAMPLE_BASE, None, &[]).unwrap();
        assert_eq!(resolve_base(&plain), "myapp");
    }

    #[test]
    fn diff_summary_reports_added_changed_and_removed_by_name_only() {
        let local = {
            let mut m = BTreeMap::new();
            m.insert("KEEP".to_string(), "same".to_string());
            m.insert("CHANGED".to_string(), "new-value".to_string());
            m.insert("ADDED".to_string(), "fresh".to_string());
            m
        };
        let remote = {
            let mut m = BTreeMap::new();
            m.insert("KEEP".to_string(), digest_of("same"));
            m.insert("CHANGED".to_string(), digest_of("old-value"));
            m.insert("REMOVED".to_string(), digest_of("gone"));
            m
        };

        let d = diff_vars(&local, &remote);
        assert_eq!(d.added, vec!["ADDED".to_string()]);
        assert_eq!(d.changed, vec!["CHANGED".to_string()]);
        assert_eq!(d.removed, vec!["REMOVED".to_string()]);
        assert_eq!(d.unchanged, 1);

        let rendered = d.render();
        for value in ["same", "new-value", "old-value", "gone", "fresh"] {
            assert!(!rendered.contains(value), "a value leaked: {rendered}");
        }
    }

    fn digest_of(value: &str) -> String {
        pi_domain::secretgroup::digest(value.as_bytes())
    }
```

Add a `SAMPLE_BASE` const to that test module: the minimal `rpi.toml` text used by the overlay tests, with `name = "myapp"` and `hostname = "app.example.com"`.

In `crates/bin/src/main.rs` test module:

```rust
    #[test]
    fn secrets_push_and_diff_parse_with_group_flags() {
        let cli = Cli::try_parse_from([
            "pi", "secrets", "push", "--group", "preview", "--merge", "--force", "--apply",
        ])
        .unwrap();
        match cli.command {
            Cmd::Secrets {
                cmd:
                    SecretsCmd::Push {
                        group,
                        merge,
                        force,
                        apply,
                        ..
                    },
            } => {
                assert_eq!(group.as_deref(), Some("preview"));
                assert!(merge && force && apply);
            }
            _ => panic!("expected secrets push"),
        }
        assert!(Cli::try_parse_from(["pi", "secrets", "diff"]).is_ok());
        assert!(Cli::try_parse_from(["pi", "secrets", "send"]).is_ok(), "alias kept");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p rpi secrets_push && rtk cargo test -p rpi secret_groups_feature && rtk cargo test -p rpi base_comes_from`
Expected: FAIL — unknown variant `Push`, no `Feature::SecretGroups`, no `resolve_base`/`diff_vars`.

- [ ] **Step 3: Add the compat feature**

In `crates/bin/src/compat.rs`, add `SecretGroups` to the `Feature` enum, to `Feature::ALL`, and to each `match` arm block, following the existing shape: wire name `"secret-groups"`, description `"secret groups"`, policy `Policy::Required`, minimum version `"0.27.0"`.

- [ ] **Step 4: Add the client methods**

In `crates/bin/src/cli/api.rs`, following the shape of `send_secrets`:

```rust
    pub async fn push_secret_group(
        &self,
        base: &str,
        group: &str,
        req: &crate::proto::SecretGroupPushRequest,
    ) -> anyhow::Result<crate::proto::SecretGroupPushResponse> {
        let resp = self
            .http
            .put(format!(
                "{}/v1/projects/{base}/secret-groups/{group}",
                self.base
            ))
            .json(req)
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn head_secret_group(
        &self,
        base: &str,
        group: &str,
    ) -> anyhow::Result<crate::proto::SecretGroupHeadResponse> {
        let resp = self
            .http
            .get(format!(
                "{}/v1/projects/{base}/secret-groups/{group}",
                self.base
            ))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn list_secret_groups(
        &self,
        base: &str,
    ) -> anyhow::Result<crate::proto::SecretGroupsListResponse> {
        let resp = self
            .http
            .get(format!("{}/v1/projects/{base}/secret-groups", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn delete_secret_group(
        &self,
        base: &str,
        group: &str,
        force: bool,
    ) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(format!(
                "{}/v1/projects/{base}/secret-groups/{group}?force={force}",
                self.base
            ))
            .send()
            .await?;
        expect_feature(resp, crate::compat::Feature::SecretGroups).await?;
        Ok(())
    }
```

- [ ] **Step 5: Implement `resolve_base`, the diff type and the commands**

In `crates/bin/src/cli/commands.rs`:

```rust
/// Base project that owns a group. With `--env` the resolved
/// `project.name` is the derived deploy key (`myapp--branch--login`), so a
/// group addressed by it would land under a directory no project owns — the
/// base always comes from the environment selection.
pub fn resolve_base(resolved: &crate::cli::overlay::Resolved) -> String {
    match &resolved.env {
        Some(env) => env.base.clone(),
        None => resolved.rpitoml.project.name.clone(),
    }
}

/// What a push would change, by name. Values never appear here — the remote
/// side only ever gave us digests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NameDiff {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

impl NameDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        for (label, names) in [
            ("+", &self.added),
            ("~", &self.changed),
            ("-", &self.removed),
        ] {
            for name in names {
                parts.push(format!("{label}{name}"));
            }
        }
        if parts.is_empty() {
            return format!("no changes ({} unchanged)", self.unchanged);
        }
        format!("{} ({} unchanged)", parts.join(" "), self.unchanged)
    }
}

/// Compares local values against remote digests. `remote` maps name ->
/// digest, so the comparison is digest-to-digest and no local value is sent
/// anywhere to make it.
pub fn diff_vars(
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
) -> NameDiff {
    let mut d = NameDiff::default();
    for (name, value) in local {
        match remote.get(name) {
            None => d.added.push(name.clone()),
            Some(digest) if *digest != pi_domain::secretgroup::digest(value.as_bytes()) => {
                d.changed.push(name.clone())
            }
            Some(_) => d.unchanged += 1,
        }
    }
    for name in remote.keys() {
        if !local.contains_key(name) {
            d.removed.push(name.clone());
        }
    }
    d
}

/// Same comparison for files: local bytes are already base64 here (that is
/// what `collect_secrets` produces), so decode before digesting.
pub fn diff_files(
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, crate::proto::SecretFileHeadDto>,
) -> NameDiff {
    let mut d = NameDiff::default();
    for (path, b64) in local {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap_or_default();
        match remote.get(path) {
            None => d.added.push(path.clone()),
            Some(head) if head.digest != pi_domain::secretgroup::digest(&bytes) => {
                d.changed.push(path.clone())
            }
            Some(_) => d.unchanged += 1,
        }
    }
    for path in remote.keys() {
        if !local.contains_key(path) {
            d.removed.push(path.clone());
        }
    }
    d
}

/// `rpi secrets push`. Without `--group` this targets the deploy key's own
/// bundle and behaves exactly like the pre-groups `rpi secrets send`.
pub async fn secrets_push(
    group: Option<String>,
    merge: bool,
    force: bool,
    apply: bool,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let is_env = resolved.env.is_some();
    let base = resolve_base(&resolved);
    let project_name = resolved.rpitoml.project.name.clone();
    let (vars_map, files) = collect_secrets(Path::new("."), &resolved.rpitoml.secrets)?;
    if vars_map.is_empty() && files.is_empty() {
        anyhow::bail!("no secrets to send: env file has no variables and [secrets].files is empty");
    }
    let file_mode = match &resolved.rpitoml.secrets.file_mode {
        Some(text) => Some(
            pi_domain::secretmode::parse(text)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].file_mode: {e}"))?,
        ),
        None => None,
    };

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Secrets)?;
    if is_env {
        compat.gate(crate::compat::Feature::Environments)?;
    }
    if file_mode.is_some() {
        compat.gate(crate::compat::Feature::SecretModes)?;
    }

    match group {
        Some(group) => {
            pi_domain::secretgroup::validate_group_name(&group)
                .map_err(|e| anyhow::anyhow!("--group: {e}"))?;
            compat.gate(crate::compat::Feature::SecretGroups)?;
            let head = api.head_secret_group(&base, &group).await.ok();
            let expected = if force {
                None
            } else {
                Some(head.as_ref().map(|h| h.revision).unwrap_or(0))
            };
            if let Some(head) = &head {
                let vd = diff_vars(&vars_map, &head.vars);
                let fd = diff_files(&files, &head.files);
                output::info(format!("env keys: {}", vd.render()));
                output::info(format!("files: {}", fd.render()));
            }
            let resp = api
                .push_secret_group(
                    &base,
                    &group,
                    &crate::proto::SecretGroupPushRequest {
                        vars: vars_map,
                        files,
                        file_mode,
                        expected_revision: expected,
                        merge,
                    },
                )
                .await?;
            output::success(format!(
                "group '{base}/{group}' now at revision {} ({} key(s), {} file(s))",
                resp.revision, resp.keys, resp.files
            ));
            if apply {
                apply_to_resolved_project(&api, &project_name, &base, &group).await?;
            }
        }
        None => {
            if !compat.supports(crate::compat::Feature::SecretGroups) {
                output::warn(
                    "this agent predates secret groups: the overwrite guard is unavailable, \
                     so a concurrent change on the agent will be replaced silently",
                );
            }
            let (n, m) = (vars_map.len(), files.len());
            let resp = api
                .send_secrets(&project_name, vars_map, files, file_mode, apply)
                .await?;
            output::success(format!(
                "saved {n} key(s) and {m} file(s) for project '{project_name}'"
            ));
            if resp.applied {
                output::success("secrets applied to running containers");
            }
        }
    }
    Ok(())
}

/// `--apply` after a group push: apply to the project the current config
/// resolves to, and name the others that declare the group as untouched. A
/// fan-out that restarts every attached environment from one command is too
/// abrupt a default.
async fn apply_to_resolved_project(
    api: &crate::cli::api::ApiClient,
    project: &str,
    base: &str,
    group: &str,
) -> anyhow::Result<()> {
    let listed = api.list_secret_groups(base).await?;
    let others: Vec<String> = listed
        .groups
        .iter()
        .filter(|g| g.name == group)
        .flat_map(|g| g.attached_by.iter().cloned())
        .filter(|k| k != project)
        .collect();
    api.apply_key_secrets(project).await?;
    output::success(format!("applied to '{project}'"));
    if !others.is_empty() {
        output::info(format!(
            "also declared by (not applied): {}",
            others.join(", ")
        ));
    }
    Ok(())
}

/// `rpi secrets diff` — local sources against the agent, by digest.
pub async fn secrets_diff(
    group: Option<String>,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let base = resolve_base(&resolved);
    let project_name = resolved.rpitoml.project.name.clone();
    let (vars_map, files) = collect_secrets(Path::new("."), &resolved.rpitoml.secrets)?;

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::SecretGroups)?;

    let (label, head) = match &group {
        Some(group) => (
            format!("group '{base}/{group}'"),
            api.head_secret_group(&base, group).await?,
        ),
        None => (
            format!("project '{project_name}'"),
            api.head_key_secrets(&project_name).await?,
        ),
    };
    output::heading(format!("{label} at revision {}", head.revision));
    output::info(format!("env keys: {}", diff_vars(&vars_map, &head.vars).render()));
    output::info(format!("files: {}", diff_files(&files, &head.files).render()));
    Ok(())
}
```

`api.head_key_secrets(project)` and `api.apply_key_secrets(project)` call the two per-key routes Task 7 added (`GET /v1/projects/{name}/secrets/head` → `SecretGroupHeadResponse`, `POST /v1/projects/{name}/secrets/apply` → `SecretsApplyResponse`). Add them to `api.rs` in the same shape as the group methods above, both gated on `Feature::SecretGroups`.

Rename the existing `secrets_send` to a thin deprecation shim:

```rust
/// Deprecated alias kept so existing scripts keep working; `push` without
/// `--group` is the same operation.
pub async fn secrets_send(
    apply: bool,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    output::warn("`rpi secrets send` is deprecated; use `rpi secrets push`");
    secrets_push(None, false, false, apply, env, vars, connect).await
}
```

- [ ] **Step 6: Add the CLI variants**

In `crates/bin/src/main.rs`, add to `SecretsCmd` (keeping `Send` exactly as it is):

```rust
    /// Push the env file and [secrets].files to a group, or to this project's own bundle
    Push {
        /// Target group (omit to target this deploy key's own bundle)
        #[arg(long)]
        group: Option<String>,
        /// Upsert instead of replacing the group wholesale
        #[arg(long)]
        merge: bool,
        /// Overwrite even if the group changed since it was read
        #[arg(long)]
        force: bool,
        /// Also apply the new secrets to running containers
        #[arg(long)]
        apply: bool,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Compare local secret sources against the agent (by digest, never values)
    Diff {
        /// Target group (omit to compare against this deploy key's own bundle)
        #[arg(long)]
        group: Option<String>,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
```

Wire both in the dispatch `match`, next to the existing `SecretsCmd::Send` arm.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `rtk cargo test -p rpi`
Expected: PASS.

- [ ] **Step 8: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 9: Commit**

```bash
rtk git add crates/bin
rtk git commit -m "feat(cli): add secrets push and secrets diff"
```

---

### Task 9: CLI `secrets group ls` / `group rm` and the effective `secrets ls`

**Files:**
- Modify: `crates/bin/src/main.rs`, `crates/bin/src/cli/commands.rs`, `crates/bin/src/cli/api.rs`, `crates/bin/src/proto.rs`, `crates/bin/src/agent/http.rs`
- Test: inline test modules in `commands.rs`, `main.rs`, `http.rs`

**Interfaces:**
- Consumes: `resolve_base` (Task 8), the group endpoints (Task 7), `MergedSecrets` (Task 2).
- Produces: `SecretsCmd::Group { cmd: SecretsGroupCmd }` with `Ls`/`Rm`, `secrets_group_ls`, `secrets_group_rm`,
  and `SecretsListResponse.layers` carrying per-object provenance.

- [ ] **Step 1: Write the failing tests**

In `crates/bin/src/cli/commands.rs` test module:

```rust
    #[test]
    fn effective_view_marks_shadowed_entries_and_names_the_winning_layer() {
        let resp = crate::proto::SecretsListResponse {
            keys: vec!["A".into(), "B".into()],
            files: vec!["certs/server.pem".into()],
            file_mode: Some(0o640),
            layers: vec![
                crate::proto::SecretLayerDto {
                    label: "common".into(),
                    revision: 3,
                    vars: vec!["A".into(), "B".into()],
                    files: vec![],
                },
                crate::proto::SecretLayerDto {
                    label: "key".into(),
                    revision: 2,
                    vars: vec!["B".into()],
                    files: vec!["certs/server.pem".into()],
                },
            ],
        };

        let rows = effective_rows(&resp);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("A".to_string(), "common".to_string(), false));
        assert_eq!(
            rows[1],
            ("B".to_string(), "key".to_string(), true),
            "B is supplied by key and shadows common"
        );
        assert_eq!(
            rows[2],
            ("certs/server.pem".to_string(), "key".to_string(), false)
        );
    }
```

In `crates/bin/src/main.rs` test module:

```rust
    #[test]
    fn secrets_group_subcommands_parse() {
        assert!(Cli::try_parse_from(["pi", "secrets", "group", "ls"]).is_ok());
        let cli =
            Cli::try_parse_from(["pi", "secrets", "group", "rm", "preview", "--force"]).unwrap();
        match cli.command {
            Cmd::Secrets {
                cmd: SecretsCmd::Group {
                    cmd: SecretsGroupCmd::Rm { name, force, .. },
                },
            } => {
                assert_eq!(name, "preview");
                assert!(force);
            }
            _ => panic!("expected secrets group rm"),
        }
    }
```

In `crates/bin/src/agent/http.rs` test module:

```rust
    #[tokio::test]
    async fn effective_secrets_list_reports_layers_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({
            "vars": { "SHARED": "group-value" },
            "expected_revision": 0
        });
        let (status, _) = request(
            app.clone(),
            put_json("/v1/projects/myapp/secret-groups/common", &body),
        )
        .await;
        assert_eq!(status, 200);

        let (status, json) = request(app, get_req("/v1/projects/myapp/secrets")).await;
        assert_eq!(status, 200);
        let labels: Vec<&str> = json["layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["key"], "no groups declared yet -> key only");
        assert!(!json.to_string().contains("group-value"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p rpi group`
Expected: FAIL — no `SecretsGroupCmd`, no `effective_rows`, no `layers` field.

- [ ] **Step 3: Extend the per-key list response with layers**

In `crates/bin/src/proto.rs`:

```rust
/// One layer of the effective view (secret-groups spec: Attachment and
/// layering). Names only — a layer never carries values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretLayerDto {
    pub label: String,
    pub revision: u64,
    pub vars: Vec<String>,
    pub files: Vec<String>,
}
```

Add to `SecretsListResponse`:

```rust
    /// Absent from agents older than 0.27.0 — the CLI then prints the flat
    /// list it always printed rather than inventing provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<SecretLayerDto>,
```

In `crates/application/src/secrets.rs`, extend `StoredSecrets` with `layers: Vec<(String, u64, Vec<String>, Vec<String>)>` and have `ListSecrets::execute` resolve the project's declared groups plus its key group, in the same order the deploy uses, returning the merged names and the per-layer name lists. `ListSecrets` therefore needs `Arc<dyn ProjectRepository>` alongside its store — add it to the constructor and update `state.rs`.

In `list_secrets_handler`, map those into `layers`.

- [ ] **Step 4: Implement the CLI**

In `crates/bin/src/cli/commands.rs`:

```rust
/// Rows of the effective `rpi secrets ls`: object name, the layer that
/// supplied the winning value, and whether it shadows an earlier layer.
/// Later layers win, so the last layer mentioning a name owns it.
pub fn effective_rows(
    resp: &crate::proto::SecretsListResponse,
) -> Vec<(String, String, bool)> {
    let mut winner: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for layer in &resp.layers {
        for name in layer.vars.iter().chain(layer.files.iter()) {
            let shadows = winner.contains_key(name);
            winner.insert(name.clone(), (layer.label.clone(), shadows));
        }
    }
    winner
        .into_iter()
        .map(|(name, (label, shadows))| (name, label, shadows))
        .collect()
}

pub async fn secrets_group_ls(
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let base = resolve_base(&resolved);
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::SecretGroups)?;

    let listed = api.list_secret_groups(&base).await?;
    if listed.groups.is_empty() {
        output::info(format!("no secret groups for project '{base}'"));
        return Ok(());
    }
    output::heading(format!("secret groups of '{base}':"));
    for g in &listed.groups {
        let attached = if g.attached_by.is_empty() {
            "-".to_string()
        } else {
            g.attached_by.join(", ")
        };
        println!(
            "  {}  r{}  {} key(s), {} file(s), {} B  attached: {attached}",
            g.name, g.revision, g.keys, g.files, g.bytes
        );
    }
    Ok(())
}

pub async fn secrets_group_rm(
    name: String,
    force: bool,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    pi_domain::secretgroup::validate_group_name(&name)
        .map_err(|e| anyhow::anyhow!("group name: {e}"))?;
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let base = resolve_base(&resolved);
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::SecretGroups)?;

    api.delete_secret_group(&base, &name, force).await?;
    output::success(format!("removed secret group '{base}/{name}'"));
    Ok(())
}
```

Extend `secrets_ls` so that with `--group` it prints that group's head (revision, names, digests, sizes) via `head_secret_group`, and without `--group` it prints the effective rows from `effective_rows`, marking a shadowing row with a trailing `(overrides earlier layer)`. Keep the existing flat output when `resp.layers` is empty, which is what a pre-0.27.0 agent returns.

- [ ] **Step 5: Add the CLI variants**

In `crates/bin/src/main.rs`, add to `SecretsCmd`:

```rust
    /// Manage this project's secret groups
    Group {
        #[command(subcommand)]
        cmd: SecretsGroupCmd,
    },
```

and the nested enum next to `SecretsCmd`:

```rust
#[derive(Subcommand)]
enum SecretsGroupCmd {
    /// List the base project's secret groups and who attaches them
    Ls {
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Delete a secret group
    Rm {
        /// Group name
        name: String,
        /// Delete even while a registered project declares it
        #[arg(long)]
        force: bool,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
}
```

Add a `--group` flag to the existing `SecretsCmd::Ls` variant and wire every new arm in the dispatch `match`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `rtk cargo test -p rpi`
Expected: PASS.

- [ ] **Step 7: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 8: Commit**

```bash
rtk git add crates/bin crates/application
rtk git commit -m "feat(cli): list and remove secret groups, show the effective view"
```

---

### Task 10: Lifecycle — group ownership on teardown

**Files:**
- Modify: `crates/application/src/remove.rs`, `crates/bin/src/cli/commands.rs` (the `rpi rm` confirmation)
- Test: inline test modules in `remove.rs` and `commands.rs`

**Interfaces:**
- Consumes: `SecretStore::remove_base` (Task 3), `ProjectConfig.environment` (existing).
- Produces: the guarantee later tasks and the e2e scenario rely on — `rpi rm <base>` drops the base's groups, `rpi env destroy` never does.

- [ ] **Step 1: Write the failing tests**

In `crates/application/src/remove.rs` test module:

```rust
    #[tokio::test]
    async fn removing_a_base_project_drops_its_declared_groups() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_remove()
            .withf(|r| *r == GroupRef::key("myapp"))
            .times(1)
            .returning(|_| Ok(()));
        secrets
            .expect_remove_base()
            .withf(|base| base == "myapp")
            .times(1)
            .returning(|_| Ok(()));

        remove_project_with(secrets, project("myapp", None)).await.unwrap();
    }

    #[tokio::test]
    async fn removing_an_environment_keeps_the_base_groups() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_remove()
            .withf(|r| *r == GroupRef::key("myapp--branch--x"))
            .times(1)
            .returning(|_| Ok(()));
        // The whole point of groups: tearing down one environment must not
        // take the shared secrets every other environment attaches.
        secrets.expect_remove_base().times(0);

        remove_project_with(secrets, project("myapp--branch--x", Some(env_meta())))
            .await
            .unwrap();
    }
```

Add a `remove_project_with(secrets, project)` helper to that module's tests that builds `RemoveProject` with the given store and permissive mocks for everything else, mirroring the existing test setup.

In `crates/bin/src/cli/commands.rs` test module:

```rust
    #[test]
    fn rm_confirmation_names_groups_and_affected_environments() {
        let text = rm_confirmation_text("myapp", 2, &["myapp--test".into()], true);
        assert!(text.contains("2 secret group(s)"), "got: {text}");
        assert!(text.contains("myapp--test"), "got: {text}");

        let plain = rm_confirmation_text("myapp", 0, &[], false);
        assert!(!plain.contains("secret group"), "got: {plain}");
        assert!(!plain.contains("environment"), "got: {plain}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-application remove && rtk cargo test -p rpi rm_confirmation`
Expected: FAIL — `expect_remove_base` unsatisfied; `rm_confirmation_text` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/application/src/remove.rs`, after the existing `self.secrets.remove(...)` call:

```rust
        // Declared groups belong to the base project, so they go with it —
        // but never with an environment, whose teardown must leave the shared
        // groups its siblings attach untouched.
        if existing.config.environment.is_none() {
            self.secrets.remove_base(project).await?;
        }
```

(`existing` is the project row `RemoveProject::execute` already fetched; reuse it rather than re-fetching.)

In `crates/bin/src/cli/commands.rs`, extract the confirmation text so it is testable and extend it:

```rust
/// Confirmation text for `rpi rm`. Groups and still-registered environments
/// are named because deleting a base project takes its groups with it, and
/// any environment left behind will fail its next deploy on the missing
/// group — loud beats silent.
pub fn rm_confirmation_text(
    project: &str,
    groups: usize,
    environments: &[String],
    with_volumes: bool,
) -> String {
    let mut text = format!(
        "this removes containers{}, the ingress route, workdir, secrets, deploy key and history of '{project}'",
        if with_volumes { " and volumes" } else { "" }
    );
    if groups > 0 {
        text.push_str(&format!(", plus {groups} secret group(s)"));
    }
    if !environments.is_empty() {
        text.push_str(&format!(
            "\nenvironments that would lose those groups and fail their next deploy: {}",
            environments.join(", ")
        ));
    }
    text
}
```

In the `rm` command, before prompting, fetch `api.list_secret_groups(project)` and `api.list_environments(Some(project))` (both best-effort: on a pre-0.27.0 agent the group call fails, and the text then simply omits the group clause) and pass their counts into `rm_confirmation_text`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-application remove && rtk cargo test -p rpi rm_confirmation`
Expected: PASS.

- [ ] **Step 5: Update the architecture docs**

In `docs/architecture/flows/secrets.md` and `docs/architecture/flows/environments.md`, state the ownership rule: `rpi rm <base>` removes the base's groups; `rpi env destroy` and the TTL reaper remove only the key bundle.

- [ ] **Step 6: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
rtk git add crates/application crates/bin docs/architecture
rtk git commit -m "feat(secrets): tie group ownership to the base project on teardown"
```

---

### Task 11: Conditional writes on the per-key path

**Files:**
- Modify: `crates/bin/src/proto.rs`, `crates/bin/src/agent/http.rs`, `crates/application/src/secrets.rs`, `crates/bin/src/cli/commands.rs`
- Test: inline test modules in `http.rs` and `secrets.rs`

**Interfaces:**
- Consumes: `SecretStore::save` with `expected` (Task 3).
- Produces: `SecretsSendRequest.expected_revision` and `SecretsSendResponse.revision`; `SendSecrets::execute` gains an `expected: Option<u64>` parameter.

- [ ] **Step 1: Write the failing tests**

In `crates/application/src/secrets.rs` test module:

```rust
    #[tokio::test]
    async fn send_passes_the_expected_revision_through_to_the_store() {
        let mut m = mocks();
        m.secrets
            .expect_save()
            .withf(|r, _, expected| *r == GroupRef::key("rateme") && *expected == Some(4))
            .times(1)
            .returning(|_, _, _| Ok(5));

        let saved = build(m)
            .execute("rateme", bundle(), Some(4), false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(saved.revision, 5);
    }

    #[tokio::test]
    async fn a_conflict_from_the_store_surfaces_unchanged() {
        let mut m = mocks();
        m.secrets.expect_save().returning(|_, _, _| {
            Err(DomainError::Conflict("secret group changed since revision 1".into()))
        });
        let err = build(m)
            .execute("rateme", bundle(), Some(1), false, CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
    }
```

In `crates/bin/src/agent/http.rs` test module:

```rust
    #[tokio::test]
    async fn per_key_secrets_put_honours_expected_revision() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({ "vars": { "A": "1" }, "expected_revision": 0 });
        let (status, json) =
            request(app.clone(), put_json("/v1/projects/rateme/secrets", &body)).await;
        assert_eq!(status, 200, "{json:?}");
        assert_eq!(json["revision"], 1);

        let (status, json) =
            request(app, put_json("/v1/projects/rateme/secrets", &body)).await;
        assert_eq!(status, 409, "{json:?}");
    }

    #[tokio::test]
    async fn per_key_secrets_put_without_expected_revision_still_writes() {
        let dir = tempfile::tempdir().unwrap();
        let app = state_with_secret_groups(dir.path());
        let body = serde_json::json!({ "vars": { "A": "1" } });
        for _ in 0..2 {
            let (status, _) =
                request(app.clone(), put_json("/v1/projects/rateme/secrets", &body)).await;
            assert_eq!(
                status, 200,
                "an old CLI that sends no expected_revision must keep working"
            );
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `rtk cargo test -p pi-application secrets && rtk cargo test -p rpi per_key_secrets`
Expected: FAIL — `execute` takes 4 arguments; no `revision` in the response.

- [ ] **Step 3: Write the implementation**

In `crates/bin/src/proto.rs`, add to `SecretsSendRequest`:

```rust
    /// Write only if the stored revision equals this. Absent means an
    /// unconditional write — which is what every CLI before 0.27.0 sends, so
    /// the guard is opt-in on this path and never breaks an old client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
```

and to `SecretsSendResponse`:

```rust
    #[serde(default)]
    pub revision: u64,
```

In `crates/application/src/secrets.rs`, add `expected: Option<u64>` to `SendSecrets::execute` (after `bundle`), pass it to `save`, and put the returned revision on `SecretsSaved`:

```rust
pub struct SecretsSaved {
    pub keys: usize,
    pub files: usize,
    pub applied: bool,
    pub revision: u64,
}
```

In `send_secrets_handler`, pass `req.expected_revision` through and return `saved.revision`. In `api.rs`, add the field to the request `send_secrets` builds. In `secrets_push`'s no-group branch, read the current revision with `api.head_key_secrets` (Task 8) and send `Some(revision)` unless `--force`, keeping the pre-0.27.0 warning path sending `None`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `rtk cargo test -p pi-application secrets && rtk cargo test -p rpi per_key_secrets`
Expected: PASS.

- [ ] **Step 5: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
rtk git add crates/bin crates/application
rtk git commit -m "feat(secrets): guard per-key writes with an expected revision"
```

---

### Task 12: End-to-end scenario, skills and compatibility table

**Files:**
- Create: `tests/e2e/scenarios/secret-groups/scenario.sh`, `tests/e2e/scenarios/secret-groups/app/rpi.toml`, `tests/e2e/scenarios/secret-groups/app/rpi.branch.toml`, `tests/e2e/scenarios/secret-groups/app/compose.yaml`
- Modify: the `rpi-toml` and `rpi-cli` skill files under `plugins/`, `docs/architecture/flows/secrets.md`
- Test: the scenario itself

**Interfaces:**
- Consumes: everything above, through the real CLI and agent.
- Produces: the executable proof that two branch environments share one pushed group.

- [ ] **Step 1: Write the scenario fixture**

`tests/e2e/scenarios/secret-groups/app/rpi.toml` — a base project whose overlay declares a group. Copy `tests/e2e/app.default/rpi.toml` and add:

```toml
[secrets]
env = ".env.shared"
groups = ["shared"]
```

`tests/e2e/scenarios/secret-groups/app/rpi.branch.toml`:

```toml
[source]
branch = "${BRANCH_NAME}"

[ingress]
hostname = ""

[secrets]
env = ".env.shared"
groups = ["shared"]
```

Copy `compose.yaml` from `tests/e2e/scenarios/secret-file-perms/app/compose.yaml`, and give it a command that prints the injected variable so the scenario can assert on it through `rpi command`.

- [ ] **Step 2: Write the scenario**

`tests/e2e/scenarios/secret-groups/scenario.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Secret-groups spec: a group is pushed once for the base project, and every
# branch environment that declares it gets those secrets without a second
# upload. Before groups, each new slug produced a new deploy key with an
# empty bundle, so this scenario is the regression test for the whole point
# of the feature.

source /opt/e2e/lib.sh
e2e_bootstrap

printf 'SHARED_TOKEN=group-token-value\n' > .env.shared

# One push, addressed to the base project's group.
run_capture push.log rpi secrets push --group shared "${CONNECT[@]}"
assert_log push.log "group 'e2e-fixture/shared' now at revision 1"

run_capture groups.log rpi secrets group ls "${CONNECT[@]}"
assert_log groups.log 'shared'

# Two different branches, no secrets sent for either of them.
run_capture a.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_deploy_log a.log
assert_log a.log 'groups: shared@r1, key@r0'

run_capture b.log rpi deploy --env branch --vars BRANCH_NAME=feature/two "${CONNECT[@]}"
assert_deploy_log b.log

for slug in feature-one feature-two; do
  run_capture "read-$slug.log" rpi command print-token \
    --env branch --vars "BRANCH_NAME=feature/${slug#feature-}" "${CONNECT[@]}"
  assert_log "read-$slug.log" 'group-token-value'
done

# Rotation: one push, both environments pick it up on their next deploy.
printf 'SHARED_TOKEN=rotated-token-value\n' > .env.shared
run_capture rotate.log rpi secrets push --group shared "${CONNECT[@]}"
assert_log rotate.log 'revision 2'
assert_log rotate.log '~SHARED_TOKEN'

run_capture redeploy.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_log redeploy.log 'groups: shared@r2, key@r0'
run_capture read-rotated.log rpi command print-token \
  --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_log read-rotated.log 'rotated-token-value'

# A stale push is refused rather than silently reverting the rotation.
run_capture conflict.log rpi secrets push --group shared --force "${CONNECT[@]}"
assert_log conflict.log 'revision 3'

# Destroying one environment must not take the shared group with it.
run_capture destroy.log rpi env destroy branch --vars BRANCH_NAME=feature/two --yes "${CONNECT[@]}"
run_capture groups-after.log rpi secrets group ls "${CONNECT[@]}"
assert_log groups-after.log 'shared'

run_capture recreate.log rpi deploy --env branch --vars BRANCH_NAME=feature/two "${CONNECT[@]}"
assert_deploy_log recreate.log
assert_log recreate.log 'groups: shared@r3, key@r0'

# A declared group that does not exist fails loudly at the secrets stage.
run_capture rm.log rpi secrets group rm shared --force "${CONNECT[@]}"
if run_capture missing.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"; then
  fail 'deploy succeeded with a missing declared group'
fi
assert_log missing.log "secret group 'shared'"

echo 'rpi e2e: PASS'
```

Match the exact helper names in `tests/e2e/lib.sh` (`run_capture`, `assert_log`, `assert_deploy_log`, `fail`, `CONNECT`, `SSH`) and the flag `rpi env destroy` actually takes for non-interactive confirmation — read `lib.sh` and `main.rs` rather than assuming.

- [ ] **Step 3: Register and run the scenario**

Run: `node tests/e2e/run.mjs --scenario secret-groups`
Expected: `rpi e2e: PASS`. If `run.mjs` enumerates scenarios from a list rather than the directory, add `secret-groups` to it.

- [ ] **Step 4: Update the skills and the compatibility table**

In the `rpi-toml` skill: document `[secrets].groups` — an array of group names, applied in order, replaced wholesale by an overlay, `groups = []` detaches, names match `^[a-z][a-z0-9-]*$`.

In the `rpi-cli` skill: document `rpi secrets push` (with `--group`, `--merge`, `--force`, `--apply`), `rpi secrets diff`, `rpi secrets ls --group`, `rpi secrets group ls`, `rpi secrets group rm`, and that `rpi secrets send` is a deprecated alias. State that values are never returned by any command.

Add `secret-groups` (0.27.0) to the compatibility table wherever the existing features are listed in docs.

In `docs/architecture/flows/secrets.md`, add the conditional-write path: revision, `expected_revision`, 409, and the `--force` bypass.

- [ ] **Step 5: Run the full gates**

Run: `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`
Expected: all green.

- [ ] **Step 6: Verify the architecture docs match the code**

Run the `architecture-audit` skill and fix any drift it reports in the three documents this plan touched.

- [ ] **Step 7: Commit**

```bash
rtk git add tests/e2e docs plugins
rtk git commit -m "test(e2e): prove branch environments share one pushed secret group"
```

---

## Self-Review

**Spec coverage.** Every spec section maps to a task: Data model → Tasks 1–3; digests → Task 1; storage layout and no-migration → Task 3; attachment and layering → Tasks 4–6; limits on the merged set → Task 2 (`merge_layers`) wired in Task 6; CLI surface → Tasks 8–9 (`push`, `ls`, `diff`, `group ls`, `group rm`, `send` alias); wire protocol → Tasks 7, 9, 11; compatibility gating and the pre-0.27.0 warning → Task 8; lifecycle → Task 10; error handling table → Tasks 6 (missing group), 7 (attached `group rm`, invalid name), 3 (stale revision), 2 (size ceiling); safety invariants → Tasks 3, 6, 7 tests; testing section → the unit/store/HTTP tests in each task plus Task 12's e2e; documentation → Tasks 6, 10, 12.

**Deviations from the spec, deliberate:** the spec's `GroupHead` sketch nests size and digest in a tuple; the plan uses a named `FileHead`/`SecretFileHeadDto` instead, because a tuple field in a serialized DTO reads as `[4, "ab12"]` on the wire. The spec lists three implementation phases; the plan's twelve tasks are those phases cut at reviewable boundaries — Tasks 1–3 are phase 1, 4–10 are phase 2, 11 is phase 3, and 12 is the cross-phase proof.

**Cross-task couplings the implementer should expect:** Task 6 defines `effective_secrets` in `crates/application/src/lib.rs`, and Tasks 7 (`ApplySecrets`) and 9 (the effective view) both call it — implementing 7 or 9 before 6 means writing that function there instead. Task 7 adds two per-key routes (`/secrets/head`, `/secrets/apply`) that Task 8's `--apply` and `diff` consume, and Task 11 extends the same per-key write path. Tasks 7, 9 and 11 all touch `AppState` in `crates/bin/src/agent/state.rs`; expect a small merge there when they land in sequence.

**Verified during self-review, worth stating:** `MergedSecrets.revisions` (Task 2) is the single source of the provenance line in Task 6 and the layer list in Task 9. `DomainError::Conflict` already maps to HTTP 409 in `crates/bin/src/agent/http.rs:83`, so the 409 assertions in Tasks 7 and 11 need no new mapping. `ProjectConfig` has no `Default` impl, which is why Task 5 spends a step fixing every struct literal instead of expecting the field to default in.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-26-secret-groups.md`.

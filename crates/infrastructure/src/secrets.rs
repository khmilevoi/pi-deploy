use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use age::secrecy::ExposeSecret;
use async_trait::async_trait;
use base64::Engine as _;
use pi_domain::contracts::SecretStore;
use pi_domain::entities::SecretsBundle;
use pi_domain::error::DomainError;
use pi_domain::secretgroup::{validate_group_name, GroupHead, GroupRef, GroupSummary, SecretGroup};
use serde::{Deserialize, Serialize};

use crate::dotenv;
use crate::fsutil;

fn secrets_err(msg: impl std::fmt::Display) -> DomainError {
    DomainError::Secrets(msg.to_string())
}

/// On-disk plaintext (before age encryption): JSON with base64 file bodies.
#[derive(Serialize, Deserialize)]
struct StoredBundle {
    vars: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Absent in bundles written before 0.26.0 — those load as `None` and get
    /// the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_mode: Option<u32>,
    /// Absent in bundles written before groups existed — those load as 0, so
    /// the first conditional write against them (`expected: Some(0)`)
    /// succeeds and no migration is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
}

/// age-encrypted bundles at <data_dir>/secrets/<project>.secrets.age (legacy
/// <project>.env.age is read as fallback and dropped on the next save); the
/// agent key is generated on first start at <data_dir>/secret.key, 0600
/// (§10, §17).
pub struct EncryptedFileStore {
    dir: PathBuf,
    identity: age::x25519::Identity,
    /// Serializes `save`'s read-compare-write critical section. Without it,
    /// two concurrent guarded saves against the same group can both read the
    /// same `current` revision, both pass the `expected` check, and the
    /// second silently overwrites the first — exactly the lost update the
    /// guard exists to prevent. Store-wide (not per-group) granularity is
    /// fine: secret writes are rare, so contention costs nothing. Must be a
    /// `tokio::sync::Mutex`, not `std`'s — the guard is held across `.await`.
    save_lock: tokio::sync::Mutex<()>,
}

impl EncryptedFileStore {
    pub fn open(data_dir: &Path) -> Result<Arc<EncryptedFileStore>, DomainError> {
        std::fs::create_dir_all(data_dir).map_err(secrets_err)?;
        let key_path = data_dir.join("secret.key");
        let identity = open_or_create_identity(&key_path)?;
        let dir = data_dir.join("secrets");
        std::fs::create_dir_all(&dir).map_err(secrets_err)?;
        Ok(Arc::new(EncryptedFileStore {
            dir,
            identity,
            save_lock: tokio::sync::Mutex::new(()),
        }))
    }

    fn bundle_path(&self, project: &str) -> Result<PathBuf, DomainError> {
        let project = validated_project(project)?;
        Ok(self.dir.join(format!("{project}.secrets.age")))
    }

    fn legacy_path(&self, project: &str) -> Result<PathBuf, DomainError> {
        let project = validated_project(project)?;
        Ok(self.dir.join(format!("{project}.env.age")))
    }

    /// `<dir>/groups/<base>/<name>.age`. A group name cannot contain a path
    /// separator (`validate_group_name`), and the base goes through the same
    /// `validated_project` check as a deploy key, so the result is always one
    /// file two levels below `self.dir`.
    fn group_path(&self, base: &str, name: &str) -> Result<PathBuf, DomainError> {
        let base = validated_project(base)?;
        validate_group_name(name).map_err(secrets_err)?;
        Ok(self
            .dir
            .join("groups")
            .join(base)
            .join(format!("{name}.age")))
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
}

fn validated_project(project: &str) -> Result<&str, DomainError> {
    if project.is_empty()
        || project == "."
        || project.contains("..")
        || project.contains('/')
        || project.contains('\\')
    {
        return Err(secrets_err(format!("invalid project name: {project:?}")));
    }
    Ok(project)
}

fn read_identity(path: &Path) -> Result<age::x25519::Identity, DomainError> {
    fs::read_to_string(path)
        .map_err(secrets_err)?
        .trim()
        .parse::<age::x25519::Identity>()
        .map_err(secrets_err)
}

fn open_or_create_identity(path: &Path) -> Result<age::x25519::Identity, DomainError> {
    let identity = age::x25519::Identity::generate();
    let contents = identity.to_string();
    if fsutil::write_private_exclusive(path, contents.expose_secret().as_bytes())
        .map_err(secrets_err)?
    {
        Ok(identity)
    } else {
        read_identity(path)
    }
}

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
        // Held across the read (`load`) below and the write at the bottom of
        // this function, so the whole read-compare-write is one atomic
        // section per store — see `save_lock`'s doc comment.
        let _guard = self.save_lock.lock().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use pi_domain::secretgroup::GroupRef;

    fn bundle() -> SecretsBundle {
        let mut b = SecretsBundle::default();
        b.vars
            .insert("DB_PASSWORD".into(), "super-secret-value".into());
        b.vars.insert("PORT".into(), "3000".into());
        b.files
            .insert("certs/server.pem".into(), vec![0u8, 159, 146, 150]); // non-UTF8 binary
        b
    }

    #[tokio::test]
    async fn save_load_roundtrips_vars_and_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        assert_eq!(
            store.load(&GroupRef::key("rateme")).await.unwrap().objects,
            bundle()
        );
    }

    #[tokio::test]
    async fn load_missing_project_returns_empty_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        assert!(store
            .load(&GroupRef::key("nope"))
            .await
            .unwrap()
            .objects
            .is_empty());
    }

    #[tokio::test]
    async fn reopened_store_reuses_key_and_decrypts_old_bundles() {
        let dir = tempfile::tempdir().unwrap();
        EncryptedFileStore::open(dir.path())
            .unwrap()
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        let reopened = EncryptedFileStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened
                .load(&GroupRef::key("rateme"))
                .await
                .unwrap()
                .objects,
            bundle()
        );
    }

    #[tokio::test]
    async fn load_falls_back_to_legacy_env_age_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        // simulate a pre-secrets agent: dotenv text encrypted at <p>.env.age
        let legacy =
            age::encrypt(&store.identity.to_public(), b"DB_PASSWORD=old-secret\n").unwrap();
        std::fs::write(dir.path().join("secrets").join("rateme.env.age"), legacy).unwrap();

        let loaded = store.load(&GroupRef::key("rateme")).await.unwrap().objects;
        assert_eq!(loaded.vars["DB_PASSWORD"], "old-secret");
        assert!(loaded.files.is_empty());
    }

    #[tokio::test]
    async fn save_removes_legacy_env_age_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let legacy_path = dir.path().join("secrets").join("rateme.env.age");
        let legacy = age::encrypt(&store.identity.to_public(), b"A=1\n").unwrap();
        std::fs::write(&legacy_path, legacy).unwrap();

        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();

        assert!(!legacy_path.exists(), "legacy bundle must be removed");
        assert!(dir
            .path()
            .join("secrets")
            .join("rateme.secrets.age")
            .exists());
        assert_eq!(
            store.load(&GroupRef::key("rateme")).await.unwrap().objects,
            bundle()
        );
    }

    #[tokio::test]
    async fn remove_deletes_both_formats() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        let legacy = age::encrypt(&store.identity.to_public(), b"A=1\n").unwrap();
        std::fs::write(dir.path().join("secrets").join("rateme.env.age"), legacy).unwrap();

        store.remove(&GroupRef::key("rateme")).await.unwrap();

        assert!(std::fs::read_dir(dir.path().join("secrets"))
            .unwrap()
            .next()
            .is_none());
    }

    #[tokio::test]
    async fn bundle_on_disk_is_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        let raw = std::fs::read(dir.path().join("secrets").join("rateme.secrets.age")).unwrap();
        for needle in [
            b"super-secret-value".as_slice(),
            b"certs/server.pem".as_slice(),
        ] {
            assert!(!raw.windows(needle.len()).any(|w| w == needle));
        }
    }

    #[tokio::test]
    async fn invalid_project_names_are_rejected_without_escaping_secrets_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();

        for project in ["", "..", "../escape", "nested/project", r"nested\project"] {
            let result = store.save(&GroupRef::key(project), &bundle(), None).await;
            assert!(
                matches!(result, Err(DomainError::Secrets(_))),
                "{project:?}"
            );
        }

        assert!(!dir.path().join("escape.env.age").exists());
        assert!(!dir.path().join("nested").exists());
        assert!(std::fs::read_dir(dir.path().join("secrets"))
            .unwrap()
            .next()
            .is_none());
    }

    #[tokio::test]
    async fn overwriting_bundle_preserves_encryption_and_loads_latest_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let mut updated = bundle();
        updated.vars.insert("DB_PASSWORD".into(), "rotated".into());

        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        store
            .save(&GroupRef::key("rateme"), &updated, None)
            .await
            .unwrap();

        assert_eq!(
            store.load(&GroupRef::key("rateme")).await.unwrap().objects,
            updated
        );
        let raw = std::fs::read(dir.path().join("secrets").join("rateme.secrets.age")).unwrap();
        for plaintext in [b"super-secret-value".as_slice(), b"rotated".as_slice()] {
            assert!(!raw.windows(plaintext.len()).any(|w| w == plaintext));
        }
    }

    #[test]
    fn opening_with_existing_key_reuses_persisted_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("secret.key"),
            identity.to_string().expose_secret().as_bytes(),
        )
        .unwrap();

        let store = EncryptedFileStore::open(dir.path()).unwrap();

        assert_eq!(
            store.identity.to_public().to_string(),
            identity.to_public().to_string()
        );
    }

    #[tokio::test]
    async fn file_mode_survives_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let mut b = bundle();
        b.file_mode = Some(0o640);
        store
            .save(&GroupRef::key("rateme"), &b, None)
            .await
            .unwrap();
        let loaded = store.load(&GroupRef::key("rateme")).await.unwrap().objects;
        assert_eq!(loaded.file_mode, Some(0o640));
    }

    #[tokio::test]
    async fn a_bundle_stored_before_modes_existed_loads_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        let loaded = store.load(&GroupRef::key("rateme")).await.unwrap().objects;
        assert_eq!(loaded.file_mode, None);
        assert_eq!(loaded.secret_file_mode(), 0o644);
        assert_eq!(loaded.env_mode(), 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn key_and_bundle_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        store
            .save(&GroupRef::key("rateme"), &bundle(), None)
            .await
            .unwrap();
        for file in ["secret.key", "secrets/rateme.secrets.age"] {
            let mode = std::fs::metadata(dir.path().join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
    }

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
    async fn concurrent_guarded_saves_serialize_so_exactly_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = EncryptedFileStore::open(dir.path()).unwrap();
        let r = GroupRef::named("myapp", "preview");

        // Both racers assume the group is still freshly created (revision
        // 0). Without serializing `save`'s read-compare-write section, both
        // could read revision 0 concurrently, both pass the `expected(0)`
        // guard, and the second would silently clobber the first on disk —
        // the lost update the guard exists to prevent — while both report
        // success. `tokio::join!` polls both futures on this one task, so
        // the assertion below is deterministic: it holds regardless of
        // which racer's internal await points happen to interleave first.
        let bundle_a = bundle();
        let bundle_b = bundle();
        let (a, b) = tokio::join!(
            store.save(&r, &bundle_a, Some(0)),
            store.save(&r, &bundle_b, Some(0)),
        );

        let results = [&a, &b];
        let ok_count = results.iter().filter(|res| res.is_ok()).count();
        let conflict_count = results
            .iter()
            .filter(|res| matches!(res, Err(DomainError::Conflict(_))))
            .count();
        assert_eq!(
            (ok_count, conflict_count),
            (1, 1),
            "exactly one concurrent guarded save must win and the other must see \
             a conflict, not both silently succeeding: {a:?} / {b:?}"
        );
        assert_eq!(
            store.load(&r).await.unwrap().revision,
            1,
            "one winning write must land as revision 1, not two writes collapsed into one"
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
        // No group was ever validly named, so the `groups` directory itself
        // must never have been created as a side effect of a rejected save.
        // (Not `.join("secrets/groups").join("..")`: on Windows, `GetFileAttributesW`
        // collapses `..` lexically before touching the filesystem, so that
        // path would report existing even though `groups` never did — a
        // POSIX-only check would silently pass on `windows-latest` CI.)
        assert!(!dir.path().join("secrets").join("groups").exists());
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
}

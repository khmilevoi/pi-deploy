//! What a secret group *is*: its identity, its metadata projection, and how
//! layers of groups combine at deploy time (secret-groups spec: Data model,
//! Attachment and layering). No I/O — the store lives in
//! `pi-infrastructure`, the orchestration in `pi-application`.

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
        return Err(format!("group name '{name}' must match ^[a-z][a-z0-9-]*$"));
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
            assert!(
                validate_group_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
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

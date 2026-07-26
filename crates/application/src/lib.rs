pub mod command;
pub mod deploy;
pub mod diagnostics;
pub mod environments;
pub mod gc;
pub mod lifecycle;
pub mod list;
pub mod logs;
pub mod mask;
pub mod remove;
pub mod scheduler;
pub mod secretgroups;
pub mod secrets;
pub mod stats;
pub mod tail;

#[cfg(test)]
pub mod test_support;

/// Ceiling for the merged secret payload, mirroring
/// `crate::proto::MAX_SECRETS_BUNDLE_BYTES` in the bin crate (8 MiB). Kept
/// here because the merge happens in this crate and `pi-application` must
/// never depend on bin-crate code.
pub const MAX_MERGED_SECRET_BYTES: usize = 8 * 1024 * 1024;

/// Loads one project's layer stack in deploy order: every group in `groups`,
/// resolved against `base`, then `key_project`'s own bundle last
/// (secret-groups spec: Attachment and layering) — the single place "declared
/// groups, then key, in order" is written down, so `effective_secrets` (which
/// injects) and `ListSecrets` (which lists) can never resolve layers
/// differently.
///
/// `require_non_empty` is the one behavior the two callers must *not* share:
/// an injection (`effective_secrets`, and therefore `rpi deploy` and every
/// `--apply` path) must fail fast on a declared-but-empty group, before
/// anything is written — the application would otherwise start without
/// configuration it depends on. A listing (`ListSecrets`) must not: an
/// operator debugging exactly that condition still needs to see everything
/// else `rpi secrets ls` would show.
pub async fn load_layer_stack(
    secrets: &dyn pi_domain::contracts::SecretStore,
    base: &str,
    key_project: &str,
    groups: &[String],
    require_non_empty: bool,
) -> Result<Vec<(String, pi_domain::secretgroup::SecretGroup)>, pi_domain::error::DomainError> {
    use pi_domain::error::DomainError;
    use pi_domain::secretgroup::GroupRef;

    let mut loaded = Vec::with_capacity(groups.len() + 1);
    for name in groups {
        let group = secrets.load(&GroupRef::named(base, name)).await?;
        if require_non_empty && group.objects.is_empty() {
            return Err(DomainError::NotFound(format!(
                "secret group '{name}' of project '{base}' has no secrets; \
                 push it with `rpi secrets push --group {name}` before deploying"
            )));
        }
        loaded.push((name.clone(), group));
    }
    loaded.push((
        "key".to_string(),
        secrets.load(&GroupRef::key(key_project)).await?,
    ));
    Ok(loaded)
}

/// Loads a project's effective secrets: every declared group in order, then
/// the deploy key's own bundle on top (secret-groups spec: Attachment and
/// layering). A declared group with no secrets is `NotFound` — an application
/// started without its secrets breaks later and less legibly than a deploy
/// that refuses to start.
pub async fn effective_secrets(
    secrets: &dyn pi_domain::contracts::SecretStore,
    config: &pi_domain::entities::ProjectConfig,
) -> Result<pi_domain::secretgroup::MergedSecrets, pi_domain::error::DomainError> {
    use pi_domain::secretgroup::{merge_layers, Layer};

    let base = config
        .environment
        .as_ref()
        .map(|e| e.base.clone())
        .unwrap_or_else(|| config.name.clone());
    let loaded =
        load_layer_stack(secrets, &base, &config.name, &config.secret_groups, true).await?;

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

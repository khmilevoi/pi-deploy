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

/// Ceiling for the merged secret payload (8 MiB). An alias, not a second
/// definition: the constant lives in `pi_domain::secretgroup` next to
/// `merge_layers`, so a stored group and a merged set can never be bounded by
/// two numbers that drift apart.
pub use pi_domain::secretgroup::MAX_SECRET_BUNDLE_BYTES as MAX_MERGED_SECRET_BYTES;

/// The project that owns a deploy key's declared groups: an environment
/// overlay borrows its *base* project's groups, every other key owns its own.
/// One function, so `effective_secrets`, `ListSecrets` and `StreamLogs`
/// cannot disagree about which project a `groups = [...]` entry points at.
pub fn group_base(config: &pi_domain::entities::ProjectConfig) -> &str {
    config
        .environment
        .as_ref()
        .map(|e| e.base.as_str())
        .unwrap_or(config.name.as_str())
}

/// Merges an already-loaded layer stack under the one shared ceiling and
/// records each layer's revision. Split out of `effective_secrets` so the
/// read-only callers (`ListSecrets`, `StreamLogs`) merge exactly the same way
/// without inheriting the fail-fast-on-an-empty-group behavior only an
/// injection needs — that decision belongs to `load_layer_stack`'s
/// `require_non_empty`, and nowhere else.
pub fn merge_loaded_layers(
    loaded: &[(String, pi_domain::secretgroup::SecretGroup)],
) -> Result<pi_domain::secretgroup::MergedSecrets, pi_domain::error::DomainError> {
    use pi_domain::secretgroup::{merge_layers, Layer};

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
    let loaded = load_layer_stack(
        secrets,
        group_base(config),
        &config.name,
        &config.secret_groups,
        true,
    )
    .await?;
    merge_loaded_layers(&loaded)
}

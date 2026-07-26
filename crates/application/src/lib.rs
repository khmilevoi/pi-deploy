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

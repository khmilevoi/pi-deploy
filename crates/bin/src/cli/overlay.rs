use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::rpitoml::{CommandValue, RpiToml};
use crate::cli::vars::{self, VarRef, VarSet};

pub const RESERVED_ENV_NAMES: &[&str] = &["show", "ls", "destroy", "reset-data"];

pub fn validate_env_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'));
    if !ok {
        anyhow::bail!("environment name '{name}' must match ^[a-z][a-z0-9-]*$");
    }
    if RESERVED_ENV_NAMES.contains(&name) {
        anyhow::bail!(
            "environment name '{name}' is reserved (reserved: {})",
            RESERVED_ENV_NAMES.join(", ")
        );
    }
    Ok(())
}

const MAX_SLUG_LEN: usize = 30;

pub fn derive_slug(branch: &str) -> anyhow::Result<String> {
    let mut slug = String::new();
    for c in branch.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(MAX_SLUG_LEN);
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        anyhow::bail!(
            "cannot derive ${{env.slug}} from branch '{branch}' (no [a-z0-9] characters)"
        );
    }
    Ok(slug)
}

pub fn derive_key(base: &str, env: &str, slug: Option<&str>) -> String {
    match slug {
        Some(slug) => format!("{base}--{env}--{slug}"),
        None => format!("{base}--{env}"),
    }
}

/// Overlay file `rpi.<env>.toml`: every field optional; unknown fields are
/// errors (stricter than the base file); `schema`/`[project]` forbidden.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpiTomlOverlay {
    /// Forbidden — schema version is a property of the base file.
    schema: Option<toml::Value>,
    /// Forbidden — the project key is CLI-derived.
    project: Option<toml::Value>,
    pub source: Option<OverlaySource>,
    pub build: Option<OverlayBuild>,
    pub ingress: Option<OverlayIngress>,
    pub timeouts: Option<OverlayTimeouts>,
    pub healthcheck: Option<OverlayHealthcheck>,
    pub secrets: Option<OverlaySecrets>,
    pub commands: Option<BTreeMap<String, CommandValue>>,
    pub environment: Option<EnvironmentSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySource {
    pub repo: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayBuild {
    pub compose: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayIngress {
    pub hostname: Option<String>,
    pub service: Option<String>,
    pub port: Option<u16>,
    pub expose: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayTimeouts {
    pub fetch: Option<String>,
    pub build: Option<String>,
    pub up: Option<String>,
    pub command: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayHealthcheck {
    pub path: Option<String>,
    pub expect: Option<String>,
    pub timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySecrets {
    pub env: Option<String>,
    pub files: Option<Vec<String>>,
    pub file_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSection {
    pub ttl: Option<String>,
    pub on_create: Option<String>,
}

impl RpiTomlOverlay {
    pub fn parse(text: &str, file: &str) -> anyhow::Result<RpiTomlOverlay> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
        RpiTomlOverlay::from_value(value, file)
    }

    /// Same checks as `parse`, but starting from an already-parsed (and, in
    /// the resolver's case, already-substituted) document.
    pub fn from_value(value: toml::Value, file: &str) -> anyhow::Result<RpiTomlOverlay> {
        let parsed: RpiTomlOverlay = value
            .try_into()
            .map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
        if parsed.schema.is_some() {
            anyhow::bail!("{file}: `schema` is not allowed in an overlay (set it in rpi.toml)");
        }
        if parsed.project.is_some() {
            anyhow::bail!("{file}: [project] is not allowed in an overlay (the project key is derived by the CLI)");
        }
        Ok(parsed)
    }

    #[allow(dead_code)]
    pub fn load(path: &Path) -> anyhow::Result<RpiTomlOverlay> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        RpiTomlOverlay::parse(&text, &path.display().to_string())
    }
}

/// Path of the one field resolved in phase 1. `env.slug` derives from it, so
/// it cannot itself see the slug — that is the circular reference guarded
/// below.
const BRANCH_PATH: &str = "source.branch";

/// Walks every string leaf of a TOML document, handing each one its dotted
/// path (`ingress.hostname`, `commands.seed`, `secrets.files.0`) so errors
/// and warnings can name the field the user actually wrote.
fn walk_strings(
    value: &mut toml::Value,
    path: &mut Vec<String>,
    f: &mut impl FnMut(&str, &mut String) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match value {
        toml::Value::String(s) => f(&path.join("."), s),
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                path.push(key.clone());
                walk_strings(child, path, f)?;
                path.pop();
            }
            Ok(())
        }
        toml::Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                path.push(i.to_string());
                walk_strings(child, path, f)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Every reference in the document, paired with the path it was found at.
fn collect_refs(value: &mut toml::Value) -> anyhow::Result<Vec<(String, VarRef)>> {
    let mut found = Vec::new();
    walk_strings(value, &mut Vec::new(), &mut |path, s| {
        for r in vars::refs(path, s)? {
            found.push((path.to_string(), r));
        }
        Ok(())
    })?;
    Ok(found)
}

/// Substitutes into the fields selected by `want`, leaving the rest alone.
fn substitute_where(
    value: &mut toml::Value,
    set: &VarSet,
    want: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    walk_strings(value, &mut Vec::new(), &mut |path, s| {
        if want(path) {
            *s = vars::substitute(path, s, set)?;
        }
        Ok(())
    })
}

/// Reads a string at a dotted path without disturbing the tree.
fn string_at(value: &toml::Value, path: &str) -> Option<String> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    current.as_str().map(str::to_string)
}

/// Computes the `${git.*}` inputs the documents actually reference. Lazy on
/// purpose: `rpi config show` must keep working outside a git repository for
/// a configuration that never asks for one.
fn git_inputs(refs: &[(String, VarRef)]) -> anyhow::Result<BTreeMap<String, String>> {
    let dir = std::path::Path::new(".");
    let mut out = BTreeMap::new();
    for (_, r) in refs {
        let VarRef::Input(ns, field) = r else {
            continue;
        };
        if ns != "git" || out.contains_key(&r.name()) {
            continue;
        }
        let value = match field.as_str() {
            "branch" => crate::cli::gitctx::branch(dir)?,
            "sha" => crate::cli::gitctx::sha(dir)?,
            "short_sha" => crate::cli::gitctx::short_sha(dir)?,
            other => anyhow::bail!(
                "unknown variable 'git.{other}' (available: git.branch, git.sha, git.short_sha)"
            ),
        };
        out.insert(r.name(), value);
    }
    Ok(out)
}

/// `""` resets an optional field to `None`; any other value replaces it.
fn reset_or(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Typed schema-aware merge (spec: scalars replace, tables field-wise,
/// arrays and [commands] wholesale, "" resets optionals).
pub fn apply_overlay(base: &mut RpiToml, overlay: RpiTomlOverlay) {
    if let Some(s) = overlay.source {
        if let Some(repo) = s.repo {
            base.source.repo = repo;
        }
        if let Some(branch) = s.branch {
            base.source.branch = branch;
        }
    }
    if let Some(b) = overlay.build {
        if let Some(compose) = b.compose {
            base.build.compose = compose;
        }
    }
    if let Some(i) = overlay.ingress {
        if let Some(hostname) = i.hostname {
            base.ingress.hostname = reset_or(hostname);
        }
        if let Some(service) = i.service {
            base.ingress.service = service;
        }
        if let Some(port) = i.port {
            base.ingress.port = port;
        }
        if let Some(expose) = i.expose {
            base.ingress.expose = reset_or(expose);
        }
    }
    if let Some(t) = overlay.timeouts {
        if let Some(v) = t.fetch {
            base.timeouts.fetch = reset_or(v);
        }
        if let Some(v) = t.build {
            base.timeouts.build = reset_or(v);
        }
        if let Some(v) = t.up {
            base.timeouts.up = reset_or(v);
        }
        if let Some(v) = t.command {
            base.timeouts.command = reset_or(v);
        }
    }
    if let Some(h) = overlay.healthcheck {
        if let Some(v) = h.path {
            base.healthcheck.path = reset_or(v);
        }
        if let Some(v) = h.expect {
            base.healthcheck.expect = reset_or(v);
        }
        if let Some(v) = h.timeout {
            base.healthcheck.timeout = reset_or(v);
        }
    }
    if let Some(s) = overlay.secrets {
        if let Some(env) = s.env {
            base.secrets.env = reset_or(env);
        }
        if let Some(files) = s.files {
            base.secrets.files = files;
        }
        if let Some(mode) = s.file_mode {
            base.secrets.file_mode = reset_or(mode);
        }
    }
    if let Some(commands) = overlay.commands {
        base.commands = Some(commands);
    }
}

/// The environment an overlay resolution selected, plus everything derived
/// from it that the deploy path needs. The derived key itself lives on
/// `Resolved::rpitoml.project.name` (set by `resolve_from` before this is
/// built), so it is not duplicated here — `rpi env destroy`/`reset-data`
/// compute the key directly via `derive_key` instead of going through a
/// full overlay resolution (see `envcmds::resolve_key`).
#[derive(Debug)]
pub struct EnvSelection {
    pub env: String,
    pub base: String,
    pub slug: Option<String>,
    pub ttl_secs: Option<u64>,
    pub on_create: Option<String>,
}

/// Outcome of resolving `rpi.toml` (+ an optional overlay): the merged,
/// validated configuration and, when an environment was selected, the
/// derived environment metadata.
#[derive(Debug)]
pub struct Resolved {
    pub rpitoml: RpiToml,
    pub env: Option<EnvSelection>,
}

/// Loads `./rpi.toml` (+ `./rpi.<env>.toml` when `env` is set) and resolves
/// everything: interpolation, merge, revalidation and key/ttl derivation.
pub fn resolve(env: Option<&str>, vars: &[String]) -> anyhow::Result<Resolved> {
    let base_text = std::fs::read_to_string("rpi.toml").map_err(|e| {
        anyhow::anyhow!("cannot read rpi.toml: {e} (run from the project root, see §12)")
    })?;
    let overlay = match env {
        None => None,
        Some(name) => {
            validate_env_name(name)?;
            let file = format!("rpi.{name}.toml");
            let text = std::fs::read_to_string(&file).map_err(|e| {
                anyhow::anyhow!("cannot read {file}: {e}{}", available_overlays_hint())
            })?;
            Some((name, text))
        }
    };
    resolve_from(
        &base_text,
        overlay.as_ref().map(|(n, t)| (*n, t.as_str())),
        vars,
    )
}

/// " (found overlays: rpi.test.toml, rpi.branch.toml)" or "" — for the
/// missing-overlay-file error.
fn available_overlays_hint() -> String {
    let mut found: Vec<String> = std::fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("rpi.") && n.ends_with(".toml") && *n != "rpi.toml")
        .collect();
    found.sort();
    if found.is_empty() {
        String::new()
    } else {
        format!(" (found overlays: {})", found.join(", "))
    }
}

/// Same as `resolve`, but from explicit texts and with a caller-supplied
/// warning sink — unit-testable without touching the filesystem, and with
/// warnings captured instead of printed.
///
/// Resolution runs in two phases over the raw `toml::Value` trees of both
/// files, before either is deserialized: phase 1 resolves `source.branch`
/// alone (because `env.slug` is derived from the merged branch), phase 2
/// resolves everything else. Substituting before deserialization is what
/// lets every string field carry a reference and still be validated by the
/// normal typed checks afterwards.
pub fn resolve_with(
    base_text: &str,
    overlay: Option<(&str, &str)>,
    vars_arg: &[String],
    warn: &mut dyn FnMut(&str),
) -> anyhow::Result<Resolved> {
    let user = vars::parse_vars(vars_arg)?;
    let mut base_v: toml::Value =
        toml::from_str(base_text).map_err(|e| anyhow::anyhow!("rpi.toml: {e}"))?;

    // The project key must stay static: it drives base--env[--slug]
    // derivation and the production-key-hijack check, neither of which is
    // verifiable against a computed name.
    if let Some(name) = string_at(&base_v, "project.name") {
        if !vars::refs("[project].name", &name)?.is_empty() {
            anyhow::bail!(
                "[project].name: ${{...}} is not allowed (the project key must be static)"
            );
        }
    }

    let env_name = overlay.map(|(name, _)| name);
    if let Some(name) = env_name {
        validate_env_name(name)?;
    }
    let file = env_name
        .map(|n| format!("rpi.{n}.toml"))
        .unwrap_or_default();
    let mut overlay_v = match overlay {
        None => None,
        Some((_, text)) => {
            Some(toml::from_str::<toml::Value>(text).map_err(|e| anyhow::anyhow!("{file}: {e}"))?)
        }
    };

    // One reference sweep over both documents feeds every decision below:
    // which git inputs to compute, whether the slug is wanted, and which
    // --vars keys went unused.
    let mut all_refs = collect_refs(&mut base_v)?;
    let has_branch_ref =
        |refs: &[(String, VarRef)]| refs.iter().any(|(path, _)| path == BRANCH_PATH);
    // Whether the document that *wins* `source.branch` computes it, which is
    // what the no-slug key warning below turns on. An overlay branch replaces
    // the base one outright, so a base `${git.branch}` pinned to a literal by
    // the overlay is a static branch, not a computed one.
    let mut branch_is_computed = has_branch_ref(&all_refs);
    if let Some(o) = &mut overlay_v {
        let refs = collect_refs(o)?;
        if string_at(o, BRANCH_PATH).is_some() {
            branch_is_computed = has_branch_ref(&refs);
        }
        all_refs.extend(refs);
    }

    for key in user.keys() {
        if !all_refs
            .iter()
            .any(|(_, r)| r == &VarRef::User(key.clone()))
        {
            anyhow::bail!(
                "--vars: variable '{key}' is never referenced in rpi.toml{}",
                if file.is_empty() {
                    String::new()
                } else {
                    format!(" or {file}")
                }
            );
        }
    }

    // The whole `env.*` namespace describes a selected environment, so
    // without one there is nothing to report but "unknown variable" — which
    // the substitution pass below says on its own. Only when an environment
    // exists is `${env.slug}` in the branch field a genuine cycle.
    let slug_ref = VarRef::Input("env".into(), "slug".into());
    let wants_slug = env_name.is_some() && all_refs.iter().any(|(_, r)| r == &slug_ref);
    if env_name.is_some()
        && all_refs
            .iter()
            .any(|(path, r)| path == BRANCH_PATH && r == &slug_ref)
    {
        anyhow::bail!("{BRANCH_PATH}: circular reference - env.slug is derived from {BRANCH_PATH}");
    }

    let mut set = VarSet {
        user: user.clone(),
        inputs: git_inputs(&all_refs)?,
    };
    if let Some(name) = env_name {
        set.inputs.insert("env.name".into(), name.to_string());
    }

    // Phase 1: source.branch only.
    substitute_where(&mut base_v, &set, |p| p == BRANCH_PATH)?;
    if let Some(o) = &mut overlay_v {
        substitute_where(o, &set, |p| p == BRANCH_PATH)?;
    }

    let effective_branch = overlay_v
        .as_ref()
        .and_then(|o| string_at(o, BRANCH_PATH))
        .or_else(|| string_at(&base_v, BRANCH_PATH))
        .unwrap_or_else(crate::cli::rpitoml::default_branch);

    let slug = wants_slug
        .then(|| derive_slug(&effective_branch))
        .transpose()?;
    if let Some(slug) = &slug {
        set.inputs.insert("env.slug".into(), slug.clone());
    }

    // Phase 2: everything except the field already done.
    substitute_where(&mut base_v, &set, |p| p != BRANCH_PATH)?;
    if let Some(o) = &mut overlay_v {
        substitute_where(o, &set, |p| p != BRANCH_PATH)?;
    }

    let mut base = RpiToml::from_value(base_v)?;
    let Some(env_name) = env_name else {
        return Ok(Resolved {
            rpitoml: base,
            env: None,
        });
    };
    let mut overlay =
        RpiTomlOverlay::from_value(overlay_v.expect("env implies an overlay"), &file)?;

    // The signal is "the branch is variable but the key is not": whatever
    // computes `source.branch` — `${BRANCH_NAME}`, `${git.branch}`,
    // `${git.short_sha}` — every branch it can produce would land on the
    // same shared key. Keying this on `--vars` instead would both miss the
    // `${git.*}` case (no --vars at all) and nag about a deliberately shared
    // stand that merely parameterizes, say, `.env.${STAGE}`.
    if branch_is_computed && slug.is_none() {
        warn(&format!(
            "{file}: {BRANCH_PATH} is computed but nothing references ${{env.slug}}, so the key stays '{}--{env_name}' - every branch deploying this environment shares it",
            base.project.name
        ));
    }

    let environment = overlay.environment.take();
    let base_name = base.project.name.clone();
    let base_hostname = base.ingress.hostname.clone();
    apply_overlay(&mut base, overlay);
    let key = derive_key(&base_name, env_name, slug.as_deref());
    base.project.name = key.clone();
    base.validate_common()
        .map_err(|e| anyhow::anyhow!("{file}: merged configuration is invalid: {e}"))?;

    // Production-key protection (hostname edition): an environment that
    // silently inherits the base project's hostname would hijack its route
    // the moment it deploys successfully (ingress upserts the same hostname
    // to the environment's host port). The overlay must override or clear
    // (`hostname = ""`) a hostname the base sets.
    if let (Some(base_h), Some(merged_h)) = (&base_hostname, &base.ingress.hostname) {
        if base_h == merged_h {
            anyhow::bail!(
                "{file}: [ingress].hostname equals the base hostname '{base_h}' - an environment must override it or clear it (hostname = \"\")"
            );
        }
    }

    let ttl_secs = match environment.as_ref().and_then(|e| e.ttl.as_deref()) {
        Some(ttl) => Some(
            crate::duration::parse_duration_secs(ttl)
                .map_err(|e| anyhow::anyhow!("{file} [environment].ttl: {e}"))?,
        ),
        None => None,
    };
    let on_create = environment.and_then(|e| e.on_create);
    if let Some(cmd) = &on_create {
        let declared = base.commands.as_ref().is_some_and(|c| c.contains_key(cmd));
        if !declared {
            anyhow::bail!(
                "{file} [environment].on_create: command '{cmd}' is not declared in the merged [commands]"
            );
        }
    }

    Ok(Resolved {
        rpitoml: base,
        env: Some(EnvSelection {
            env: env_name.to_string(),
            base: base_name,
            slug,
            ttl_secs,
            on_create,
        }),
    })
}

/// Same as `resolve_with`, sending warnings to the standard output helper.
pub fn resolve_from(
    base_text: &str,
    overlay: Option<(&str, &str)>,
    vars_arg: &[String],
) -> anyhow::Result<Resolved> {
    resolve_with(base_text, overlay, vars_arg, &mut |w| {
        crate::output::warn(w)
    })
}

/// Render the resolved configuration (base + overlay merge) as TOML text,
/// appending a synthetic `[environment]` section describing the selected
/// environment when one was resolved (`rpi config show`).
pub fn render_resolved(r: &Resolved) -> anyhow::Result<String> {
    let mut text = toml::to_string_pretty(&r.rpitoml)?;
    if let Some(env) = &r.env {
        text.push_str("\n[environment]\n");
        text.push_str(&toml::to_string(&EnvironmentRender {
            env: &env.env,
            base: &env.base,
            slug: env.slug.as_deref(),
            ttl_secs: env.ttl_secs,
            on_create: env.on_create.as_deref(),
        })?);
    }
    Ok(text)
}

#[derive(Serialize)]
struct EnvironmentRender<'a> {
    env: &'a str,
    base: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_create: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
schema = 1

[project]
name = "myapp"

[source]
repo = "git@github.com:acme/myapp.git"
branch = "main"

[ingress]
hostname = "app.example.com"
service = "web"
port = 3000

[healthcheck]
path = "/health"

[secrets]
env = ".env"

[commands]
seed = "node seed.js"
"#;

    #[test]
    fn merge_replaces_scalars_field_wise() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        let o = overlay("[source]\nbranch = \"develop\"\n\n[ingress]\nhostname = \"test.example.com\"\n\n[secrets]\nenv = \".env.test\"\n");
        apply_overlay(&mut base, o);
        assert_eq!(base.source.branch, "develop");
        assert_eq!(
            base.source.repo, "git@github.com:acme/myapp.git",
            "untouched"
        );
        assert_eq!(base.ingress.hostname.as_deref(), Some("test.example.com"));
        assert_eq!(base.ingress.service, "web", "untouched");
        assert_eq!(base.secrets.env.as_deref(), Some(".env.test"));
    }

    #[test]
    fn empty_string_resets_optional_fields() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        let o = overlay(
            "[ingress]\nhostname = \"\"\n\n[secrets]\nenv = \"\"\n\n[healthcheck]\npath = \"\"\n",
        );
        apply_overlay(&mut base, o);
        assert_eq!(base.ingress.hostname, None);
        assert_eq!(base.secrets.env, None);
        assert_eq!(base.healthcheck.path, None);
    }

    #[test]
    fn commands_table_replaces_wholesale() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        let o = overlay("[commands]\nmigrate = \"npx prisma migrate deploy\"\n");
        apply_overlay(&mut base, o);
        let commands = base.commands.as_ref().unwrap();
        assert!(commands.contains_key("migrate"));
        assert!(
            !commands.contains_key("seed"),
            "base commands must be replaced, not merged"
        );
    }

    #[test]
    fn secrets_files_replace_wholesale() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(&BASE.replace(
            "env = \".env\"",
            "env = \".env\"\nfiles = [\"a.pem\", \"b.pem\"]",
        ))
        .unwrap();
        let o = overlay("[secrets]\nfiles = [\"c.pem\"]\n");
        apply_overlay(&mut base, o);
        assert_eq!(base.secrets.files, vec!["c.pem".to_string()]);
    }

    #[test]
    fn overlay_sets_and_resets_secrets_file_mode() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        apply_overlay(&mut base, overlay("[secrets]\nfile_mode = \"0640\"\n"));
        assert_eq!(base.secrets.file_mode.as_deref(), Some("0640"));
        apply_overlay(&mut base, overlay("[secrets]\nfile_mode = \"\"\n"));
        assert_eq!(base.secrets.file_mode, None);
    }

    #[test]
    fn merged_result_passes_common_validation() {
        let mut base = crate::cli::rpitoml::RpiToml::parse(BASE).unwrap();
        let o = overlay("[healthcheck]\ntimeout = \"soon\"\n");
        apply_overlay(&mut base, o);
        assert!(
            base.validate_common().is_err(),
            "bad merged duration must fail"
        );
    }

    #[test]
    fn parses_minimal_overlay() {
        let o = RpiTomlOverlay::parse(
            "[source]\nbranch = \"develop\"\n\n[environment]\nttl = \"7d\"\non_create = \"seed\"\n",
            "rpi.test.toml",
        )
        .unwrap();
        assert_eq!(
            o.source.as_ref().unwrap().branch.as_deref(),
            Some("develop")
        );
        let env = o.environment.as_ref().unwrap();
        assert_eq!(env.ttl.as_deref(), Some("7d"));
        assert_eq!(env.on_create.as_deref(), Some("seed"));
    }

    #[test]
    fn rejects_schema_and_project_in_overlay() {
        let err = RpiTomlOverlay::parse("schema = 1\n", "rpi.test.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("schema"), "got: {err}");
        let err = RpiTomlOverlay::parse("[project]\nname = \"x\"\n", "rpi.test.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("[project]"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = RpiTomlOverlay::parse("[sourc]\nbranch = \"x\"\n", "rpi.test.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sourc"), "got: {err}");
        let err = RpiTomlOverlay::parse("[ingress]\nhost = \"x\"\n", "rpi.test.toml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("host"), "got: {err}");
    }

    #[test]
    fn env_name_charset_and_reserved() {
        assert!(validate_env_name("test").is_ok());
        assert!(validate_env_name("branch-preview2").is_ok());
        for bad in ["Test", "1x", "-x", "x_y", ""] {
            assert!(validate_env_name(bad).is_err(), "{bad} must be rejected");
        }
        for reserved in ["show", "ls", "destroy", "reset-data"] {
            let err = validate_env_name(reserved).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }
    }

    #[test]
    fn slug_normalizes_truncates_and_rejects_empty() {
        assert_eq!(derive_slug("feature/login").unwrap(), "feature-login");
        assert_eq!(derive_slug("Feature//Big__X").unwrap(), "feature-big-x");
        assert_eq!(derive_slug("-x-").unwrap(), "x");
        let long = derive_slug("abcdefghij-abcdefghij-abcdefghij-tail").unwrap();
        assert!(long.len() <= 30, "got len {}: {long}", long.len());
        assert!(!long.ends_with('-'));
        let err = derive_slug("///").unwrap_err().to_string();
        assert!(err.contains("${env.slug}"), "got: {err}");
        assert!(
            !err.contains("BRANCH_NAME") && !err.contains("RPI_ENV_SLUG"),
            "the message must name only things that still exist: {err}"
        );
    }

    #[test]
    fn key_derivation() {
        assert_eq!(derive_key("myapp", "test", None), "myapp--test");
        assert_eq!(
            derive_key("myapp", "branch", Some("feature-login")),
            "myapp--branch--feature-login"
        );
    }

    fn overlay(text: &str) -> RpiTomlOverlay {
        RpiTomlOverlay::parse(text, "rpi.branch.toml").unwrap()
    }

    #[test]
    fn substitutes_in_every_field_kind_of_an_overlay() {
        let r = resolve_from(
            BASE,
            Some((
                "test",
                concat!(
                    "[source]\nbranch = \"${BRANCH_NAME}\"\n\n",
                    "[build]\ncompose = \"compose.${STAGE}.yml\"\n\n",
                    "[ingress]\nhostname = \"${STAGE}.example.com\"\nservice = \"${STAGE}\"\n\n",
                    "[secrets]\nenv = \".env.${STAGE}\"\nfiles = [\"certs/${STAGE}.pem\"]\n\n",
                    "[timeouts]\nbuild = \"${BUILD_BUDGET}\"\n\n",
                    "[commands]\nseed = \"node seed.js --env ${STAGE}\"\n",
                    "migrate = [\"npx\", \"migrate\", \"${STAGE}\"]\n",
                ),
            )),
            &[
                "BRANCH_NAME=develop".into(),
                "STAGE=test".into(),
                "BUILD_BUDGET=20m".into(),
            ],
        )
        .unwrap();
        let c = r.rpitoml;
        assert_eq!(c.source.branch, "develop");
        assert_eq!(c.build.compose, "compose.test.yml");
        assert_eq!(c.ingress.hostname.as_deref(), Some("test.example.com"));
        assert_eq!(c.ingress.service, "test");
        assert_eq!(c.secrets.env.as_deref(), Some(".env.test"));
        assert_eq!(c.secrets.files, vec!["certs/test.pem".to_string()]);
        assert_eq!(c.timeouts.build.as_deref(), Some("20m"));
        let commands = c.to_project_config().commands;
        assert_eq!(
            commands["seed"].argv,
            vec!["node", "seed.js", "--env", "test"]
        );
        assert_eq!(commands["migrate"].argv, vec!["npx", "migrate", "test"]);
    }

    #[test]
    fn substitutes_in_the_base_file_without_any_env() {
        let base = BASE.replace("branch = \"main\"", "branch = \"${BRANCH_NAME}\"");
        let r = resolve_from(&base, None, &["BRANCH_NAME=develop".into()]).unwrap();
        assert_eq!(r.rpitoml.source.branch, "develop");
        assert_eq!(r.rpitoml.project.name, "myapp", "no env, no derived key");
        assert!(r.env.is_none());
    }

    #[test]
    fn env_slug_derives_from_the_merged_branch_and_drives_the_key() {
        let r = resolve_from(
            BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"${env.slug}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--branch--feature-login");
        assert_eq!(r.rpitoml.source.branch, "feature/login");
        assert_eq!(
            r.rpitoml.ingress.hostname.as_deref(),
            Some("feature-login.preview.example.com")
        );
        assert_eq!(r.env.unwrap().slug.as_deref(), Some("feature-login"));
    }

    #[test]
    fn a_variable_that_is_not_the_slug_does_not_add_a_slug_suffix() {
        // The old rule granted the suffix for using ANY variable, which turned
        // a shared stand into a per-branch environment demanding a branch name.
        let r = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[secrets]\nenv = \".env.${STAGE}\"\n",
            )),
            &["STAGE=qa".into()],
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--test");
        assert_eq!(r.env.unwrap().slug, None);
    }

    #[test]
    fn env_name_is_available_as_an_input() {
        let r = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"${env.name}.example.com\"\n\n[secrets]\nenv = \".env.${env.name}\"\n",
            )),
            &[],
        )
        .unwrap();
        assert_eq!(
            r.rpitoml.ingress.hostname.as_deref(),
            Some("test.example.com")
        );
        assert_eq!(r.rpitoml.secrets.env.as_deref(), Some(".env.test"));
    }

    #[test]
    fn env_inputs_are_unavailable_without_an_env() {
        for text in ["${env.name}", "${env.slug}"] {
            let base = BASE.replace("branch = \"main\"", &format!("branch = \"{text}\""));
            let err = resolve_from(&base, None, &[]).unwrap_err().to_string();
            assert!(err.contains("unknown variable"), "{text}: {err}");
        }
    }

    #[test]
    fn escaped_reference_survives_into_a_command() {
        let base = BASE.replace(
            "seed = \"node seed.js\"",
            "seed = \"sh -c 'tar -C $${HOME} .'\"",
        );
        let r = resolve_from(&base, None, &[]).unwrap();
        assert_eq!(
            r.rpitoml.to_project_config().commands["seed"].argv,
            vec!["sh", "-c", "tar -C ${HOME} ."]
        );
    }

    #[test]
    fn unreferenced_vars_are_rejected_by_name() {
        let err = resolve_from(
            BASE,
            Some(("test", "[ingress]\nhostname = \"test.example.com\"\n")),
            &["TYPO=1".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("TYPO"), "got: {err}");
        assert!(err.contains("never referenced"), "got: {err}");
    }

    #[test]
    fn interpolation_in_the_project_name_is_rejected() {
        let base = BASE.replace("name = \"myapp\"", "name = \"app-${STAGE}\"");
        let err = resolve_from(&base, None, &["STAGE=qa".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("[project].name"), "got: {err}");
    }

    #[test]
    fn env_slug_inside_source_branch_is_a_circular_reference() {
        let err = resolve_from(
            BASE,
            Some(("branch", "[source]\nbranch = \"${env.slug}\"\n")),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("circular"), "got: {err}");
    }

    #[test]
    fn rpi_variables_are_rejected_with_the_runtime_hint() {
        let err = resolve_from(
            BASE,
            Some((
                "branch",
                "[ingress]\nhostname = \"${RPI_ENV_SLUG}.preview.example.com\"\n",
            )),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(err.contains("${env.slug}"), "got: {err}");
    }

    #[test]
    fn parameterized_env_without_a_slug_reference_warns_about_the_key() {
        let mut warnings = Vec::new();
        let r = resolve_with(
            BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"fixed.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--branch");
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(warnings[0].contains("${env.slug}"), "got: {warnings:?}");
    }

    #[test]
    fn a_git_derived_branch_without_a_slug_reference_also_warns() {
        // No --vars at all, yet every branch resolves to a different
        // source.branch and they would all share the key 'myapp--branch'.
        // Reads ${git.short_sha} from the ambient repository, which is
        // readable even on the detached HEAD a CI checkout leaves behind.
        let mut warnings = Vec::new();
        let r = resolve_with(
            BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${git.short_sha}\"\n\n[ingress]\nhostname = \"fixed.example.com\"\n",
            )),
            &[],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--branch");
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(warnings[0].contains("${env.slug}"), "got: {warnings:?}");
    }

    #[test]
    fn a_static_env_with_no_vars_never_warns() {
        let mut warnings = Vec::new();
        resolve_with(
            BASE,
            Some(("test", "[ingress]\nhostname = \"test.example.com\"\n")),
            &[],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert!(warnings.is_empty(), "got: {warnings:?}");
    }

    #[test]
    fn an_overlay_pinning_a_static_branch_over_a_computed_base_never_warns() {
        // The overlay's source.branch replaces the base's outright, so the
        // branch this environment deploys is the literal "main" - shared on
        // purpose, with nothing to warn about, even though the base file
        // computes its own branch for the production deploy.
        let base = BASE.replace("branch = \"main\"", "branch = \"${BRANCH_NAME}\"");
        let mut warnings = Vec::new();
        let r = resolve_with(
            &base,
            Some((
                "test",
                "[source]\nbranch = \"main\"\n\n[ingress]\nhostname = \"test.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert_eq!(r.rpitoml.source.branch, "main");
        assert!(warnings.is_empty(), "got: {warnings:?}");
    }

    #[test]
    fn a_shared_stand_parameterizing_something_else_never_warns() {
        // A fixed hostname plus `.env.${STAGE}` is a deliberately shared
        // stand (see a_variable_that_is_not_the_slug_does_not_add_a_slug_
        // suffix). Its branch is static, so the key it gets is the key it
        // wants and there is nothing to warn about.
        let mut warnings = Vec::new();
        let r = resolve_with(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[secrets]\nenv = \".env.${STAGE}\"\n",
            )),
            &["STAGE=qa".into()],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--test");
        assert!(warnings.is_empty(), "got: {warnings:?}");
    }

    #[test]
    fn substituted_values_are_revalidated() {
        // A raw branch name substituted into a hostname is invalid DNS - the
        // slash in "feature/login" makes an invalid label. The check runs
        // post-substitution, which is why ${env.slug} (the sanitized form)
        // exists.
        let err = resolve_from(
            BASE,
            Some((
                "branch",
                "[ingress]\nhostname = \"${BRANCH_NAME}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hostname"), "got: {err}");
    }

    #[test]
    fn resolve_named_env_derives_key_and_ttl() {
        let r = resolve_from(
            BASE,
            Some((
                "test",
                "[source]\nbranch = \"develop\"\n\n[ingress]\nhostname = \"test.example.com\"\n\n[environment]\nttl = \"7d\"\non_create = \"seed\"\n",
            )),
            &[],
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--test");
        let env = r.env.unwrap();
        assert_eq!(env.base, "myapp");
        assert_eq!(env.slug, None);
        assert_eq!(env.ttl_secs, Some(7 * 24 * 3600));
        assert_eq!(env.on_create.as_deref(), Some("seed"));
    }

    #[test]
    fn on_create_must_reference_a_merged_command() {
        let err = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[environment]\non_create = \"nope\"\n",
            )),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nope"), "got: {err}");
        // [commands] replaced wholesale without the referenced command -> error too
        let err = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[commands]\nother = \"run x\"\n\n[environment]\non_create = \"seed\"\n",
            )),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("seed"), "got: {err}");
    }

    #[test]
    fn merged_config_is_revalidated() {
        let err = resolve_from(
            BASE,
            Some(("test", "[ingress]\nexpose = \"public\"\n")),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("expose"), "got: {err}");
    }

    // BASE's [ingress].hostname is "app.example.com" (see the BASE const above).

    #[test]
    fn overlay_inheriting_base_hostname_is_rejected() {
        // Minimal overlay with no [ingress] at all -> the merged hostname is
        // still the base's, which would hijack the production route.
        let err = resolve_from(
            BASE,
            Some(("test", "[source]\nbranch = \"develop\"\n")),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hostname"), "got: {err}");
        assert!(err.contains("app.example.com"), "got: {err}");
    }

    #[test]
    fn overlay_explicitly_repeating_base_hostname_is_also_rejected() {
        let err = resolve_from(
            BASE,
            Some(("test", "[ingress]\nhostname = \"app.example.com\"\n")),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hostname"), "got: {err}");
    }

    #[test]
    fn overlay_overriding_hostname_is_ok() {
        let r = resolve_from(
            BASE,
            Some(("test", "[ingress]\nhostname = \"test.example.com\"\n")),
            &[],
        )
        .unwrap();
        assert_eq!(
            r.rpitoml.ingress.hostname.as_deref(),
            Some("test.example.com")
        );
    }

    #[test]
    fn overlay_clearing_hostname_is_ok() {
        let r = resolve_from(BASE, Some(("test", "[ingress]\nhostname = \"\"\n")), &[]).unwrap();
        assert_eq!(r.rpitoml.ingress.hostname, None);
    }

    #[test]
    fn overlay_is_ok_when_base_has_no_hostname() {
        let base_no_hostname = BASE.replace("hostname = \"app.example.com\"\n", "");
        let r = resolve_from(
            &base_no_hostname,
            Some(("test", "[source]\nbranch = \"develop\"\n")),
            &[],
        )
        .unwrap();
        assert_eq!(r.rpitoml.ingress.hostname, None);
    }

    #[test]
    fn bad_ttl_is_rejected() {
        let err = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[environment]\nttl = \"soon\"\n",
            )),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ttl"), "got: {err}");
    }

    #[test]
    fn render_resolved_prints_toml_with_key_and_environment() {
        let r = resolve_from(
            BASE,
            Some(("test", "[source]\nbranch = \"develop\"\n\n[ingress]\nhostname = \"test.example.com\"\n\n[environment]\nttl = \"7d\"\non_create = \"seed\"\n")),
            &[],
        )
        .unwrap();
        let text = render_resolved(&r).unwrap();
        assert!(text.contains("name = \"myapp--test\""), "got:\n{text}");
        assert!(text.contains("branch = \"develop\""));
        assert!(text.contains("[environment]"));
        assert!(text.contains("ttl_secs = 604800"));
        assert!(text.contains("on_create = \"seed\""));
        // resolved output must round-trip as valid TOML
        toml::from_str::<toml::Value>(&text).unwrap();
    }

    #[test]
    fn render_resolved_escapes_control_characters_as_valid_toml() {
        // Verify that EnvironmentRender serialization produces valid TOML
        // even when fields contain control characters.
        let render = EnvironmentRender {
            env: "test",
            base: "my\u{0007}app", // bell character (control char)
            slug: None,
            ttl_secs: None,
            on_create: None,
        };
        let text = toml::to_string(&render).unwrap();
        // The serialized output must parse as valid TOML
        let parsed = toml::from_str::<toml::Value>(&text).unwrap();
        // Verify the control character was preserved in round-trip
        assert_eq!(parsed["base"].as_str().unwrap(), "my\u{0007}app");
    }
}

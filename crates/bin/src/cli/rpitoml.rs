use std::collections::BTreeMap;

use crate::duration::parse_duration_secs;
use pi_domain::entities::{
    CommandSpec, ExposeMode, HealthcheckConfig, ProjectConfig, StageTimeoutOverrides,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct RpiToml {
    pub schema: u32,
    pub project: ProjectSection,
    pub source: SourceSection,
    #[serde(default)]
    pub build: BuildSection,
    pub ingress: IngressSection,
    #[serde(default)]
    pub timeouts: TimeoutsSection,
    #[serde(default)]
    pub healthcheck: HealthcheckSection,
    #[serde(default)]
    pub secrets: SecretsSection,
    /// Legacy [env] table: rejected in parse() with a migration hint. Detected
    /// via Option<toml::Value> because serde tolerates unknown sections.
    #[serde(default, rename = "env", skip_serializing)]
    legacy_env: Option<toml::Value>,
    /// [environment] is only valid in overlay files; detected here so the
    /// base file can reject it with a clear error.
    #[serde(default, rename = "environment", skip_serializing)]
    environment_section: Option<toml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<BTreeMap<String, CommandValue>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ProjectSection {
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SourceSection {
    pub repo: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

/// Also the resolver's fallback when neither the base file nor the overlay
/// spells out `[source].branch` but `${env.slug}` still has to be derived
/// from it.
pub(crate) fn default_branch() -> String {
    "main".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BuildSection {
    #[serde(default = "default_compose")]
    pub compose: String,
}

impl Default for BuildSection {
    fn default() -> BuildSection {
        BuildSection {
            compose: default_compose(),
        }
    }
}

fn default_compose() -> String {
    "docker-compose.yml".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IngressSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    pub service: String,
    pub port: u16,
    /// `expose = "lan"` binds `0.0.0.0` (all interfaces), not just the LAN.
    /// On a host with a public IPv4 this publishes the service to the public
    /// internet. Docker also bypasses host firewalls (UFW/iptables) for
    /// published ports via its own `DOCKER` chain, so firewall rules will not
    /// block it. Use `"lan"` only on trusted networks or behind an external
    /// firewall/router. Defaults to `"private"` (127.0.0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose: Option<String>,
}

/// [timeouts] in rpi.toml — per-project stage overrides (§12, §8.1).
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TimeoutsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up: Option<String>,
    /// Budget for one `rpi command` run on the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct HealthcheckSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// "2xx" | "3xx" | exact code like "204".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// "60s" | "2m" | bare seconds. Default 60s (§22).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// [secrets] in rpi.toml (secrets spec §3): what `rpi secrets send` reads.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SecretsSection {
    /// Local env file. None -> default ".env" (missing file is fine then);
    /// Some(path) -> the file must exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Secret files, relative forward-slash paths (recreated verbatim on the Pi).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Mode for the materialized secrets, e.g. "0640". None -> 0644 for
    /// `[secrets].files` and 0600 for the injected `.env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<String>,
    /// Declared secret groups, applied in this order at deploy time before the
    /// deploy key's own bundle (secret-groups spec: Attachment and layering).
    /// An explicit empty list detaches every group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

/// [commands] value: a shell-word string, an explicit argv array, or a table
/// with `run` (string/array) plus an optional `service` (§spec).
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CommandValue {
    Line(String),
    Argv(Vec<String>),
    Table {
        run: CommandRun,
        #[serde(default)]
        service: Option<String>,
    },
}

/// The `run` of a table-form command: same two shapes as the shorthand value.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CommandRun {
    Line(String),
    Argv(Vec<String>),
}

fn is_valid_command_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn argv_from_run(name: &str, line_or_argv: RunShape) -> anyhow::Result<Vec<String>> {
    let argv = match line_or_argv {
        RunShape::Argv(items) => items,
        RunShape::Line(line) => shlex::split(&line)
            .ok_or_else(|| anyhow::anyhow!("rpi.toml [commands].{name}: unbalanced quote"))?,
    };
    if argv.is_empty() || argv.iter().any(|a| a.is_empty()) {
        anyhow::bail!("rpi.toml [commands].{name}: command must not be empty");
    }
    Ok(argv)
}

/// Internal shape passed to `argv_from_run` from either the shorthand value or a
/// table's `run`.
enum RunShape {
    Line(String),
    Argv(Vec<String>),
}

/// String form is split client-side with shell-word rules (quotes only —
/// no variables, pipes or redirects; a shell must be spelled out as
/// `sh -c '...'`). Array form is taken verbatim.
fn command_spec(name: &str, value: &CommandValue) -> anyhow::Result<CommandSpec> {
    let (run, service) = match value {
        CommandValue::Line(line) => (RunShape::Line(line.clone()), None),
        CommandValue::Argv(items) => (RunShape::Argv(items.clone()), None),
        CommandValue::Table { run, service } => {
            let run = match run {
                CommandRun::Line(line) => RunShape::Line(line.clone()),
                CommandRun::Argv(items) => RunShape::Argv(items.clone()),
            };
            (run, service.clone())
        }
    };
    if let Some(service) = &service {
        if service.is_empty() {
            anyhow::bail!("rpi.toml [commands].{name}: service must not be empty");
        }
    }
    let argv = argv_from_run(name, run)?;
    Ok(CommandSpec { argv, service })
}

/// RFC-1123-style hostname check: total length, per-label length/charset,
/// no leading/trailing '-' per label. Runs post-substitution (base file and
/// merged overlay result alike), so a parameterized `${BRANCH_NAME}` value
/// containing `/` or other illegal characters is caught here rather than
/// silently producing an unroutable ingress rule.
fn validate_hostname(hostname: &str) -> Result<(), String> {
    if hostname.is_empty() || hostname.len() > 253 {
        return Err(format!(
            "invalid [ingress].hostname '{hostname}' (not a valid DNS hostname)"
        ));
    }
    for label in hostname.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok {
            return Err(format!(
                "invalid [ingress].hostname '{hostname}' (not a valid DNS hostname)"
            ));
        }
    }
    Ok(())
}

fn validate_expect(expect: &str) -> Result<(), String> {
    let ok = matches!(expect, "2xx" | "3xx")
        || (expect.len() == 3 && expect.chars().all(|c| c.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid [healthcheck].expect '{expect}' (use \"2xx\", \"3xx\" or a code like \"204\")"
        ))
    }
}

impl RpiToml {
    /// The text entry point, kept as the counterpart of `from_value` and as
    /// the shape the rpi.toml tests exercise. The resolver deliberately does
    /// not use it: it must substitute `${...}` into the raw value tree before
    /// anything is deserialized, so it parses the text itself and hands the
    /// substituted tree to `from_value`. Hence no production caller today —
    /// the same reason `RpiTomlOverlay::load` carries this allow.
    #[allow(dead_code)]
    pub fn parse(text: &str) -> anyhow::Result<RpiToml> {
        RpiToml::from_value(toml::from_str(text)?)
    }

    /// Same checks as `parse`, but starting from an already-parsed (and,
    /// in the resolver's case, already-substituted) document.
    pub fn from_value(value: toml::Value) -> anyhow::Result<RpiToml> {
        let parsed: RpiToml = value.try_into()?;
        if parsed.schema != 1 {
            anyhow::bail!(
                "unsupported rpi.toml schema {} (this rpi supports schema 1)",
                parsed.schema
            );
        }
        if parsed.project.name.contains("--") {
            anyhow::bail!(
                "rpi.toml [project].name '{}' must not contain '--' (reserved for environment keys; rename the project)",
                parsed.project.name
            );
        }
        if parsed.environment_section.is_some() {
            anyhow::bail!(
                "rpi.toml: [environment] is only allowed in overlay files (rpi.<env>.toml)"
            );
        }
        parsed.validate_common()?;
        Ok(parsed)
    }

    /// Validation shared by the base file (`parse`) and a merged overlay
    /// result: duration formats, `[healthcheck].expect`, `[ingress].expose`,
    /// legacy `[env]` rejection, secret-path checks and `[commands]` checks.
    /// Excludes the schema check and the `--` project-name ban, which only
    /// apply to the base file before a merge.
    pub fn validate_common(&self) -> anyhow::Result<()> {
        if let Some(timeout) = &self.healthcheck.timeout {
            parse_duration_secs(timeout)
                .map_err(|e| anyhow::anyhow!("rpi.toml [healthcheck]: {e}"))?;
        }
        for (field, value) in [
            ("fetch", &self.timeouts.fetch),
            ("build", &self.timeouts.build),
            ("up", &self.timeouts.up),
            ("command", &self.timeouts.command),
        ] {
            if let Some(timeout) = value {
                parse_duration_secs(timeout)
                    .map_err(|e| anyhow::anyhow!("rpi.toml [timeouts].{field}: {e}"))?;
            }
        }
        if let Some(expect) = &self.healthcheck.expect {
            validate_expect(expect).map_err(|e| anyhow::anyhow!("rpi.toml [healthcheck]: {e}"))?;
        }
        if let Some(hostname) = &self.ingress.hostname {
            validate_hostname(hostname).map_err(|e| anyhow::anyhow!(e))?;
        }
        if let Some(expose) = &self.ingress.expose {
            if ExposeMode::parse(expose).is_none() {
                anyhow::bail!(
                    "invalid rpi.toml [ingress].expose '{expose}' (use \"private\" or \"lan\")"
                );
            }
        }
        if self.legacy_env.is_some() {
            anyhow::bail!(
                "rpi.toml: [env] was replaced by [secrets]; move `file = \"...\"` to:\n[secrets]\nenv = \"...\""
            );
        }
        if let Some(env) = &self.secrets.env {
            pi_infrastructure::secretpath::validate_rel_path(env)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].env: '{env}': {e}"))?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for path in &self.secrets.files {
            pi_infrastructure::secretpath::validate_rel_path(path)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].files: '{path}': {e}"))?;
            if !seen.insert(path.as_str()) {
                anyhow::bail!("rpi.toml [secrets].files: duplicate path '{path}'");
            }
        }
        if let Some(mode) = &self.secrets.file_mode {
            pi_domain::secretmode::parse(mode)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].file_mode: {e}"))?;
        }
        let mut seen_groups = std::collections::BTreeSet::new();
        for name in &self.secrets.groups {
            pi_domain::secretgroup::validate_group_name(name)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].groups: {e}"))?;
            if !seen_groups.insert(name) {
                anyhow::bail!("rpi.toml [secrets].groups: duplicate group '{name}'");
            }
        }
        if let Some(commands) = &self.commands {
            if commands.is_empty() {
                anyhow::bail!(
                    "rpi.toml [commands] is empty - declare a command or remove the section"
                );
            }
            for (name, value) in commands {
                if !is_valid_command_name(name) {
                    anyhow::bail!(
                        "rpi.toml [commands]: command name '{name}' must match ^[a-z0-9][a-z0-9_-]*$"
                    );
                }
                command_spec(name, value)?;
            }
        }
        Ok(())
    }

    pub fn to_project_config(&self) -> ProjectConfig {
        ProjectConfig {
            name: self.project.name.clone(),
            repo: self.source.repo.clone(),
            branch: self.source.branch.clone(),
            compose_path: self.build.compose.clone(),
            service: self.ingress.service.clone(),
            container_port: self.ingress.port,
            hostname: self.ingress.hostname.clone(),
            expose: self
                .ingress
                .expose
                .as_deref()
                .and_then(ExposeMode::parse)
                .unwrap_or_default(),
            healthcheck: HealthcheckConfig {
                path: self.healthcheck.path.clone(),
                expect: self.healthcheck.expect.clone(),
                timeout_secs: self
                    .healthcheck
                    .timeout
                    .as_deref()
                    .and_then(|t| parse_duration_secs(t).ok())
                    .unwrap_or(60),
            },
            timeouts: StageTimeoutOverrides {
                fetch_secs: self
                    .timeouts
                    .fetch
                    .as_deref()
                    .and_then(|t| parse_duration_secs(t).ok()),
                build_secs: self
                    .timeouts
                    .build
                    .as_deref()
                    .and_then(|t| parse_duration_secs(t).ok()),
                up_secs: self
                    .timeouts
                    .up
                    .as_deref()
                    .and_then(|t| parse_duration_secs(t).ok()),
            },
            commands: self
                .commands
                .as_ref()
                .map(|map| {
                    map.iter()
                        .map(|(name, value)| {
                            let spec =
                                command_spec(name, value).expect("validated in RpiToml::parse");
                            (name.clone(), spec)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            command_timeout_secs: self
                .timeouts
                .command
                .as_deref()
                .and_then(|t| parse_duration_secs(t).ok()),
            // The deploy path fills this from the resolved `EnvSelection`
            // (later task); rpi.toml alone never carries an environment.
            environment: None,
            // Declared order is layer order (secret-groups spec: Attachment
            // and layering) — carried through verbatim, never re-sorted.
            secret_groups: self.secrets.groups.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema = 1

[project]
name = "rateme"

[source]
repo = "git@github.com:isskelo/rateme.git"
branch = "main"

[build]
compose = "docker-compose.yml"

[ingress]
hostname = "rateme.isskelo.com"
service = "web"
port = 3000

[healthcheck]
path = "/"

[secrets]
env = ".env"
files = ["certs/server.pem"]
"#;

    #[test]
    fn parses_spec_sample_and_tolerates_future_sections() {
        let parsed = RpiToml::parse(SAMPLE).unwrap();
        let config = parsed.to_project_config();
        assert_eq!(config.name, "rateme");
        assert_eq!(config.repo, "git@github.com:isskelo/rateme.git");
        assert_eq!(config.branch, "main");
        assert_eq!(config.compose_path, "docker-compose.yml");
        assert_eq!(config.service, "web");
        assert_eq!(config.container_port, 3000);
        assert_eq!(config.hostname.as_deref(), Some("rateme.isskelo.com"));
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let toml = SAMPLE.replace("schema = 1", "schema = 2");
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("schema"), "got: {err}");
    }

    #[test]
    fn env_and_healthcheck_sections_are_parsed_with_defaults() {
        let parsed = RpiToml::parse(SAMPLE).unwrap();
        assert_eq!(parsed.secrets.env.as_deref(), Some(".env"));
        assert_eq!(parsed.secrets.files, vec!["certs/server.pem".to_string()]);
        let config = parsed.to_project_config();
        assert_eq!(config.healthcheck.path.as_deref(), Some("/"));
        assert_eq!(config.healthcheck.expect, None);
        assert_eq!(config.healthcheck.timeout_secs, 60, "default budget");
    }

    #[test]
    fn missing_env_and_healthcheck_sections_fall_back_to_defaults() {
        let toml = SAMPLE.replace("[healthcheck]\npath = \"/\"\n", "").replace(
            "[secrets]\nenv = \".env\"\nfiles = [\"certs/server.pem\"]\n",
            "",
        );
        let parsed = RpiToml::parse(&toml).unwrap();
        assert!(parsed.secrets.env.is_none());
        assert!(parsed.secrets.files.is_empty());
        let config = parsed.to_project_config();
        assert_eq!(config.healthcheck.path, None, "no path -> TCP probe");
        assert_eq!(config.healthcheck.timeout_secs, 60);
    }

    #[test]
    fn legacy_env_section_is_a_hard_error_with_migration_hint() {
        let toml = SAMPLE.replace(
            "[secrets]\nenv = \".env\"\nfiles = [\"certs/server.pem\"]\n",
            "[env]\nfile = \".env\"\n",
        );
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(
            err.contains("[env] was replaced by [secrets]"),
            "got: {err}"
        );
    }

    #[test]
    fn secrets_files_paths_are_validated() {
        for bad in ["../escape", "/abs", r"win\path", "a//b"] {
            let toml = SAMPLE.replace(
                "files = [\"certs/server.pem\"]",
                &format!("files = [\"{}\"]", bad.replace('\\', "\\\\")),
            );
            let err = RpiToml::parse(&toml).unwrap_err().to_string();
            assert!(err.contains("[secrets].files"), "{bad}: {err}");
        }
    }

    #[test]
    fn secrets_env_path_is_validated() {
        for bad in ["../escape", "/abs", r"win\path"] {
            let toml = SAMPLE.replace(
                "env = \".env\"",
                &format!("env = \"{}\"", bad.replace('\\', "\\\\")),
            );
            let err = RpiToml::parse(&toml).unwrap_err().to_string();
            assert!(err.contains("[secrets].env"), "{bad}: {err}");
        }
    }

    #[test]
    fn duplicate_secrets_files_are_rejected() {
        let toml = SAMPLE.replace(
            "files = [\"certs/server.pem\"]",
            "files = [\"a.pem\", \"a.pem\"]",
        );
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

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

    #[test]
    fn healthcheck_timeout_and_expect_are_validated() {
        let toml = SAMPLE.replace(
            "path = \"/\"",
            "path = \"/\"\ntimeout = \"2m\"\nexpect = \"204\"",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(config.healthcheck.timeout_secs, 120);
        assert_eq!(config.healthcheck.expect.as_deref(), Some("204"));

        let bad = SAMPLE.replace("path = \"/\"", "path = \"/\"\ntimeout = \"soon\"");
        assert!(RpiToml::parse(&bad).is_err());
        let bad = SAMPLE.replace("path = \"/\"", "path = \"/\"\nexpect = \"ok\"");
        assert!(RpiToml::parse(&bad).is_err());
    }

    #[test]
    fn timeouts_section_maps_to_overrides_and_is_validated() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[timeouts]\nfetch = \"3m\"\nup = \"120s\"\n\n[healthcheck]",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(config.timeouts.fetch_secs, Some(180));
        assert_eq!(config.timeouts.build_secs, None, "not set -> agent default");
        assert_eq!(config.timeouts.up_secs, Some(120));

        let bad = SAMPLE.replace(
            "[healthcheck]",
            "[timeouts]\nbuild = \"soon\"\n\n[healthcheck]",
        );
        assert!(RpiToml::parse(&bad).is_err());
    }

    #[test]
    fn missing_timeouts_section_means_no_overrides() {
        let config = RpiToml::parse(SAMPLE).unwrap().to_project_config();
        assert_eq!(config.timeouts, Default::default());
    }

    #[test]
    fn expose_defaults_private_and_parses_lan() {
        let default_cfg = RpiToml::parse(SAMPLE).unwrap().to_project_config();
        assert_eq!(default_cfg.expose, pi_domain::entities::ExposeMode::Private);

        let lan = SAMPLE.replace("port = 3000", "port = 3000\nexpose = \"lan\"");
        let lan_cfg = RpiToml::parse(&lan).unwrap().to_project_config();
        assert_eq!(lan_cfg.expose, pi_domain::entities::ExposeMode::Lan);
    }

    #[test]
    fn invalid_hostname_is_rejected() {
        for bad in ["feature/login.example.com", "-bad.example.com"] {
            let toml = SAMPLE.replace(
                "hostname = \"rateme.isskelo.com\"",
                &format!("hostname = \"{bad}\""),
            );
            let err = RpiToml::parse(&toml).unwrap_err().to_string();
            assert!(err.contains("hostname"), "{bad}: got: {err}");
        }
    }

    #[test]
    fn invalid_expose_is_rejected() {
        let bad = SAMPLE.replace("port = 3000", "port = 3000\nexpose = \"public\"");
        let err = RpiToml::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("expose"), "got: {err}");
    }

    #[test]
    fn commands_section_parses_string_and_array_forms() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[commands]\ncreate-invite = \"node scripts/create-invite.js --admin\"\nmigrate = [\"npx\", \"prisma\", \"migrate\", \"deploy\"]\nbackup = \"sh -c 'pg_dump mydb | gzip > /b.gz'\"\n\n[healthcheck]",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(
            config.commands.get("create-invite").unwrap().argv,
            vec![
                "node".to_string(),
                "scripts/create-invite.js".into(),
                "--admin".into()
            ]
        );
        assert_eq!(
            config.commands.get("migrate").unwrap().argv,
            vec![
                "npx".to_string(),
                "prisma".into(),
                "migrate".into(),
                "deploy".into()
            ]
        );
        assert_eq!(
            config.commands.get("backup").unwrap().argv,
            vec![
                "sh".to_string(),
                "-c".into(),
                "pg_dump mydb | gzip > /b.gz".into()
            ],
            "quoted segment must stay one argv item"
        );
    }

    #[test]
    fn commands_table_form_pins_service() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[commands.create-invite]\nrun = \"node create-invite.cjs\"\nservice = \"server\"\n\n[commands.seed]\nrun = [\"node\", \"seed.js\"]\n\n[healthcheck]",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        let invite = config.commands.get("create-invite").unwrap();
        assert_eq!(
            invite.argv,
            vec!["node".to_string(), "create-invite.cjs".into()]
        );
        assert_eq!(invite.service.as_deref(), Some("server"));
        let seed = config.commands.get("seed").unwrap();
        assert_eq!(seed.argv, vec!["node".to_string(), "seed.js".into()]);
        assert_eq!(seed.service, None, "table form without service => None");
    }

    #[test]
    fn shorthand_forms_have_no_service() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[commands]\nmigrate = \"npx prisma migrate deploy\"\n\n[healthcheck]",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(config.commands.get("migrate").unwrap().service, None);
    }

    #[test]
    fn empty_service_string_is_rejected() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[commands.x]\nrun = \"node x.js\"\nservice = \"\"\n\n[healthcheck]",
        );
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("service"), "got: {err}");
    }

    #[test]
    fn missing_commands_section_means_no_commands() {
        let config = RpiToml::parse(SAMPLE).unwrap().to_project_config();
        assert!(config.commands.is_empty());
        assert_eq!(config.command_timeout_secs, None);
    }

    #[test]
    fn empty_commands_section_is_rejected() {
        let toml = SAMPLE.replace("[healthcheck]", "[commands]\n\n[healthcheck]");
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("[commands]"), "got: {err}");
    }

    #[test]
    fn invalid_command_name_is_rejected() {
        for bad in [
            "\"Bad Name\" = \"run\"",
            "\"-x\" = \"run\"",
            "\"UP\" = \"run\"",
        ] {
            let toml = SAMPLE.replace(
                "[healthcheck]",
                &format!("[commands]\n{bad}\n\n[healthcheck]"),
            );
            let err = RpiToml::parse(&toml).unwrap_err().to_string();
            assert!(err.contains("command name"), "{bad}: got: {err}");
        }
    }

    #[test]
    fn empty_command_values_are_rejected() {
        for bad in ["x = \"\"", "x = []", "x = [\"\"]"] {
            let toml = SAMPLE.replace(
                "[healthcheck]",
                &format!("[commands]\n{bad}\n\n[healthcheck]"),
            );
            assert!(RpiToml::parse(&toml).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn unbalanced_quotes_in_command_are_rejected() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[commands]\nx = \"sh -c 'oops\"\n\n[healthcheck]",
        );
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("quote"), "got: {err}");
    }

    #[test]
    fn base_name_with_double_dash_is_rejected() {
        let toml = SAMPLE.replace("name = \"rateme\"", "name = \"rate--me\"");
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("--"), "got: {err}");
    }

    #[test]
    fn environment_section_in_base_is_rejected() {
        let toml = format!("{SAMPLE}\n[environment]\nttl = \"7d\"\n");
        let err = RpiToml::parse(&toml).unwrap_err().to_string();
        assert!(err.contains("[environment]"), "got: {err}");
    }

    #[test]
    fn command_timeout_is_parsed_and_validated() {
        let toml = SAMPLE.replace(
            "[healthcheck]",
            "[timeouts]\ncommand = \"30m\"\n\n[healthcheck]",
        );
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(config.command_timeout_secs, Some(1800));

        let bad = SAMPLE.replace(
            "[healthcheck]",
            "[timeouts]\ncommand = \"soon\"\n\n[healthcheck]",
        );
        assert!(RpiToml::parse(&bad).is_err());
    }

    #[test]
    fn secrets_groups_are_parsed_and_default_to_empty() {
        let parsed = RpiToml::parse(
            &SAMPLE.replace("[secrets]", "[secrets]\ngroups = [\"common\", \"preview\"]"),
        )
        .unwrap();
        assert_eq!(
            parsed.secrets.groups,
            vec!["common".to_string(), "preview".to_string()]
        );
        assert!(RpiToml::parse(SAMPLE).unwrap().secrets.groups.is_empty());
    }

    #[test]
    fn to_project_config_carries_secret_groups_in_declared_order() {
        let toml = SAMPLE.replace("[secrets]", "[secrets]\ngroups = [\"common\", \"preview\"]");
        let config = RpiToml::parse(&toml).unwrap().to_project_config();
        assert_eq!(
            config.secret_groups,
            vec!["common".to_string(), "preview".to_string()]
        );

        let config = RpiToml::parse(SAMPLE).unwrap().to_project_config();
        assert!(config.secret_groups.is_empty());
    }

    #[test]
    fn invalid_group_names_are_rejected_at_parse_time() {
        for bad in ["Preview", "a/b", "1st", "under_score", ""] {
            let err = RpiToml::parse(
                &SAMPLE.replace("[secrets]", &format!("[secrets]\ngroups = [\"{bad}\"]")),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("[secrets].groups"), "{bad}: {err}");
        }
    }

    #[test]
    fn duplicate_groups_are_rejected() {
        let err = RpiToml::parse(
            &SAMPLE.replace("[secrets]", "[secrets]\ngroups = [\"common\", \"common\"]"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }
}

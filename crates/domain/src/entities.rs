use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::de::{Deserializer, Error as _};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// Project secrets: env vars + secret files (secrets spec 2026-07-07).
/// Values and file contents never leave the agent unmasked.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretsBundle {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> raw file bytes.
    pub files: BTreeMap<String, Vec<u8>>,
    /// `[secrets].file_mode`, when the project set one. `None` means the
    /// defaults in `crate::secretmode` apply.
    pub file_mode: Option<u32>,
}

impl SecretsBundle {
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.files.is_empty()
    }

    /// Env key names only (sorted, BTreeMap order) — `rpi secrets ls`.
    pub fn keys(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    /// Secret file paths only (sorted) — `rpi secrets ls`.
    pub fn file_paths(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// Mode for files from `[secrets].files`.
    pub fn secret_file_mode(&self) -> u32 {
        self.file_mode
            .unwrap_or(crate::secretmode::DEFAULT_SECRET_FILE_MODE)
    }

    /// Mode for the injected `.env`. Only widened when the project asked for
    /// it explicitly.
    pub fn env_mode(&self) -> u32 {
        self.file_mode
            .unwrap_or(crate::secretmode::DEFAULT_ENV_MODE)
    }
}

impl std::fmt::Debug for SecretsBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsBundle")
            .field("keys", &self.keys())
            .field("files", &self.file_paths())
            .finish()
    }
}

/// Deploy gate settings from [healthcheck] in rpi.toml (§8, §12).
/// Per-deploy input: travels with ProjectConfig, not persisted in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthcheckConfig {
    /// HTTP probe path; None => plain TCP connect (when no docker healthcheck).
    pub path: Option<String>,
    /// Expected HTTP status: "2xx" | "3xx" | exact like "204". None => 2xx/3xx.
    pub expect: Option<String>,
    /// Total gate budget in seconds.
    pub timeout_secs: u64,
}

impl Default for HealthcheckConfig {
    fn default() -> HealthcheckConfig {
        HealthcheckConfig {
            path: None,
            expect: None,
            timeout_secs: 60,
        }
    }
}

/// Per-stage deploy timeouts (§8.1). Agent-wide defaults live in agent.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageTimeouts {
    pub fetch_secs: u64,
    pub build_secs: u64,
    pub up_secs: u64,
}

impl Default for StageTimeouts {
    fn default() -> StageTimeouts {
        StageTimeouts {
            fetch_secs: 120,
            build_secs: 1800,
            up_secs: 300,
        }
    }
}

impl StageTimeouts {
    /// Project overrides from [timeouts] in rpi.toml win over agent defaults (§12).
    pub fn with_overrides(&self, overrides: &StageTimeoutOverrides) -> StageTimeouts {
        StageTimeouts {
            fetch_secs: overrides.fetch_secs.unwrap_or(self.fetch_secs),
            build_secs: overrides.build_secs.unwrap_or(self.build_secs),
            up_secs: overrides.up_secs.unwrap_or(self.up_secs),
        }
    }
}

/// Optional per-project overrides ([timeouts] in rpi.toml, §12).
/// Travels with ProjectConfig like HealthcheckConfig; not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageTimeoutOverrides {
    pub fetch_secs: Option<u64>,
    pub build_secs: Option<u64>,
    pub up_secs: Option<u64>,
}

/// How the host port is bound. `Lan` binds `0.0.0.0` (all interfaces).
///
/// NOTE: on a host with a public IPv4, `Lan` exposes the service to the
/// public internet, not just the LAN. Docker also bypasses host firewalls
/// (UFW/iptables) for published ports via its own `DOCKER` iptables chain,
/// so an operator's firewall rules will not block the port. Use `Lan` only
/// on trusted networks or behind an external firewall/router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExposeMode {
    #[default]
    Private,
    Lan,
}

impl ExposeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExposeMode::Private => "private",
            ExposeMode::Lan => "lan",
        }
    }

    pub fn bind_addr(&self) -> &'static str {
        match self {
            ExposeMode::Private => "127.0.0.1",
            ExposeMode::Lan => "0.0.0.0",
        }
    }

    pub fn parse(s: &str) -> Option<ExposeMode> {
        match s {
            "private" => Some(ExposeMode::Private),
            "lan" => Some(ExposeMode::Lan),
            _ => None,
        }
    }
}

impl std::fmt::Display for ExposeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One deployed `[commands]` entry: the declared argv plus an optional compose
/// service to exec into. `service = None` means the project's ingress service
/// (ProjectConfig.service).
///
/// Serde is hand-written for backward compatibility: a service-less command
/// serializes to a bare argv array — byte-identical to the pre-service format
/// stored in the registry and sent over the CLI<->agent protocol — while a
/// service-pinned command uses a struct. Deserialization accepts both shapes,
/// so existing rows and older-CLI payloads decode with `service = None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    pub service: Option<String>,
}

impl CommandSpec {
    pub fn new(argv: Vec<String>) -> CommandSpec {
        CommandSpec {
            argv,
            service: None,
        }
    }
}

impl From<Vec<String>> for CommandSpec {
    fn from(argv: Vec<String>) -> CommandSpec {
        CommandSpec::new(argv)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum CommandSpecRepr {
    Argv(Vec<String>),
    Full {
        argv: Vec<String>,
        #[serde(default)]
        service: Option<String>,
    },
}

impl Serialize for CommandSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.service {
            None => self.argv.serialize(serializer),
            Some(service) => CommandSpecRepr::Full {
                argv: self.argv.clone(),
                service: Some(service.clone()),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CommandSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<CommandSpec, D::Error> {
        match CommandSpecRepr::deserialize(deserializer)? {
            CommandSpecRepr::Argv(argv) => Ok(CommandSpec {
                argv,
                service: None,
            }),
            CommandSpecRepr::Full { argv, service } => {
                if argv.is_empty() {
                    return Err(D::Error::custom("command argv must not be empty"));
                }
                Ok(CommandSpec { argv, service })
            }
        }
    }
}

/// Environment overlay metadata (received in the deploy request's
/// `environment` block, environment-overlays spec). Not persisted by this
/// task — registry persistence is Task 9, handler logic is Task 10.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentMeta {
    pub env: String,
    pub base: String,
    pub slug: Option<String>,
    pub ttl_secs: Option<u64>,
    pub on_create: Option<String>,
}

/// Project config from rpi.toml (received in deploy request, §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    pub repo: String,
    pub branch: String,
    /// Path to compose file relative to repo root.
    pub compose_path: String,
    /// Public service from compose ([ingress].service).
    pub service: String,
    /// Container port of the public service ([ingress].port).
    pub container_port: u16,
    /// FQDN ([ingress].hostname). In v0.1 only stored (ingress — v0.2).
    pub hostname: Option<String>,
    /// How the host port should be exposed. Defaults private for existing configs.
    pub expose: ExposeMode,
    /// Health gate settings ([healthcheck] from rpi.toml). Not persisted in DB.
    pub healthcheck: HealthcheckConfig,
    /// Stage timeout overrides ([timeouts] from rpi.toml). Not persisted in DB.
    pub timeouts: StageTimeoutOverrides,
    /// Declared admin commands ([commands] in rpi.toml), name -> spec.
    /// Persisted in the registry: `rpi command` must run what was deployed,
    /// not what the local file currently says.
    pub commands: BTreeMap<String, CommandSpec>,
    /// [timeouts].command override in seconds. Persisted alongside commands
    /// because it is needed at invocation time, not deploy time.
    pub command_timeout_secs: Option<u64>,
    /// Environment overlay metadata from the deploy request's `environment`
    /// block. `None` for plain (non-overlay) deploys.
    pub environment: Option<EnvironmentMeta>,
    /// Declared secret groups, in attachment order (secret-groups spec).
    /// Empty means only the deploy key's own bundle is injected — the
    /// behavior of every version before groups existed.
    pub secret_groups: Vec<String>,
}

/// Registered project: config + allocated host port (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub config: ProjectConfig,
    pub host_port: u16,
    pub created_at: i64,
    /// Whether the overlay's `on_create` command has already run once.
    pub on_create_done: bool,
    /// Timestamp (unix seconds) of the most recent successful deploy.
    pub last_success_at: Option<i64>,
    /// Commit sha of the most recent successful deploy. Feeds
    /// `RPI_COMMIT_SHA` outside a deploy (`rpi command`, `rpi restart`),
    /// where no fetch has just happened to supply one.
    pub last_commit_sha: Option<String>,
}

/// Branch or specific commit-sha (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployRef {
    Branch(String),
    Sha(String),
}

impl DeployRef {
    /// 40 hex characters is a sha, everything else is a branch.
    pub fn parse(s: &str) -> DeployRef {
        let is_sha = s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit());
        if is_sha {
            DeployRef::Sha(s.to_string())
        } else {
            DeployRef::Branch(s.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            DeployRef::Branch(s) | DeployRef::Sha(s) => s,
        }
    }
}

/// All deployment statuses (§18). Stored as strings in the DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentStatus {
    Queued,
    Running,
    Success,
    Failed,
    Canceled,
    Interrupted,
    Superseded,
}

impl DeploymentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeploymentStatus::Queued => "queued",
            DeploymentStatus::Running => "running",
            DeploymentStatus::Success => "success",
            DeploymentStatus::Failed => "failed",
            DeploymentStatus::Canceled => "canceled",
            DeploymentStatus::Interrupted => "interrupted",
            DeploymentStatus::Superseded => "superseded",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, DeploymentStatus::Queued | DeploymentStatus::Running)
    }
}

impl std::str::FromStr for DeploymentStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(DeploymentStatus::Queued),
            "running" => Ok(DeploymentStatus::Running),
            "success" => Ok(DeploymentStatus::Success),
            "failed" => Ok(DeploymentStatus::Failed),
            "canceled" => Ok(DeploymentStatus::Canceled),
            "interrupted" => Ok(DeploymentStatus::Interrupted),
            "superseded" => Ok(DeploymentStatus::Superseded),
            _ => Err(()),
        }
    }
}

/// Per-stage progress marker for the deploy pipeline view (deploy-stages
/// spec). Travels through `LogSink` alongside log lines so ordering with the
/// surrounding output is guaranteed by the channel itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Started,
    Ok,
    Failed,
    Skipped,
}

impl StageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StageStatus::Started => "started",
            StageStatus::Ok => "ok",
            StageStatus::Failed => "failed",
            StageStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageEvent {
    pub stage: String,
    pub status: StageStatus,
    /// Present on completions (`ok`/`failed`/`skipped`), absent on `started`.
    pub elapsed_ms: Option<u64>,
}

impl StageEvent {
    pub fn started(stage: &str) -> StageEvent {
        StageEvent {
            stage: stage.to_string(),
            status: StageStatus::Started,
            elapsed_ms: None,
        }
    }

    fn done(stage: &str, status: StageStatus, elapsed: std::time::Duration) -> StageEvent {
        StageEvent {
            stage: stage.to_string(),
            status,
            elapsed_ms: Some(elapsed.as_millis() as u64),
        }
    }

    pub fn ok(stage: &str, elapsed: std::time::Duration) -> StageEvent {
        StageEvent::done(stage, StageStatus::Ok, elapsed)
    }

    pub fn failed(stage: &str, elapsed: std::time::Duration) -> StageEvent {
        StageEvent::done(stage, StageStatus::Failed, elapsed)
    }

    pub fn skipped(stage: &str, elapsed: std::time::Duration) -> StageEvent {
        StageEvent::done(stage, StageStatus::Skipped, elapsed)
    }
}

/// One deployment action (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: String,
    pub project: String,
    pub git_ref: String,
    pub commit_sha: Option<String>,
    pub status: DeploymentStatus,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub log_tail: String,
}

/// Result of Source::fetch — where the code is located and which sha was fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedSource {
    pub workdir: PathBuf,
    pub commit_sha: String,
}

/// State of one service in a compose stack (for `rpi ls`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    pub service: String,
    pub state: String,
    /// Docker healthcheck state ("healthy"/"unhealthy"/"starting"), None when
    /// the service declares no healthcheck.
    pub health: Option<String>,
}

/// What to run: project + absolute paths to compose files.
/// Repository docker-compose.override.yml is discovered by the adapter (§12.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeStack {
    pub project_name: String,
    pub workdir: PathBuf,
    pub compose_file: PathBuf,
    pub override_file: PathBuf,
    /// `RPI_*` runtime variables. Exported into the environment of every
    /// `docker compose` invocation for this stack, so `${RPI_*}` interpolates
    /// inside the project's own compose file, and passed as `-e` flags on
    /// `exec`. Empty for call paths that do not need them.
    pub env: BTreeMap<String, String>,
}

/// Live container metrics of one compose service (`rpi stats`, v0.4 design §4).
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceStats {
    pub service: String,
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_limit_bytes: u64,
    /// Docker compose `State` string (e.g. "running"); empty when not reported.
    pub state: String,
    /// Docker healthcheck state ("healthy"/"unhealthy"/"starting"), None when
    /// the service declares no healthcheck.
    pub health: Option<String>,
}

/// Per-project slice of `rpi stats`. last_deploy is filled by the GetStats
/// use-case from DeploymentHistory, not by the stats provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectStats {
    pub project: String,
    pub services: Vec<ServiceStats>,
    pub last_deploy: Option<Deployment>,
}

/// Host metrics (sysinfo + DiskProbe).
#[derive(Debug, Clone, PartialEq)]
pub struct HostStats {
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub disk_used_percent: u8,
    pub uptime_secs: u64,
    /// CPU temperature in °C; `None` on hosts without a readable thermal zone.
    pub temp_celsius: Option<f64>,
}

/// One background host sample retained in the agent's ring buffer.
/// Disk and uptime are intentionally not sampled per-tick — they are
/// assembled into the snapshot at request time.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSample {
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub temp_celsius: Option<f64>,
}

/// Full `rpi stats` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsReport {
    pub host: HostStats,
    pub projects: Vec<ProjectStats>,
    /// Recent host samples (oldest→newest) for CLI sparklines/charts.
    pub host_history: Vec<HostSample>,
}

/// One PASS/FAIL check of `rpi doctor` (§14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
    /// How to fix; only meaningful on failed checks.
    pub hint: Option<String>,
}

/// `rpi doctor` result (§14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticReport {
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// `rpi start|stop|restart` (§16). Maps 1:1 to compose subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Start,
    Stop,
    Restart,
}

impl LifecycleAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleAction::Start => "start",
            LifecycleAction::Stop => "stop",
            LifecycleAction::Restart => "restart",
        }
    }
}

impl std::str::FromStr for LifecycleAction {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(LifecycleAction::Start),
            "stop" => Ok(LifecycleAction::Stop),
            "restart" => Ok(LifecycleAction::Restart),
            _ => Err(()),
        }
    }
}

/// `rpi status` summary (v0.4 design §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOverview {
    pub version: String,
    pub uptime_secs: u64,
    pub disk_used_percent: u8,
    pub projects: usize,
    pub active_deployments: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_bundle_default_is_empty_and_keys_are_sorted() {
        let mut bundle = SecretsBundle::default();
        assert!(bundle.is_empty());
        bundle.vars.insert("Z_KEY".into(), "1".into());
        bundle.vars.insert("A_KEY".into(), "2".into());
        assert!(!bundle.is_empty());
        assert_eq!(
            bundle.keys(),
            vec!["A_KEY".to_string(), "Z_KEY".to_string()]
        );
    }

    #[test]
    fn secrets_bundle_with_only_files_is_not_empty() {
        let mut bundle = SecretsBundle::default();
        bundle
            .files
            .insert("certs/server.pem".into(), b"PEM".to_vec());
        assert!(!bundle.is_empty());
        assert_eq!(bundle.file_paths(), vec!["certs/server.pem".to_string()]);
        assert!(bundle.keys().is_empty());
    }

    #[test]
    fn secrets_bundle_debug_shows_names_without_values_or_contents() {
        let mut bundle = SecretsBundle::default();
        bundle
            .vars
            .insert("API_TOKEN".into(), "raw-token-value".into());
        bundle
            .files
            .insert("certs/server.pem".into(), b"secret-file-body".to_vec());

        let debug = format!("{bundle:?}");

        assert!(debug.contains("SecretsBundle"));
        assert!(debug.contains("API_TOKEN"));
        assert!(debug.contains("certs/server.pem"));
        assert!(!debug.contains("raw-token-value"));
        assert!(!debug.contains("secret-file-body"));
    }

    #[test]
    fn healthcheck_defaults_match_spec() {
        let hc = HealthcheckConfig::default();
        assert_eq!(hc.path, None);
        assert_eq!(hc.expect, None);
        assert_eq!(hc.timeout_secs, 60);
    }

    #[test]
    fn expose_mode_maps_strings_bind_addrs_and_default() {
        assert_eq!(ExposeMode::default(), ExposeMode::Private);
        assert_eq!(ExposeMode::Private.as_str(), "private");
        assert_eq!(ExposeMode::Lan.as_str(), "lan");
        assert_eq!(ExposeMode::Private.bind_addr(), "127.0.0.1");
        assert_eq!(ExposeMode::Lan.bind_addr(), "0.0.0.0");
        assert_eq!(ExposeMode::parse("private"), Some(ExposeMode::Private));
        assert_eq!(ExposeMode::parse("lan"), Some(ExposeMode::Lan));
        assert_eq!(ExposeMode::parse("public"), None);
    }

    #[test]
    fn parse_40_hex_chars_as_sha() {
        let r = DeployRef::parse("0123456789abcdef0123456789abcdef01234567");
        assert_eq!(
            r,
            DeployRef::Sha("0123456789abcdef0123456789abcdef01234567".into())
        );
    }

    #[test]
    fn parse_anything_else_as_branch() {
        assert_eq!(DeployRef::parse("main"), DeployRef::Branch("main".into()));
        // 40 characters but not hex — this is a branch
        assert_eq!(
            DeployRef::parse("zzzz456789abcdef0123456789abcdef01234567"),
            DeployRef::Branch("zzzz456789abcdef0123456789abcdef01234567".into())
        );
    }

    #[test]
    fn status_roundtrips_through_str() {
        for s in [
            DeploymentStatus::Queued,
            DeploymentStatus::Running,
            DeploymentStatus::Success,
            DeploymentStatus::Failed,
            DeploymentStatus::Canceled,
            DeploymentStatus::Interrupted,
            DeploymentStatus::Superseded,
        ] {
            assert_eq!(s.as_str().parse::<DeploymentStatus>(), Ok(s));
        }
        assert_eq!("bogus".parse::<DeploymentStatus>(), Err(()));
    }

    #[test]
    fn terminal_statuses() {
        assert!(!DeploymentStatus::Queued.is_terminal());
        assert!(!DeploymentStatus::Running.is_terminal());
        for s in [
            DeploymentStatus::Success,
            DeploymentStatus::Failed,
            DeploymentStatus::Canceled,
            DeploymentStatus::Interrupted,
            DeploymentStatus::Superseded,
        ] {
            assert!(s.is_terminal(), "{s:?} must be terminal");
        }
    }

    #[test]
    fn stage_timeouts_defaults_match_spec_and_overrides_win() {
        let defaults = StageTimeouts::default();
        assert_eq!(defaults.fetch_secs, 120, "fetch 2m (§8.1)");
        assert_eq!(defaults.build_secs, 1800, "build 30m (§8.1)");
        assert_eq!(defaults.up_secs, 300, "up 5m (§8.1)");

        let overrides = StageTimeoutOverrides {
            build_secs: Some(3600),
            ..StageTimeoutOverrides::default()
        };
        let effective = defaults.with_overrides(&overrides);
        assert_eq!(effective.fetch_secs, 120, "no override -> default");
        assert_eq!(effective.build_secs, 3600, "override wins");
        assert_eq!(effective.up_secs, 300);
    }

    #[test]
    fn lifecycle_action_roundtrips_through_str() {
        for a in [
            LifecycleAction::Start,
            LifecycleAction::Stop,
            LifecycleAction::Restart,
        ] {
            assert_eq!(a.as_str().parse::<LifecycleAction>(), Ok(a));
        }
        assert_eq!("bogus".parse::<LifecycleAction>(), Err(()));
    }

    #[test]
    fn diagnostic_report_all_passed() {
        let pass = DiagnosticCheck {
            name: "docker daemon".into(),
            passed: true,
            detail: "27.0".into(),
            hint: None,
        };
        let fail = DiagnosticCheck {
            name: "cloudflared unit".into(),
            passed: false,
            detail: "inactive".into(),
            hint: Some("systemctl --user start cloudflared".into()),
        };
        assert!(DiagnosticReport {
            checks: vec![pass.clone()]
        }
        .all_passed());
        assert!(!DiagnosticReport {
            checks: vec![pass, fail]
        }
        .all_passed());
        assert!(
            DiagnosticReport::default().all_passed(),
            "no checks - nothing failed"
        );
    }

    #[test]
    fn stage_event_constructors_carry_status_and_elapsed_ms() {
        use std::time::Duration;
        let s = StageEvent::started("fetch");
        assert_eq!(s.stage, "fetch");
        assert_eq!(s.status, StageStatus::Started);
        assert_eq!(s.elapsed_ms, None);

        let ok = StageEvent::ok("build", Duration::from_millis(48_231));
        assert_eq!(ok.status, StageStatus::Ok);
        assert_eq!(ok.elapsed_ms, Some(48_231));

        assert_eq!(
            StageEvent::failed("start", Duration::from_secs(2)).status,
            StageStatus::Failed
        );
        assert_eq!(
            StageEvent::skipped("gc", Duration::from_millis(400)).status,
            StageStatus::Skipped
        );

        assert_eq!(StageStatus::Started.as_str(), "started");
        assert_eq!(StageStatus::Ok.as_str(), "ok");
        assert_eq!(StageStatus::Failed.as_str(), "failed");
        assert_eq!(StageStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn host_sample_holds_all_fields() {
        let s = HostSample {
            at_ms: 1_700_000_000_000,
            cpu_percent: 12.5,
            mem_used_bytes: 1024,
            mem_total_bytes: 4096,
            temp_celsius: Some(42.0),
        };
        assert_eq!(s.at_ms, 1_700_000_000_000);
        assert_eq!(s.temp_celsius, Some(42.0));
        assert_eq!(s.clone(), s);
    }
}

#[cfg(test)]
mod command_spec_tests {
    use super::CommandSpec;

    #[test]
    fn legacy_array_json_deserializes_to_no_service() {
        let spec: CommandSpec = serde_json::from_str(r#"["node","seed.js"]"#).unwrap();
        assert_eq!(
            spec,
            CommandSpec::new(vec!["node".into(), "seed.js".into()])
        );
        assert_eq!(spec.service, None);
    }

    #[test]
    fn no_service_serializes_back_to_bare_array() {
        let spec = CommandSpec::new(vec!["node".into(), "seed.js".into()]);
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(json, r#"["node","seed.js"]"#);
    }

    #[test]
    fn service_pinned_uses_struct_shape_and_roundtrips() {
        let spec = CommandSpec {
            argv: vec!["node".into(), "create-invite.cjs".into()],
            service: Some("server".into()),
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(
            json,
            r#"{"argv":["node","create-invite.cjs"],"service":"server"}"#
        );
        let back: CommandSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn struct_shape_without_service_key_deserializes_to_none() {
        let spec: CommandSpec = serde_json::from_str(r#"{"argv":["node","x.js"]}"#).unwrap();
        assert_eq!(spec.service, None);
        assert_eq!(spec.argv, vec!["node".to_string(), "x.js".into()]);
    }

    #[test]
    fn from_vec_has_no_service() {
        let spec: CommandSpec = vec!["a".to_string()].into();
        assert_eq!(spec.service, None);
    }
}

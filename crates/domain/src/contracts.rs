use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
#[cfg(feature = "mocks")]
use mockall::automock;

use crate::entities::{
    AgentOverview, ComposeStack, DeployRef, Deployment, DeploymentStatus, DiagnosticReport,
    FetchedSource, HostSample, LifecycleAction, Project, ProjectConfig, SecretsBundle,
    ServiceState, ServiceStats, StageEvent, StatsReport,
};
use crate::error::DomainError;
use crate::secretgroup::{GroupHead, GroupRef, GroupSummary, SecretGroup};

/// Receiver for line-by-line deployment logs + terminal event.
/// Implementations: SSE hub of the agent, TailSink in application, stubs in tests.
pub trait LogSink: Send + Sync {
    fn line(&self, line: &str);
    fn finished(&self, status: DeploymentStatus);
    /// Deploy pipeline stage marker (deploy-stages spec). Default no-op: only
    /// the deploy chain (masker → tail → hub) forwards these.
    fn stage(&self, _ev: &StageEvent) {}
    /// Service count after the health gate, for the CLI's result stamp.
    fn summary(&self, _services: usize) {}
}

/// Fetch code by DeployRef (§6). v1 adapter — GitSource.
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait Source: Send + Sync {
    /// Where this project's working copy lives on the agent host (used by
    /// `rpi env send --apply` to re-inject .env without a fetch, §10).
    fn workdir(&self, project_name: &str) -> PathBuf;

    async fn fetch(
        &self,
        project: &ProjectConfig,
        git_ref: &DeployRef,
        log: Arc<dyn LogSink>,
    ) -> Result<FetchedSource, DomainError>;

    async fn cleanup(&self, project_name: &str) -> Result<(), DomainError>;

    /// Deploy-key preflight: verify the agent can read `repo`, generating the
    /// project's deploy key first when missing (SSH repos only; anything else
    /// is `Ok` without probing). Probe failures are data (`Denied`), not
    /// errors — `Err` is reserved for local failures (keygen, fs).
    async fn check_access(
        &self,
        project_name: &str,
        repo: &str,
    ) -> Result<SourceAccess, DomainError>;
}

/// Abstraction of container backend (§6). v1 — DockerComposeRuntime.
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    async fn build(&self, stack: &ComposeStack, log: Arc<dyn LogSink>) -> Result<(), DomainError>;
    async fn up(&self, stack: &ComposeStack, log: Arc<dyn LogSink>) -> Result<(), DomainError>;
    async fn ps(&self, project_name: &str) -> Result<Vec<ServiceState>, DomainError>;
    /// `docker image prune -f` — dangling images only; build cache stays (§8.1).
    async fn prune_images(&self, log: Arc<dyn LogSink>) -> Result<(), DomainError>;
    /// `docker builder prune -f` with an age filter — frees build cache when
    /// the disk crosses the GC threshold (§8.1).
    async fn prune_builder(&self, log: Arc<dyn LogSink>) -> Result<(), DomainError>;
    /// `docker compose logs` of the project (v0.4). With `follow` the future
    /// completes only when interrupted — dropping it kills the child.
    async fn logs(
        &self,
        project_name: &str,
        tail: usize,
        follow: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError>;
    /// Live CPU/memory per service (`docker stats --no-stream`).
    async fn stats(&self, project_name: &str) -> Result<Vec<ServiceStats>, DomainError>;
    /// `docker compose start|stop|restart` — no rebuild, not a deploy.
    async fn lifecycle(
        &self,
        stack: &ComposeStack,
        action: LifecycleAction,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError>;
    /// `docker compose down` for `rpi rm`; named volumes die only with the flag.
    async fn down(
        &self,
        stack: &ComposeStack,
        remove_volumes: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError>;
    /// `docker compose exec -T <service> <argv...>` inside the running
    /// service container ([commands], §spec). Returns the process exit code;
    /// nonzero is data, not an error. Dropping the future kills the compose
    /// exec client (best effort — the in-container process may survive).
    async fn exec(
        &self,
        stack: &ComposeStack,
        service: &str,
        argv: &[String],
        log: Arc<dyn LogSink>,
    ) -> Result<i32, DomainError>;
}

/// Disk fill probe for the GC threshold decision (§8.1). v1 — sysinfo.
#[cfg_attr(feature = "mocks", automock)]
pub trait DiskProbe: Send + Sync {
    /// Used space of the filesystem holding the agent data dir, percent 0..=100.
    fn used_percent(&self) -> Result<u8, DomainError>;
}

/// Detects the agent host's primary LAN IPv4 for building reachable URLs
/// (used by `rpi deploy`/`rpi ls` for expose=lan projects). None when undetectable.
#[cfg_attr(feature = "mocks", automock)]
pub trait HostNetwork: Send + Sync {
    fn primary_ipv4(&self) -> Option<std::net::IpAddr>;
}

/// Project registry + port allocation (§6).
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait ProjectRepository: Send + Sync {
    /// Creates a project (with host port allocation) or updates the config,
    /// preserving the already-allocated host port.
    async fn upsert(&self, config: &ProjectConfig) -> Result<Project, DomainError>;
    async fn get(&self, name: &str) -> Result<Option<Project>, DomainError>;
    async fn list(&self) -> Result<Vec<Project>, DomainError>;
    async fn remove(&self, name: &str) -> Result<(), DomainError>;
    /// Environment entries only (env_name set); base=None -> all bases.
    async fn list_environments<'a>(
        &self,
        base: Option<&'a str>,
    ) -> Result<Vec<Project>, DomainError>;
    /// Sets last_success_at (TTL sliding anchor).
    async fn mark_deploy_success(&self, name: &str, at: i64) -> Result<(), DomainError>;
    async fn set_on_create_done(&self, name: &str, done: bool) -> Result<(), DomainError>;
}

/// Deployment history (§6, §18).
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait DeploymentHistory: Send + Sync {
    /// INSERT the deployment row (normally status Queued). The adapter prunes
    /// old terminal rows of the project beyond its retention right after (§18).
    async fn record_queued(&self, deployment: &Deployment) -> Result<(), DomainError>;
    /// Queued -> Running; refreshes started_at to the actual start moment.
    async fn mark_running(&self, id: &str, started_at: i64) -> Result<(), DomainError>;
    async fn record_finished<'a>(
        &self,
        id: &str,
        status: DeploymentStatus,
        commit_sha: Option<&'a str>,
        finished_at: i64,
        log_tail: &str,
    ) -> Result<(), DomainError>;
    async fn get(&self, id: &str) -> Result<Option<Deployment>, DomainError>;
    /// Non-terminal deployments of a project (queued/running), newest first.
    async fn active(&self, project: &str) -> Result<Vec<Deployment>, DomainError>;
    /// Crash-recovery sweep at agent start (§8.1): queued/running -> interrupted.
    /// Returns the number of rows swept.
    async fn sweep_interrupted(&self, finished_at: i64) -> Result<u64, DomainError>;
    async fn latest(&self, project: &str) -> Result<Option<Deployment>, DomainError>;
    async fn remove_project(&self, project: &str) -> Result<(), DomainError>;
}

/// Writes compose-override mapping <bind>:<host> -> <container> (§12.1).
/// `bind` is "127.0.0.1" (private) or "0.0.0.0" (lan), from ExposeMode.
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait OverrideStore: Send + Sync {
    /// Returns the expected path of the override file for the project.
    fn path(&self, project: &str) -> PathBuf;
    async fn write(
        &self,
        project: &str,
        service: &str,
        bind: &str,
        host_port: u16,
        container_port: u16,
    ) -> Result<PathBuf, DomainError>;
    async fn remove(&self, project: &str) -> Result<(), DomainError>;
}

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

/// Writes the decrypted bundle into the project workdir: `.env` from vars
/// plus each secret file at its relative path (secrets spec §7).
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait SecretsWriter: Send + Sync {
    /// Fails with NotFound when the workdir does not exist (never deployed).
    async fn write(&self, workdir: &Path, bundle: &SecretsBundle) -> Result<(), DomainError>;
}

/// Deploy gate (§8): hybrid docker healthcheck -> HTTP -> TCP.
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait HealthGate: Send + Sync {
    async fn check(
        &self,
        config: &ProjectConfig,
        host_port: u16,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError>;
}

/// Deploy-key preflight verdict (spec 2026-07-10): `Ok` — the agent can read
/// the repo; `Denied` — it cannot, and the project's public deploy key plus
/// the probe's error text travel back for the CLI to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceAccess {
    Ok,
    Denied { pubkey: String, error: String },
}

/// Result of an ingress upsert: `Applied` when the edge is (or already was)
/// routing the hostname, `Skipped` when a disabled backend did nothing and
/// routing remains manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressOutcome {
    Applied,
    Skipped,
}

/// Routes hostname -> 127.0.0.1:host_port on the edge (§6, §11).
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait Ingress: Send + Sync {
    async fn upsert(
        &self,
        hostname: &str,
        host_port: u16,
        log: Arc<dyn LogSink>,
    ) -> Result<IngressOutcome, DomainError>;
    async fn remove(&self, hostname: &str, log: Arc<dyn LogSink>) -> Result<(), DomainError>;
}

/// Locally-managed tunnel credentials (no cert.pem, no `cloudflared tunnel login`).
pub struct TunnelCreds {
    pub account_tag: String,
    pub tunnel_id: String,
    pub tunnel_name: String,
    /// base64. EMPTY when an existing tunnel was adopted rather than created
    /// (the secret can't be recovered from the API) — callers must not
    /// overwrite an existing credentials file with an empty secret.
    pub tunnel_secret: String,
}

/// Cloudflare REST API: tunnel create/adopt + DNS CNAME writes (§6, §11).
#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait CloudflareApi: Send + Sync {
    async fn zone_id(&self, zone: &str) -> Result<String, DomainError>;
    /// Adopt an existing tunnel by name, else create it. Returns creds.
    async fn find_or_create_tunnel(&self, name: &str) -> Result<TunnelCreds, DomainError>;
    /// Upsert a proxied CNAME <name> -> <tunnel_id>.cfargotunnel.com.
    async fn put_tunnel_cname(
        &self,
        zone: &str,
        name: &str,
        tunnel_id: &str,
    ) -> Result<(), DomainError>;
    /// Delete the proxied CNAME for <name> if it exists. Absent record is Ok.
    async fn delete_tunnel_cname(&self, zone: &str, name: &str) -> Result<(), DomainError>;
}

#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait StatsProvider: Send + Sync {
    async fn report(&self, projects: Vec<String>) -> Result<StatsReport, DomainError>;
}

#[cfg_attr(feature = "mocks", automock)]
#[async_trait]
pub trait SystemProbe: Send + Sync {
    async fn diagnostics(&self) -> DiagnosticReport;
    async fn overview(&self) -> Result<AgentOverview, DomainError>;
}

/// Time determinism in tests (§6).
#[cfg_attr(feature = "mocks", automock)]
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
}

/// Identifier determinism in tests (§6).
#[cfg_attr(feature = "mocks", automock)]
pub trait IdGen: Send + Sync {
    fn new_id(&self) -> String;
}

/// Read side of the agent's host-metrics ring buffer (synchronous, no IO).
#[cfg_attr(feature = "mocks", automock)]
pub trait HostMetricsStore: Send + Sync {
    fn latest(&self) -> Option<HostSample>;
    fn history(&self) -> Vec<HostSample>;
}

/// CPU temperature probe (synchronous).
#[cfg_attr(feature = "mocks", automock)]
pub trait TempProbe: Send + Sync {
    fn cpu_celsius(&self) -> Option<f64>;
}

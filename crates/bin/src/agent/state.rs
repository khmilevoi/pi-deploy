use std::sync::Arc;

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_application::command::RunCommand;
use pi_application::deploy::DeployProject;
use pi_application::diagnostics::{AgentStatus, RunDiagnostics};
use pi_application::environments::{DestroyEnvironment, ListEnvironments, ResetEnvironmentData};
use pi_application::gc::RunGc;
use pi_application::lifecycle::ControlLifecycle;
use pi_application::list::ListProjects;
use pi_application::logs::StreamLogs;
use pi_application::remove::RemoveProject;
use pi_application::scheduler::{DeployRunner, DeployScheduler};
use pi_application::secretgroups::{
    ListSecretGroups, PushSecretGroup, RemoveSecretGroup, ShowSecretGroup,
};
use pi_application::secrets::{ApplySecrets, HeadKeySecrets, ListSecrets, SendSecrets};
use pi_application::stats::GetStats;
use pi_domain::contracts::{
    DeploymentHistory, HostMetricsStore, HostNetwork, IdGen, Ingress, ProjectRepository, Source,
    TempProbe,
};
use pi_infrastructure::cloudflared::{CloudflaredIngress, DisabledIngress};
use pi_infrastructure::disk::SysinfoDiskProbe;
use pi_infrastructure::docker::DockerComposeRuntime;
use pi_infrastructure::events::DeployEventsHub;
use pi_infrastructure::git::GitSource;
use pi_infrastructure::health::HybridHealthGate;
use pi_infrastructure::history::SqliteHistory;
use pi_infrastructure::hostnet::UdpHostNetwork;
use pi_infrastructure::metrics::HostMetricsSampler;
use pi_infrastructure::overrides::FsOverrideStore;
use pi_infrastructure::probe::{HostSystemProbe, SystemRunner};
use pi_infrastructure::repo::SqliteProjectRepo;
use pi_infrastructure::secrets::EncryptedFileStore;
use pi_infrastructure::secretsfile::FsSecretsWriter;
use pi_infrastructure::sqlite::Db;
use pi_infrastructure::stats::CompositeStats;
use pi_infrastructure::sys::{SystemClock, UuidGen};
use pi_infrastructure::temp::ThermalZoneTempProbe;

use crate::agent::config::AgentConfig;

#[derive(Clone)]
pub struct AppState {
    pub scheduler: Arc<DeployScheduler>,
    pub list: Arc<ListProjects>,
    pub history: Arc<dyn DeploymentHistory>,
    pub hub: Arc<DeployEventsHub>,
    pub ids: Arc<dyn IdGen>,
    pub source: Arc<dyn Source>,
    pub send_secrets: Arc<SendSecrets>,
    pub list_secrets: Arc<ListSecrets>,
    /// Deploy-key metadata head (secret-groups spec) — never a value, so the
    /// HTTP layer never needs to touch the store directly.
    pub head_key_secrets: Arc<HeadKeySecrets>,
    /// Re-injects a deploy key's merged (groups + own) secrets and runs
    /// `up -d`. Shared by `send_secrets_handler`'s `--apply` branch and
    /// `apply_key_secrets_handler` — one implementation, not two.
    pub apply_secrets: Arc<ApplySecrets>,
    /// Group CRUD (secret-groups spec): `/v1/projects/{base}/secret-groups`.
    pub push_secret_group: Arc<PushSecretGroup>,
    pub show_secret_group: Arc<ShowSecretGroup>,
    pub list_secret_groups: Arc<ListSecretGroups>,
    pub remove_secret_group: Arc<RemoveSecretGroup>,
    pub gc: Arc<RunGc>,
    pub stream_logs: Arc<StreamLogs>,
    pub stats: Arc<GetStats>,
    pub lifecycle: Arc<ControlLifecycle>,
    pub commands: Arc<RunCommand>,
    pub remove: Arc<RemoveProject>,
    /// Environment overlay lifecycle (environment-overlays spec, `rpi env
    /// ls/rm/reset-data`). `destroy_env` wraps `remove` above rather than
    /// duplicating its teardown; `reset_env` has its own guard.
    pub list_envs: Arc<ListEnvironments>,
    pub destroy_env: Arc<DestroyEnvironment>,
    pub reset_env: Arc<ResetEnvironmentData>,
    pub diagnostics: Arc<RunDiagnostics>,
    pub agent_status: Arc<AgentStatus>,
    pub host_network: Arc<dyn HostNetwork>,
    pub log_dir: PathBuf,
    pub log_dir_available: bool,
    /// Project registry, consulted by `create_deployment` for the base/
    /// environment kind guard (environment-overlays spec) before submitting
    /// the deploy.
    pub projects: Arc<dyn ProjectRepository>,
    // Not read directly today — `stats` already holds its own clone via
    // `CompositeStats`. Kept on `AppState` as the shared handle for handlers
    // added by later stats-modes work (e.g. a live/SSE metrics endpoint).
    #[allow(dead_code)]
    pub metrics: Arc<dyn HostMetricsStore>,
}

pub fn build_state(
    config: &AgentConfig,
    log_dir_available: bool,
) -> anyhow::Result<(AppState, HostMetricsSampler)> {
    std::fs::create_dir_all(&config.data_dir)?;
    let db = Db::open(&config.data_dir.join("state.db")).map_err(|e| anyhow::anyhow!("{e}"))?;

    let projects = SqliteProjectRepo::new(db.clone(), config.port_min, config.port_max);
    let history: Arc<dyn DeploymentHistory> = SqliteHistory::new(db, config.history_keep);
    let source = GitSource::new(&config.data_dir);
    let runtime = DockerComposeRuntime::new();
    let disk = SysinfoDiskProbe::new(&config.data_dir);
    let gc = RunGc::new(
        runtime.clone(),
        disk.clone(),
        config.gc.disk_threshold_percent,
    );
    let overrides = FsOverrideStore::new(config.data_dir.join("overrides"));
    let secrets = EncryptedFileStore::open(&config.data_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    let secrets_writer: Arc<dyn pi_domain::contracts::SecretsWriter> = FsSecretsWriter::new();
    let health = HybridHealthGate::new(runtime.clone());
    let mut ingress_active = false;
    let ingress: Arc<dyn Ingress> = match (&config.cloudflared, &config.cloudflare) {
        (Some(cf_local), Some(cf_acct)) => match std::fs::read_to_string(&cf_acct.token_file) {
            Ok(raw) => {
                let token = raw.trim().to_string();
                let api: Arc<dyn pi_domain::contracts::CloudflareApi> =
                    Arc::new(pi_infrastructure::cloudflare::HttpCloudflare::new(
                        token,
                        cf_acct.account_id.clone(),
                    ));
                let tunnel_id = cf_local.tunnel_id.clone().unwrap_or_default();
                ingress_active = true;
                CloudflaredIngress::new(
                    cf_local.config.clone(),
                    tunnel_id,
                    cf_acct.zone.clone(),
                    cf_local.restart.clone(),
                    api,
                )
            }
            Err(e) => {
                tracing::warn!(
                    "cloudflare token unreadable at {}: {e}; automatic ingress disabled (deploys continue, route manually)",
                    cf_acct.token_file.display()
                );
                DisabledIngress::new()
            }
        },
        (Some(_), None) => {
            tracing::warn!(
                "[cloudflared] is set in agent.toml but [cloudflare] (zone + token_file) is missing; automatic DNS now requires both, so ingress is disabled — add a [cloudflare] section or route manually"
            );
            DisabledIngress::new()
        }
        _ => DisabledIngress::new(),
    };

    let host_network: Arc<dyn HostNetwork> = Arc::new(UdpHostNetwork::new());

    let deploy = DeployProject::new(
        source.clone(),
        runtime.clone(),
        projects.clone(),
        Arc::clone(&history),
        overrides.clone(),
        secrets.clone(),
        Arc::clone(&secrets_writer),
        health,
        ingress.clone(),
        Arc::clone(&host_network),
        SystemClock::new(),
        Arc::clone(&gc),
        config.stage_timeouts()?,
        config.build_concurrency,
    );
    let scheduler = DeployScheduler::new(
        deploy as Arc<dyn DeployRunner>,
        Arc::clone(&history),
        SystemClock::new(),
    );
    let list = ListProjects::new(projects.clone(), runtime.clone(), Arc::clone(&host_network));
    let stream_logs = StreamLogs::new(projects.clone(), secrets.clone(), runtime.clone());
    let temp_probe: Arc<dyn TempProbe> = ThermalZoneTempProbe::new(Path::new("/"));
    let sampler = HostMetricsSampler::with_defaults(
        temp_probe,
        Duration::from_secs(300),
        Duration::from_secs(2),
    );
    let metrics = sampler.handle();
    let stats_provider = CompositeStats::new(runtime.clone(), disk.clone(), Arc::clone(&metrics));
    let stats = GetStats::new(projects.clone(), Arc::clone(&history), stats_provider);
    let lifecycle = ControlLifecycle::new(
        projects.clone(),
        runtime.clone(),
        source.clone(),
        overrides.clone(),
    );
    let commands = RunCommand::new(
        projects.clone(),
        runtime.clone(),
        source.clone(),
        overrides.clone(),
    );
    let remove = RemoveProject::new(
        projects.clone(),
        Arc::clone(&history),
        runtime.clone(),
        ingress.clone(),
        source.clone(),
        secrets.clone(),
        overrides.clone(),
    );
    let list_envs = ListEnvironments::new(projects.clone());
    let destroy_env = DestroyEnvironment::new(projects.clone(), Arc::clone(&remove));
    let reset_env = ResetEnvironmentData::new(
        projects.clone(),
        Arc::clone(&history),
        runtime.clone(),
        source.clone(),
        overrides.clone(),
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let probe = HostSystemProbe::new(
        Arc::new(SystemRunner),
        disk,
        projects.clone(),
        env!("CARGO_PKG_VERSION").to_string(),
        config.data_dir.to_string_lossy().into_owned(),
        config.gc.disk_threshold_percent,
        config.cloudflared.is_some(),
        ingress_active,
        config
            .cloudflared
            .as_ref()
            .map(|c| c.config.to_string_lossy().into_owned()),
        now,
    );
    let diagnostics = RunDiagnostics::new(probe.clone());
    let agent_status = AgentStatus::new(probe, projects.clone(), Arc::clone(&history));
    let projects_repo: Arc<dyn ProjectRepository> = projects.clone();
    let apply_secrets = ApplySecrets::new(
        secrets.clone(),
        projects.clone(),
        source.clone(),
        secrets_writer.clone(),
        overrides.clone(),
        runtime.clone(),
    );
    let push_secret_group = PushSecretGroup::new(secrets.clone());
    let show_secret_group = ShowSecretGroup::new(secrets.clone());
    let list_secret_groups = ListSecretGroups::new(secrets.clone(), projects.clone());
    let remove_secret_group = RemoveSecretGroup::new(secrets.clone(), projects.clone());
    let head_key_secrets = HeadKeySecrets::new(secrets.clone());
    let send_secrets = SendSecrets::new(secrets.clone(), Arc::clone(&apply_secrets));
    let list_secrets = ListSecrets::new(secrets, projects);

    Ok((
        AppState {
            scheduler,
            list,
            history,
            hub: DeployEventsHub::new(),
            ids: UuidGen::new(),
            source: source as Arc<dyn Source>,
            send_secrets,
            list_secrets,
            head_key_secrets,
            apply_secrets,
            push_secret_group,
            show_secret_group,
            list_secret_groups,
            remove_secret_group,
            gc,
            stream_logs,
            stats,
            lifecycle,
            commands,
            remove,
            list_envs,
            destroy_env,
            reset_env,
            diagnostics,
            agent_status,
            host_network: Arc::clone(&host_network),
            log_dir: config.logs.dir.clone(),
            log_dir_available,
            projects: projects_repo,
            metrics,
        },
        sampler,
    ))
}

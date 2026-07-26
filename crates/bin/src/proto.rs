use std::collections::BTreeMap;

use pi_application::list::ProjectView;
use pi_application::remove::RemoveReport;
use pi_domain::entities::{
    AgentOverview, CommandSpec, Deployment, DiagnosticCheck, DiagnosticReport, EnvironmentMeta,
    ExposeMode, HealthcheckConfig, HostSample, HostStats, ProjectConfig, ProjectStats,
    ServiceStats, StageTimeoutOverrides, StatsReport,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub api: String,
    /// `None`: the agent predates the capability handshake (spec
    /// 2026-07-12) — reconstruct via `compat::legacy_capabilities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckDto {
    pub path: Option<String>,
    pub expect: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutsDto {
    pub fetch_secs: Option<u64>,
    pub build_secs: Option<u64>,
    pub up_secs: Option<u64>,
    pub command_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDto {
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub compose: String,
    pub service: String,
    pub port: u16,
    pub hostname: Option<String>,
    #[serde(default)]
    pub expose: Option<String>,
    #[serde(default)]
    pub healthcheck: Option<HealthcheckDto>,
    #[serde(default)]
    pub timeouts: Option<TimeoutsDto>,
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_groups: Vec<String>,
}

impl From<ProjectDto> for ProjectConfig {
    fn from(dto: ProjectDto) -> ProjectConfig {
        let command_timeout_secs = dto.timeouts.as_ref().and_then(|t| t.command_secs);
        ProjectConfig {
            name: dto.name,
            repo: dto.repo,
            branch: dto.branch,
            compose_path: dto.compose,
            service: dto.service,
            container_port: dto.port,
            hostname: dto.hostname,
            expose: dto
                .expose
                .as_deref()
                .and_then(ExposeMode::parse)
                .unwrap_or_default(),
            healthcheck: dto
                .healthcheck
                .map(|h| HealthcheckConfig {
                    path: h.path,
                    expect: h.expect,
                    timeout_secs: h.timeout_secs.unwrap_or(60),
                })
                .unwrap_or_default(),
            timeouts: dto
                .timeouts
                .map(|t| StageTimeoutOverrides {
                    fetch_secs: t.fetch_secs,
                    build_secs: t.build_secs,
                    up_secs: t.up_secs,
                })
                .unwrap_or_default(),
            commands: dto.commands,
            command_timeout_secs,
            // The handler fills this in from the request's `environment` block.
            environment: None,
            secret_groups: dto.secret_groups,
        }
    }
}

impl From<&ProjectConfig> for ProjectDto {
    fn from(config: &ProjectConfig) -> ProjectDto {
        ProjectDto {
            name: config.name.clone(),
            repo: config.repo.clone(),
            branch: config.branch.clone(),
            compose: config.compose_path.clone(),
            service: config.service.clone(),
            port: config.container_port,
            hostname: config.hostname.clone(),
            expose: Some(config.expose.as_str().to_string()),
            healthcheck: Some(HealthcheckDto {
                path: config.healthcheck.path.clone(),
                expect: config.healthcheck.expect.clone(),
                timeout_secs: Some(config.healthcheck.timeout_secs),
            }),
            timeouts: Some(TimeoutsDto {
                fetch_secs: config.timeouts.fetch_secs,
                build_secs: config.timeouts.build_secs,
                up_secs: config.timeouts.up_secs,
                command_secs: config.command_timeout_secs,
            }),
            commands: config.commands.clone(),
            secret_groups: config.secret_groups.clone(),
        }
    }
}

/// Legacy `/env` route DTOs (agent/http.rs). The CLI no longer sends these
/// (Task 8 replaced `rpi env send/ls` with `rpi secrets send/ls`), but the
/// agent's `/env` route handlers still deserialize/serialize them to keep
/// talking to not-yet-upgraded old CLIs; kept only for that.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvSendRequest {
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvSendResponse {
    pub saved_keys: usize,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvKeysResponse {
    pub keys: Vec<String>,
}

/// Secrets bundle limits, enforced by the CLI before upload and re-checked
/// by the agent (secrets spec §2.7). Decoded byte sizes.
pub const MAX_SECRET_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_SECRETS_BUNDLE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsSendRequest {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> base64-encoded contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// `[secrets].file_mode`, already parsed. Absent -> agent defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    /// Write only if the stored revision equals this. Absent means an
    /// unconditional write — which is what every CLI before 0.27.0 sends, so
    /// the guard is opt-in on this path and never breaks an old client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsSendResponse {
    pub saved_keys: usize,
    pub saved_files: usize,
    pub applied: bool,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsListResponse {
    pub keys: Vec<String>,
    pub files: Vec<String>,
    /// Absent from agents older than 0.26.0 — the CLI then prints no mode
    /// line rather than guessing a value that host never wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    /// Absent from agents older than 0.27.0 — the CLI then prints the flat
    /// list it always printed rather than inventing provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<SecretLayerDto>,
}

/// One layer of the effective view (secret-groups spec: Attachment and
/// layering). Names only — a layer never carries values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretLayerDto {
    pub label: String,
    pub revision: u64,
    pub vars: Vec<String>,
    pub files: Vec<String>,
}

/// `PUT /v1/projects/{base}/secret-groups/{group}` (secret-groups spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupPushRequest {
    pub vars: BTreeMap<String, String>,
    /// Relative path (forward slashes) -> base64-encoded contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
    /// Write only if the stored revision equals this. Absent means an
    /// unconditional write (`--force`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Upsert instead of replacing the group wholesale.
    #[serde(default)]
    pub merge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupPushResponse {
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFileHeadDto {
    pub size: u64,
    pub digest: String,
}

/// Metadata projection. Never contains a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupHeadResponse {
    pub revision: u64,
    /// var name -> digest
    pub vars: BTreeMap<String, String>,
    pub files: BTreeMap<String, SecretFileHeadDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupSummaryDto {
    pub name: String,
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
    pub bytes: u64,
    pub updated_at: i64,
    pub attached_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretGroupsListResponse {
    pub groups: Vec<SecretGroupSummaryDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsApplyResponse {
    pub keys: usize,
    pub files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResponse {
    pub disk_used_percent: u8,
    pub builder_pruned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatsDto {
    pub service: String,
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_limit_bytes: u64,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub health: Option<String>,
}

impl From<ServiceStats> for ServiceStatsDto {
    fn from(s: ServiceStats) -> ServiceStatsDto {
        ServiceStatsDto {
            service: s.service,
            cpu_percent: s.cpu_percent,
            mem_used_bytes: s.mem_used_bytes,
            mem_limit_bytes: s.mem_limit_bytes,
            state: s.state,
            health: s.health,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatsDto {
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub disk_used_percent: u8,
    pub uptime_secs: u64,
    #[serde(default)]
    pub temp_celsius: Option<f64>,
}

impl From<HostStats> for HostStatsDto {
    fn from(h: HostStats) -> HostStatsDto {
        HostStatsDto {
            cpu_percent: h.cpu_percent,
            mem_used_bytes: h.mem_used_bytes,
            mem_total_bytes: h.mem_total_bytes,
            disk_used_percent: h.disk_used_percent,
            uptime_secs: h.uptime_secs,
            temp_celsius: h.temp_celsius,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSampleDto {
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    #[serde(default)]
    pub temp_celsius: Option<f64>,
}

impl From<HostSample> for HostSampleDto {
    fn from(s: HostSample) -> HostSampleDto {
        HostSampleDto {
            at_ms: s.at_ms,
            cpu_percent: s.cpu_percent,
            mem_used_bytes: s.mem_used_bytes,
            mem_total_bytes: s.mem_total_bytes,
            temp_celsius: s.temp_celsius,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStatsDto {
    pub project: String,
    pub services: Vec<ServiceStatsDto>,
    pub last_deploy: Option<DeploymentDto>,
}

impl From<ProjectStats> for ProjectStatsDto {
    fn from(p: ProjectStats) -> ProjectStatsDto {
        ProjectStatsDto {
            project: p.project,
            services: p.services.into_iter().map(Into::into).collect(),
            last_deploy: p.last_deploy.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsReportDto {
    pub host: HostStatsDto,
    pub projects: Vec<ProjectStatsDto>,
    #[serde(default)]
    pub host_history: Vec<HostSampleDto>,
}

impl From<StatsReport> for StatsReportDto {
    fn from(r: StatsReport) -> StatsReportDto {
        StatsReportDto {
            host: r.host.into(),
            projects: r.projects.into_iter().map(Into::into).collect(),
            host_history: r.host_history.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOverviewDto {
    pub version: String,
    pub uptime_secs: u64,
    pub disk_used_percent: u8,
    pub projects: usize,
    pub active_deployments: usize,
}

impl From<AgentOverview> for AgentOverviewDto {
    fn from(a: AgentOverview) -> AgentOverviewDto {
        AgentOverviewDto {
            version: a.version,
            uptime_secs: a.uptime_secs,
            disk_used_percent: a.disk_used_percent,
            projects: a.projects,
            active_deployments: a.active_deployments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCheckDto {
    pub name: String,
    pub passed: bool,
    pub detail: String,
    pub hint: Option<String>,
}

impl From<DiagnosticCheck> for DiagnosticCheckDto {
    fn from(c: DiagnosticCheck) -> DiagnosticCheckDto {
        DiagnosticCheckDto {
            name: c.name,
            passed: c.passed,
            detail: c.detail,
            hint: c.hint,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticReportDto {
    pub checks: Vec<DiagnosticCheckDto>,
}

impl From<DiagnosticReport> for DiagnosticReportDto {
    fn from(r: DiagnosticReport) -> DiagnosticReportDto {
        DiagnosticReportDto {
            checks: r.checks.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleResponse {
    pub project: String,
    pub action: String,
}

/// GET /v1/projects/{name}/commands — deployed [commands], name -> argv.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsResponse {
    pub commands: BTreeMap<String, CommandSpec>,
}

/// POST /v1/projects/{name}/commands/{command} body: extra argv items
/// appended to the declared command (never replacing the program).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRunRequest {
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveResponse {
    pub project: String,
    pub hostname: Option<String>,
    pub volumes_removed: bool,
}

impl From<RemoveReport> for RemoveResponse {
    fn from(r: RemoveReport) -> RemoveResponse {
        RemoveResponse {
            project: r.project,
            hostname: r.hostname,
            volumes_removed: r.volumes_removed,
        }
    }
}

/// Environment overlay metadata from the deploy request (environment-overlays
/// spec). Field-for-field mirror of `EnvironmentMeta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentDto {
    pub env: String,
    pub base: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub on_create: Option<String>,
}

impl From<EnvironmentDto> for EnvironmentMeta {
    fn from(dto: EnvironmentDto) -> EnvironmentMeta {
        EnvironmentMeta {
            env: dto.env,
            base: dto.base,
            slug: dto.slug,
            ttl_secs: dto.ttl_secs,
            on_create: dto.on_create,
        }
    }
}

impl From<&EnvironmentMeta> for EnvironmentDto {
    fn from(meta: &EnvironmentMeta) -> EnvironmentDto {
        EnvironmentDto {
            env: meta.env.clone(),
            base: meta.base.clone(),
            slug: meta.slug.clone(),
            ttl_secs: meta.ttl_secs,
            on_create: meta.on_create.clone(),
        }
    }
}

/// `GET /v1/environments` element (`rpi env ls`, environment-overlays
/// spec): registry-level view of one environment overlay, distinct from
/// [`EnvironmentDto`] (the deploy-request metadata block) because it also
/// carries the project's key and lifecycle timestamps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentViewDto {
    pub key: String,
    pub base: String,
    pub env: String,
    #[serde(default)]
    pub slug: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub last_success_at: Option<i64>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// `DELETE /v1/environments/{key}` and `POST /v1/environments/{key}/reset-data`
/// response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentActionResponse {
    pub key: String,
    #[serde(default)]
    pub already_absent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployRequest {
    pub project: ProjectDto,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// Environment overlay block (environment-overlays spec). Absent for
    /// plain deploys and for pre-overlay CLIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployAccepted {
    pub deployment_id: String,
    /// true when the deploy waits behind an active one (latest wins, §8.1).
    #[serde(default)]
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentDto {
    pub id: String,
    pub project: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub commit_sha: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub log_tail: String,
}

impl From<Deployment> for DeploymentDto {
    fn from(d: Deployment) -> DeploymentDto {
        DeploymentDto {
            id: d.id,
            project: d.project,
            git_ref: d.git_ref,
            commit_sha: d.commit_sha,
            status: d.status.as_str().to_string(),
            started_at: d.started_at,
            finished_at: d.finished_at,
            log_tail: d.log_tail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStateDto {
    pub service: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectViewDto {
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub hostname: Option<String>,
    pub host_port: u16,
    pub expose: String,
    /// Detected LAN ip for expose=lan projects; absent for private or when the
    /// agent could not detect one. `#[serde(default)]` keeps legacy agents
    /// (which never send this field) deserializable.
    #[serde(default)]
    pub lan_ip: Option<String>,
    pub services: Vec<ServiceStateDto>,
}

impl From<ProjectView> for ProjectViewDto {
    fn from(v: ProjectView) -> ProjectViewDto {
        ProjectViewDto {
            name: v.name,
            repo: v.repo,
            branch: v.branch,
            hostname: v.hostname,
            host_port: v.host_port,
            expose: v.expose.as_str().to_string(),
            lan_ip: v.lan_ip.map(|ip| ip.to_string()),
            services: v
                .services
                .into_iter()
                .map(|s| ServiceStateDto {
                    service: s.service,
                    state: s.state,
                })
                .collect(),
        }
    }
}

/// POST /v1/projects/{name}/source/check — deploy-key preflight (spec
/// 2026-07-10). A failed probe is data (`ok: false` + key to register),
/// never an HTTP error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCheckRequest {
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCheckResponse {
    pub ok: bool,
    #[serde(default)]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v01_deploy_request_without_healthcheck_still_deserializes() {
        let json = r#"{"project":{"name":"a","repo":"r","branch":"main","compose":"docker-compose.yml","service":"web","port":3000,"hostname":null},"ref":null}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        let config: ProjectConfig = req.project.into();
        assert_eq!(config.healthcheck.timeout_secs, 60);
    }

    #[test]
    fn healthcheck_roundtrips_through_dto() {
        let mut config: ProjectConfig = ProjectDto {
            name: "a".into(),
            repo: "r".into(),
            branch: "main".into(),
            compose: "docker-compose.yml".into(),
            service: "web".into(),
            port: 3000,
            hostname: None,
            expose: None,
            healthcheck: None,
            timeouts: None,
            commands: BTreeMap::new(),
            secret_groups: Vec::new(),
        }
        .into();
        config.healthcheck.path = Some("/health".into());
        config.healthcheck.timeout_secs = 120;
        let dto = ProjectDto::from(&config);
        let back: ProjectConfig = dto.into();
        assert_eq!(back.healthcheck, config.healthcheck);
    }

    #[test]
    fn timeouts_roundtrip_through_dto_and_default_when_absent() {
        let json = r#"{"project":{"name":"a","repo":"r","branch":"main","compose":"docker-compose.yml","service":"web","port":3000,"hostname":null},"ref":null}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        let config: ProjectConfig = req.project.into();
        assert_eq!(
            config.timeouts,
            Default::default(),
            "v0.2 payloads still work"
        );

        let mut config = config;
        config.timeouts.build_secs = Some(3600);
        let dto = ProjectDto::from(&config);
        let back: ProjectConfig = dto.into();
        assert_eq!(back.timeouts.build_secs, Some(3600));
    }

    #[test]
    fn expose_roundtrips_and_defaults_private_when_absent() {
        // legacy payload without `expose` -> Private
        let json = r#"{"project":{"name":"a","repo":"r","branch":"main","compose":"docker-compose.yml","service":"web","port":3000,"hostname":null},"ref":null}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        let config: ProjectConfig = req.project.into();
        assert_eq!(config.expose, pi_domain::entities::ExposeMode::Private);

        // lan roundtrip
        let mut config = config;
        config.expose = pi_domain::entities::ExposeMode::Lan;
        let dto = ProjectDto::from(&config);
        assert_eq!(dto.expose.as_deref(), Some("lan"));
        let back: ProjectConfig = dto.into();
        assert_eq!(back.expose, pi_domain::entities::ExposeMode::Lan);
    }

    #[test]
    fn project_view_dto_exposes_expose_mode_string() {
        let view = ProjectView {
            name: "a".into(),
            repo: "r".into(),
            branch: "main".into(),
            hostname: None,
            host_port: 8000,
            expose: pi_domain::entities::ExposeMode::Lan,
            lan_ip: None,
            services: vec![],
        };
        let dto = ProjectViewDto::from(view);
        assert_eq!(dto.expose, "lan");
        assert_eq!(dto.lan_ip, None);
    }

    #[test]
    fn project_view_dto_carries_expose_and_lan_ip() {
        let view = ProjectView {
            name: "a".into(),
            repo: "r".into(),
            branch: "main".into(),
            hostname: None,
            host_port: 8000,
            expose: pi_domain::entities::ExposeMode::Lan,
            lan_ip: Some("192.168.1.50".parse().unwrap()),
            services: vec![],
        };
        let dto = ProjectViewDto::from(view);
        assert_eq!(dto.expose, "lan");
        assert_eq!(dto.lan_ip.as_deref(), Some("192.168.1.50"));
    }

    #[test]
    fn legacy_project_dto_without_commands_deserializes_empty() {
        let json = serde_json::json!({
            "name": "rateme", "repo": "r", "branch": "main",
            "compose": "docker-compose.yml", "service": "web",
            "port": 3000, "hostname": null
        });
        let dto: ProjectDto = serde_json::from_value(json).unwrap();
        assert!(dto.commands.is_empty());
        let config: pi_domain::entities::ProjectConfig = dto.into();
        assert!(config.commands.is_empty());
        assert_eq!(config.command_timeout_secs, None);
    }

    #[test]
    fn commands_roundtrip_config_to_dto_and_back() {
        let mut config = pi_domain::entities::ProjectConfig {
            name: "rateme".into(),
            repo: "r".into(),
            branch: "main".into(),
            compose_path: "docker-compose.yml".into(),
            service: "web".into(),
            container_port: 3000,
            hostname: None,
            expose: Default::default(),
            healthcheck: Default::default(),
            timeouts: Default::default(),
            commands: Default::default(),
            command_timeout_secs: Some(1800),
            environment: None,
            secret_groups: Vec::new(),
        };
        config.commands.insert(
            "migrate".into(),
            CommandSpec::new(vec!["npx".into(), "prisma".into()]),
        );

        let dto: ProjectDto = (&config).into();
        let json = serde_json::to_value(&dto).unwrap();
        let back: ProjectDto = serde_json::from_value(json).unwrap();
        let roundtripped: pi_domain::entities::ProjectConfig = back.into();
        assert_eq!(roundtripped.commands, config.commands);
        assert_eq!(roundtripped.command_timeout_secs, Some(1800));
    }

    #[test]
    fn service_pinned_command_survives_dto_roundtrip() {
        let mut config = pi_domain::entities::ProjectConfig {
            name: "rateme".into(),
            repo: "r".into(),
            branch: "main".into(),
            compose_path: "docker-compose.yml".into(),
            service: "web".into(),
            container_port: 3000,
            hostname: None,
            expose: Default::default(),
            healthcheck: Default::default(),
            timeouts: Default::default(),
            commands: Default::default(),
            command_timeout_secs: None,
            environment: None,
            secret_groups: Vec::new(),
        };
        config.commands.insert(
            "create-invite".into(),
            CommandSpec {
                argv: vec!["node".into(), "x.cjs".into()],
                service: Some("server".into()),
            },
        );
        let dto: ProjectDto = (&config).into();
        let json = serde_json::to_value(&dto).unwrap();
        let back: ProjectDto = serde_json::from_value(json).unwrap();
        let roundtripped: pi_domain::entities::ProjectConfig = back.into();
        assert_eq!(
            roundtripped
                .commands
                .get("create-invite")
                .unwrap()
                .service
                .as_deref(),
            Some("server")
        );
    }

    #[test]
    fn old_agent_stats_body_without_history_or_temp_decodes_to_defaults() {
        // An old agent never emits `temp_celsius` or `host_history`.
        let json = r#"{"host":{"cpu_percent":5.0,"mem_used_bytes":100,"mem_total_bytes":200,"disk_used_percent":10,"uptime_secs":42},"projects":[]}"#;
        let dto: StatsReportDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.host.temp_celsius, None);
        assert!(dto.host_history.is_empty());
    }

    #[test]
    fn stats_report_dto_roundtrips_with_history_and_temp() {
        let report = StatsReport {
            host: HostStats {
                cpu_percent: 9.0,
                mem_used_bytes: 10,
                mem_total_bytes: 20,
                disk_used_percent: 3,
                uptime_secs: 7,
                temp_celsius: Some(50.5),
            },
            projects: vec![],
            host_history: vec![HostSample {
                at_ms: 1,
                cpu_percent: 1.0,
                mem_used_bytes: 4,
                mem_total_bytes: 8,
                temp_celsius: None,
            }],
        };
        let dto: StatsReportDto = report.into();
        let json = serde_json::to_string(&dto).unwrap();
        let back: StatsReportDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back.host.temp_celsius, Some(50.5));
        assert_eq!(back.host_history.len(), 1);
        assert_eq!(back.host_history[0].mem_total_bytes, 8);
    }

    #[test]
    fn service_stats_dto_defaults_missing_state_and_health() {
        let json = r#"{"service":"web","cpu_percent":0.0,"mem_used_bytes":10,"mem_limit_bytes":0}"#;
        let dto: ServiceStatsDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.state, "");
        assert_eq!(dto.health, None);
    }

    #[test]
    fn deploy_request_environment_roundtrips_and_is_optional() {
        let json = r#"{"project":{"name":"myapp--test","repo":"r","branch":"b","compose":"docker-compose.yml","service":"web","port":3000,"hostname":null},"ref":null,"environment":{"env":"test","base":"myapp"}}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        let env = req.environment.unwrap();
        assert_eq!(env.env, "test");
        assert_eq!(env.base, "myapp");
        assert_eq!(env.slug, None);
        // absent block (old CLI) still deserializes
        let json = r#"{"project":{"name":"myapp","repo":"r","branch":"b","compose":"c","service":"web","port":3000,"hostname":null},"ref":null}"#;
        let req: DeployRequest = serde_json::from_str(json).unwrap();
        assert!(req.environment.is_none());
    }
}

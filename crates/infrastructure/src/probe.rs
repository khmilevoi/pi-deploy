use std::sync::Arc;

use async_trait::async_trait;
use pi_domain::contracts::{DiskProbe, ProjectRepository, SystemProbe};
use pi_domain::entities::{AgentOverview, DiagnosticCheck, DiagnosticReport};
use pi_domain::error::DomainError;
use tokio::process::Command;

#[async_trait]
pub trait ProbeRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;

    /// Like `run`, but with extra environment variables set on the child
    /// process. Defaults to plain `run` (env ignored) so existing test
    /// mocks don't need to implement it.
    async fn run_with_env(
        &self,
        program: &str,
        args: &[&str],
        _envs: &[(&str, String)],
    ) -> Result<String, String> {
        self.run(program, args).await
    }
}

pub struct SystemRunner;

impl SystemRunner {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, String)],
    ) -> Result<String, String> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| format!("spawn {program}: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

#[async_trait]
impl ProbeRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.exec(program, args, &[]).await
    }

    async fn run_with_env(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, String)],
    ) -> Result<String, String> {
        self.exec(program, args, envs).await
    }
}

/// Pure decision for the doctor `memory cgroup` check. `controllers` is the
/// contents of `/sys/fs/cgroup/cgroup.controllers` (cgroup v2), `v1_present`
/// is whether `/sys/fs/cgroup/memory` exists (cgroup v1).
#[allow(dead_code)]
fn memory_cgroup_check(controllers: Option<String>, v1_present: bool) -> DiagnosticCheck {
    let v2_ok = controllers
        .as_deref()
        .map(|c| c.split_whitespace().any(|t| t == "memory"))
        .unwrap_or(false);
    let passed = v2_ok || v1_present;
    DiagnosticCheck {
        name: "memory cgroup".into(),
        passed,
        detail: if passed {
            "memory accounting enabled".into()
        } else {
            "memory cgroup controller disabled — per-container memory reports 0".into()
        },
        hint: (!passed).then(|| {
            "enable cgroup memory accounting: add 'cgroup_enable=memory cgroup_memory=1' to \
             /boot/cmdline.txt (or firmware/cmdline.txt) and reboot"
                .into()
        }),
    }
}

/// The agent's data dir (`data_dir` in agent.toml, `/var/lib/rpi` by default)
/// holds every project's plaintext secrets. It must be owned by the agent and
/// no wider than 0750; the file modes inside it are deliberately
/// container-readable, so this directory is what keeps other local users out.
/// `data_dir` is named in the `detail` string (rather than assumed to be the
/// default path) because it is configurable — stat-ing the wrong directory
/// would report green over a real secrets tree that is actually too wide.
fn data_dir_permissions_check(data_dir: &str, stat: Result<String, String>) -> DiagnosticCheck {
    let hint = "tighten the agent data dir: sudo rpi agent setup";
    match stat {
        Ok(out) => {
            let mut parts = out.split_whitespace();
            let owner = parts.next().unwrap_or_default().to_string();
            let mode = parts.next().unwrap_or_default().to_string();
            let too_wide = match u32::from_str_radix(&mode, 8) {
                Ok(bits) => bits & !0o750 != 0,
                Err(_) => true,
            };
            let passed = owner == "rpi-agent" && !too_wide;
            DiagnosticCheck {
                name: "data dir permissions".into(),
                passed,
                detail: format!("{data_dir}: owner {owner}, mode {mode}"),
                hint: if passed { None } else { Some(hint.into()) },
            }
        }
        Err(err) => DiagnosticCheck {
            name: "data dir permissions".into(),
            passed: false,
            detail: err,
            hint: Some(hint.into()),
        },
    }
}

pub struct HostSystemProbe {
    runner: Arc<dyn ProbeRunner>,
    disk: Arc<dyn DiskProbe>,
    projects: Arc<dyn ProjectRepository>,
    version: String,
    /// The agent's configured data dir (`config.data_dir`), stat'd by the
    /// "data dir permissions" check below. Not assumed to be `/var/lib/rpi`:
    /// that is only the default, and `data_dir` is an ordinary overridable
    /// agent.toml field.
    data_dir: String,
    disk_threshold_percent: u8,
    cloudflared_enabled: bool,
    ingress_active: bool,
    cloudflared_config: Option<String>,
    started_at: i64,
}

impl HostSystemProbe {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runner: Arc<dyn ProbeRunner>,
        disk: Arc<dyn DiskProbe>,
        projects: Arc<dyn ProjectRepository>,
        version: String,
        data_dir: String,
        disk_threshold_percent: u8,
        cloudflared_enabled: bool,
        ingress_active: bool,
        cloudflared_config: Option<String>,
        started_at: i64,
    ) -> Arc<HostSystemProbe> {
        Arc::new(HostSystemProbe {
            runner,
            disk,
            projects,
            version,
            data_dir,
            disk_threshold_percent,
            cloudflared_enabled,
            ingress_active,
            cloudflared_config,
            started_at,
        })
    }

    async fn command_check(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        hint: &str,
    ) -> DiagnosticCheck {
        self.command_check_with_env(name, program, args, &[], hint)
            .await
    }

    /// Same as `command_check`, but with extra environment variables set on
    /// the probed command. Needed for `systemctl --user` checks: the agent's
    /// own process environment may lack XDG_RUNTIME_DIR/DBUS_SESSION_BUS_ADDRESS
    /// even when the user unit is healthy, which would otherwise report a
    /// false failure (see the same env fixup in `cloudflared::restart_extra_env`).
    async fn command_check_with_env(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        envs: &[(&str, String)],
        hint: &str,
    ) -> DiagnosticCheck {
        match self.runner.run_with_env(program, args, envs).await {
            Ok(out) => DiagnosticCheck {
                name: name.into(),
                passed: true,
                detail: out.lines().next().unwrap_or("ok").to_string(),
                hint: None,
            },
            Err(err) => DiagnosticCheck {
                name: name.into(),
                passed: false,
                detail: err,
                hint: Some(hint.into()),
            },
        }
    }
}

#[async_trait]
impl SystemProbe for HostSystemProbe {
    async fn diagnostics(&self) -> DiagnosticReport {
        let mut checks = vec![
            self.command_check(
                "docker daemon",
                "docker",
                &["version", "--format", "{{.Server.Version}}"],
                "start Docker and make sure the rpi-agent user can access it",
            )
            .await,
            self.command_check(
                "docker compose",
                "docker",
                &["compose", "version"],
                "install Docker Compose v2",
            )
            .await,
        ];

        checks.push(match self.runner.run("id", &["-nG"]).await {
            Ok(out) => {
                let in_docker_group = out.split_whitespace().any(|g| g == "docker");
                DiagnosticCheck {
                    name: "rpi-agent group".into(),
                    passed: in_docker_group,
                    detail: format!("groups: {out}"),
                    hint: if !in_docker_group {
                        Some("add rpi-agent to the 'docker' group: sudo usermod -aG docker rpi-agent".into())
                    } else {
                        None
                    },
                }
            }
            Err(err) => DiagnosticCheck {
                name: "rpi-agent group".into(),
                passed: false,
                detail: err,
                hint: Some("add rpi-agent to the 'docker' group".into()),
            },
        });

        checks.push(data_dir_permissions_check(
            &self.data_dir,
            self.runner
                .run("stat", &["-c", "%U %a", &self.data_dir])
                .await,
        ));

        checks.push(
            match self
                .runner
                .run("loginctl", &["show-user", "-P", "Linger", "rpi-agent"])
                .await
            {
                Ok(out) => {
                    let linger_enabled = out == "yes";
                    DiagnosticCheck {
                        name: "systemd linger".into(),
                        passed: linger_enabled,
                        detail: format!("Linger={out}"),
                        hint: if !linger_enabled {
                            Some("enable linger: loginctl enable-linger rpi-agent".into())
                        } else {
                            None
                        },
                    }
                }
                Err(err) => DiagnosticCheck {
                    name: "systemd linger".into(),
                    passed: false,
                    detail: err,
                    hint: Some("enable linger: loginctl enable-linger rpi-agent".into()),
                },
            },
        );

        if self.cloudflared_enabled {
            checks.push(
                self.command_check(
                    "cloudflared",
                    "cloudflared",
                    &["--version"],
                    "install cloudflared or disable [cloudflared] routing",
                )
                .await,
            );
            let current_runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
            let user_bus_env = crate::cloudflared::restart_extra_env(
                current_runtime_dir.as_deref(),
                crate::cloudflared::current_uid(),
            );
            checks.push(
                self.command_check_with_env(
                    "cloudflared service",
                    "systemctl",
                    &["--user", "is-active", "cloudflared"],
                    &user_bus_env,
                    "enable and start cloudflared service: systemctl --user enable --now cloudflared",
                )
                .await,
            );
        }

        if !self.ingress_active {
            if let Ok(projects) = self.projects.list().await {
                let hostnames: Vec<String> = projects
                    .iter()
                    .filter_map(|p| p.config.hostname.clone())
                    .collect();
                if !hostnames.is_empty() {
                    checks.push(DiagnosticCheck {
                        name: "ingress routing".into(),
                        passed: false,
                        detail: format!(
                            "hostname(s) declared but automatic ingress is disabled: {}",
                            hostnames.join(", ")
                        ),
                        hint: Some(
                            "enable it: sudo rpi agent setup --with-cloudflared \
                             --cf-token-file <path> --domain <zone>"
                                .into(),
                        ),
                    });
                }
            }
        }

        if self.ingress_active {
            // (a) connector-alive: ingress configured but no cloudflared process.
            match self.runner.run("pgrep", &["-x", "cloudflared"]).await {
                Ok(_) => {}                             // connector up — healthy
                Err(e) if e.starts_with("spawn ") => {} // pgrep unavailable — can't tell, skip
                Err(_) => checks.push(DiagnosticCheck {
                    name: "cloudflared connector".into(),
                    passed: false,
                    detail: "ingress is configured but no cloudflared process is running".into(),
                    hint: Some(
                        "start it: sudo -u rpi-agent XDG_RUNTIME_DIR=/run/user/<uid> \
                         systemctl --user start cloudflared (or check its logs)"
                            .into(),
                    ),
                }),
            }

            // (b) route-missing: declared hostname with no route in config.yml.
            if let Some(path) = &self.cloudflared_config {
                if let Ok(text) = self.runner.run("cat", &[path]).await {
                    if let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
                        let routed: std::collections::HashSet<String> = doc
                            .get("ingress")
                            .and_then(|v| v.as_sequence())
                            .map(|rules| {
                                rules
                                    .iter()
                                    .filter_map(|r| {
                                        r.get("hostname").and_then(|h| h.as_str()).map(String::from)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if let Ok(projects) = self.projects.list().await {
                            let missing: Vec<String> = projects
                                .iter()
                                .filter_map(|p| p.config.hostname.clone())
                                .filter(|h| !routed.contains(h))
                                .collect();
                            if !missing.is_empty() {
                                checks.push(DiagnosticCheck {
                                    name: "ingress route".into(),
                                    passed: false,
                                    detail: format!(
                                        "hostname(s) declared with a running ingress but no route in config.yml: {}",
                                        missing.join(", ")
                                    ),
                                    hint: Some(
                                        "re-deploy the project(s) to (re)create the route, or check config.yml"
                                            .into(),
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        checks.push(match self.disk.used_percent() {
            Ok(percent) => DiagnosticCheck {
                name: "disk space".into(),
                passed: percent < self.disk_threshold_percent,
                detail: format!("{percent}% used"),
                hint: (percent >= self.disk_threshold_percent)
                    .then(|| "run `rpi gc` or free disk space".into()),
            },
            Err(err) => DiagnosticCheck {
                name: "disk space".into(),
                passed: false,
                detail: err.to_string(),
                hint: Some("check agent data directory mount".into()),
            },
        });

        #[cfg(target_os = "linux")]
        {
            let controllers = std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").ok();
            let v1_present = std::path::Path::new("/sys/fs/cgroup/memory").exists();
            checks.push(memory_cgroup_check(controllers, v1_present));
        }

        DiagnosticReport { checks }
    }

    async fn overview(&self) -> Result<AgentOverview, DomainError> {
        let projects = self.projects.list().await?.len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let uptime_secs = (now - self.started_at).max(0) as u64;

        Ok(AgentOverview {
            version: self.version.clone(),
            uptime_secs,
            disk_used_percent: self.disk.used_percent()?,
            projects,
            active_deployments: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_domain::contracts::{MockDiskProbe, MockProjectRepository, SystemProbe};
    use pi_domain::entities::{
        ExposeMode, HealthcheckConfig, Project, ProjectConfig, StageTimeoutOverrides,
    };

    struct FakeRunner;
    #[async_trait]
    impl ProbeRunner for FakeRunner {
        async fn run(&self, _program: &str, _args: &[&str]) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    fn project(hostname: Option<&str>) -> Project {
        Project {
            config: ProjectConfig {
                name: "app".into(),
                repo: "r".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 80,
                hostname: hostname.map(String::from),
                expose: ExposeMode::default(),
                healthcheck: HealthcheckConfig::default(),
                timeouts: StageTimeoutOverrides::default(),
                commands: Default::default(),
                command_timeout_secs: None,
                environment: None,
            },
            host_port: 8002,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
            last_commit_sha: None,
        }
    }

    fn probe(ingress_active: bool, projects: Vec<Project>) -> Arc<HostSystemProbe> {
        let mut repo = MockProjectRepository::new();
        repo.expect_list().return_once(move || Ok(projects));
        let mut disk = MockDiskProbe::new();
        disk.expect_used_percent().returning(|| Ok(10));
        HostSystemProbe::new(
            Arc::new(FakeRunner),
            Arc::new(disk),
            Arc::new(repo),
            "0.0.0".into(),
            "/var/lib/rpi".into(),
            85,
            false, // cloudflared binary/service checks off — not under test
            ingress_active,
            None,
            0,
        )
    }

    struct ScriptedRunner(std::collections::HashMap<String, Result<String, String>>);

    #[async_trait]
    impl ProbeRunner for ScriptedRunner {
        async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.0.get(&key).cloned().unwrap_or_else(|| Ok("ok".into()))
        }
    }

    fn probe_with(
        runner: Arc<dyn ProbeRunner>,
        ingress_active: bool,
        cloudflared_config: Option<String>,
        projects: Vec<Project>,
    ) -> Arc<HostSystemProbe> {
        probe_with_cloudflared(runner, false, ingress_active, cloudflared_config, projects)
    }

    #[allow(clippy::too_many_arguments)]
    fn probe_with_cloudflared(
        runner: Arc<dyn ProbeRunner>,
        cloudflared_enabled: bool,
        ingress_active: bool,
        cloudflared_config: Option<String>,
        projects: Vec<Project>,
    ) -> Arc<HostSystemProbe> {
        let mut repo = MockProjectRepository::new();
        repo.expect_list().returning(move || Ok(projects.clone()));
        let mut disk = MockDiskProbe::new();
        disk.expect_used_percent().returning(|| Ok(10));
        HostSystemProbe::new(
            runner,
            Arc::new(disk),
            Arc::new(repo),
            "0.0.0".into(),
            "/var/lib/rpi".into(),
            85,
            cloudflared_enabled,
            ingress_active,
            cloudflared_config,
            0,
        )
    }

    type RecordedCall = (String, Vec<(String, String)>);

    struct RecordingRunner {
        seen_envs: std::sync::Mutex<Vec<RecordedCall>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                seen_envs: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ProbeRunner for RecordingRunner {
        async fn run(&self, _program: &str, _args: &[&str]) -> Result<String, String> {
            Ok("active".into())
        }

        async fn run_with_env(
            &self,
            program: &str,
            args: &[&str],
            envs: &[(&str, String)],
        ) -> Result<String, String> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.seen_envs.lock().unwrap().push((
                key,
                envs.iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
            Ok("active".into())
        }
    }

    #[tokio::test]
    async fn cloudflared_service_check_passes_the_same_user_bus_env_as_restart() {
        let runner = Arc::new(RecordingRunner::new());
        let report = probe_with_cloudflared(runner.clone(), true, true, None, vec![])
            .diagnostics()
            .await;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "cloudflared service")
            .expect("cloudflared service check present");
        assert!(check.passed, "expected the check to succeed: {check:?}");

        let expected = crate::cloudflared::restart_extra_env(
            std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
            crate::cloudflared::current_uid(),
        )
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect::<Vec<_>>();

        let seen = runner.seen_envs.lock().unwrap();
        let call = seen
            .iter()
            .find(|(key, _)| key == "systemctl --user is-active cloudflared")
            .expect("systemctl --user is-active cloudflared was invoked");
        assert_eq!(call.1, expected);
    }

    #[tokio::test]
    async fn disabled_ingress_with_hostnames_fails_the_ingress_check() {
        let report = probe(false, vec![project(Some("rpi.example.com"))])
            .diagnostics()
            .await;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "ingress routing")
            .expect("ingress routing check present");
        assert!(!check.passed);
        assert!(check.detail.contains("rpi.example.com"));
        assert!(check
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("sudo rpi agent setup --with-cloudflared"));
    }

    #[tokio::test]
    async fn active_ingress_or_no_hostnames_add_no_check() {
        for (active, host) in [(true, Some("a.example.com")), (false, None)] {
            let report = probe(active, vec![project(host)]).diagnostics().await;
            assert!(
                report.checks.iter().all(|c| c.name != "ingress routing"),
                "unexpected check for active={active} host={host:?}"
            );
        }
    }

    #[tokio::test]
    async fn active_ingress_without_running_connector_fails_the_connector_check() {
        let mut responses = std::collections::HashMap::new();
        // pgrep ran, no match (non-"spawn " error) -> connector is down.
        responses.insert("pgrep -x cloudflared".to_string(), Err(String::new()));
        let report = probe_with(Arc::new(ScriptedRunner(responses)), true, None, vec![])
            .diagnostics()
            .await;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "cloudflared connector")
            .expect("connector check present");
        assert!(!check.passed);
        assert!(check.detail.contains("no cloudflared process is running"));
    }

    #[tokio::test]
    async fn active_ingress_with_running_connector_adds_no_connector_check() {
        let mut responses = std::collections::HashMap::new();
        responses.insert("pgrep -x cloudflared".to_string(), Ok("4321".to_string()));
        let report = probe_with(Arc::new(ScriptedRunner(responses)), true, None, vec![])
            .diagnostics()
            .await;
        assert!(report
            .checks
            .iter()
            .all(|c| c.name != "cloudflared connector"));
    }

    #[tokio::test]
    async fn connector_check_skipped_when_pgrep_unavailable() {
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "pgrep -x cloudflared".to_string(),
            Err("spawn pgrep: No such file or directory".to_string()),
        );
        let report = probe_with(Arc::new(ScriptedRunner(responses)), true, None, vec![])
            .diagnostics()
            .await;
        assert!(report
            .checks
            .iter()
            .all(|c| c.name != "cloudflared connector"));
    }

    #[tokio::test]
    async fn active_ingress_missing_route_fails_the_route_check() {
        let config = "tunnel: t\ningress:\n  - hostname: a.example.com\n    service: http://127.0.0.1:8001\n  - service: http_status:404\n";
        let mut responses = std::collections::HashMap::new();
        // connector up, so the route check is what fires (isolate it).
        responses.insert("pgrep -x cloudflared".to_string(), Ok("4321".to_string()));
        responses.insert(
            "cat /etc/rpi/config.yml".to_string(),
            Ok(config.to_string()),
        );
        let report = probe_with(
            Arc::new(ScriptedRunner(responses)),
            true,
            Some("/etc/rpi/config.yml".into()),
            vec![
                project(Some("a.example.com")),
                project(Some("b.example.com")),
            ],
        )
        .diagnostics()
        .await;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "ingress route")
            .expect("ingress route check present");
        assert!(!check.passed);
        assert!(check.detail.contains("b.example.com"));
        assert!(
            !check.detail.contains("a.example.com"),
            "routed hostname must not be listed"
        );
    }

    #[tokio::test]
    async fn route_check_skipped_when_config_absent_or_unreadable() {
        // config path is None -> route check skipped.
        let report = probe_with(
            Arc::new(ScriptedRunner(std::collections::HashMap::new())),
            true,
            None,
            vec![project(Some("a.example.com"))],
        )
        .diagnostics()
        .await;
        assert!(report.checks.iter().all(|c| c.name != "ingress route"));

        // config path set but `cat` fails -> route check skipped silently.
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "cat /etc/rpi/config.yml".to_string(),
            Err("No such file".to_string()),
        );
        let report = probe_with(
            Arc::new(ScriptedRunner(responses)),
            true,
            Some("/etc/rpi/config.yml".into()),
            vec![project(Some("a.example.com"))],
        )
        .diagnostics()
        .await;
        assert!(report.checks.iter().all(|c| c.name != "ingress route"));
    }

    #[test]
    fn memory_cgroup_v2_passes_when_controllers_list_memory() {
        let check = memory_cgroup_check(Some("cpuset cpu io memory pids\n".into()), false);
        assert!(check.passed);
        assert!(check.hint.is_none());
    }

    #[test]
    fn memory_cgroup_v2_fails_when_memory_absent_and_no_v1() {
        let check = memory_cgroup_check(Some("cpuset cpu io pids\n".into()), false);
        assert!(!check.passed);
        assert!(check
            .hint
            .as_deref()
            .unwrap()
            .contains("cgroup_enable=memory cgroup_memory=1"));
    }

    #[test]
    fn memory_cgroup_v1_passes_when_dir_present() {
        let check = memory_cgroup_check(None, true);
        assert!(check.passed);
    }

    #[test]
    fn memory_cgroup_fails_when_neither_present() {
        let check = memory_cgroup_check(None, false);
        assert!(!check.passed);
        assert_eq!(check.name, "memory cgroup");
    }

    #[test]
    fn data_dir_permissions_check_fails_on_a_world_readable_dir() {
        let check = data_dir_permissions_check("/var/lib/rpi", Ok("rpi-agent 755".into()));
        assert!(!check.passed);
        assert!(check.hint.unwrap().contains("rpi agent setup"));
    }

    #[test]
    fn data_dir_permissions_check_passes_at_0750() {
        assert!(data_dir_permissions_check("/var/lib/rpi", Ok("rpi-agent 750".into())).passed);
    }

    #[test]
    fn data_dir_permissions_check_passes_when_tighter() {
        assert!(data_dir_permissions_check("/var/lib/rpi", Ok("rpi-agent 700".into())).passed);
    }

    #[test]
    fn data_dir_permissions_check_fails_on_a_group_writable_dir() {
        // Ends in '0' (others), so a naive last-digit check would call this
        // healthy — but group-write matters: `rpi setup` puts the login user
        // in the `rpi-agent` group, so this is a real escalation path.
        let check = data_dir_permissions_check("/var/lib/rpi", Ok("rpi-agent 770".into()));
        assert!(!check.passed);
        assert!(check.hint.unwrap().contains("rpi agent setup"));
    }

    #[test]
    fn data_dir_permissions_check_fails_on_an_unparseable_mode() {
        let check = data_dir_permissions_check("/var/lib/rpi", Ok("rpi-agent garbage".into()));
        assert!(!check.passed);
        assert!(check.hint.unwrap().contains("rpi agent setup"));
    }

    #[test]
    fn data_dir_permissions_check_names_a_custom_data_dir_in_its_detail() {
        // A custom `data_dir` (agent.toml is not pinned to /var/lib/rpi) must
        // be reflected in the check's own detail string, not a hardcoded
        // default — otherwise the check could stat the wrong directory
        // entirely and still print a path that looks right.
        let check = data_dir_permissions_check("/srv/rpi-data", Ok("rpi-agent 750".into()));
        assert!(check.passed);
        assert!(
            check.detail.contains("/srv/rpi-data"),
            "detail must name the configured data dir: {}",
            check.detail
        );
    }

    #[tokio::test]
    async fn diagnostics_stats_the_configured_data_dir_not_a_hardcoded_default() {
        // Regression guard: the probe must stat whatever `data_dir` it was
        // built with, so a project running against a custom data dir gets a
        // real permissions check instead of one that silently inspects
        // /var/lib/rpi (which may not even be the directory in use).
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "stat -c %U %a /srv/rpi-data".to_string(),
            Ok("rpi-agent 750".to_string()),
        );
        let mut repo = MockProjectRepository::new();
        repo.expect_list().returning(|| Ok(vec![]));
        let mut disk = MockDiskProbe::new();
        disk.expect_used_percent().returning(|| Ok(10));
        let probe = HostSystemProbe::new(
            Arc::new(ScriptedRunner(responses)),
            Arc::new(disk),
            Arc::new(repo),
            "0.0.0".into(),
            "/srv/rpi-data".into(),
            85,
            false,
            false,
            None,
            0,
        );
        let report = probe.diagnostics().await;
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "data dir permissions")
            .expect("data dir permissions check present");
        assert!(check.passed, "expected pass: {check:?}");
        assert!(check.detail.contains("/srv/rpi-data"));
    }
}

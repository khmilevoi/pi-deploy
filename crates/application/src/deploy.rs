use std::sync::Arc;

use pi_domain::contracts::{
    Clock, ContainerRuntime, DeploymentHistory, HealthGate, HostNetwork, Ingress, IngressOutcome,
    LogSink, OverrideStore, ProjectRepository, SecretStore, SecretsWriter, Source,
};
use pi_domain::entities::{
    ComposeStack, DeployRef, Deployment, DeploymentStatus, ExposeMode, ProjectConfig, StageEvent,
    StageTimeouts,
};
use pi_domain::error::DomainError;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::gc::RunGc;
use crate::mask::MaskingSink;
use crate::tail::TailSink;

const LOG_TAIL_LINES: usize = 400;
const GC_TIMEOUT_SECS: u64 = 300;

/// Guarantees sending `finished(Failed)` on drop if `disarm()` is not called.
/// Protects against panics and early returns via `?`.
struct FinishGuard {
    sink: Arc<dyn LogSink>,
    armed: bool,
}

impl FinishGuard {
    fn new(sink: Arc<dyn LogSink>) -> Self {
        Self { sink, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if self.armed {
            self.sink.finished(DeploymentStatus::Failed);
        }
    }
}

/// Deploy use-case (§7, §8.2). Per-project serialization is owned by DeployScheduler.
pub struct DeployProject {
    source: Arc<dyn Source>,
    runtime: Arc<dyn ContainerRuntime>,
    projects: Arc<dyn ProjectRepository>,
    history: Arc<dyn DeploymentHistory>,
    overrides: Arc<dyn OverrideStore>,
    secrets: Arc<dyn SecretStore>,
    secrets_writer: Arc<dyn SecretsWriter>,
    health: Arc<dyn HealthGate>,
    ingress: Arc<dyn Ingress>,
    host_network: Arc<dyn HostNetwork>,
    clock: Arc<dyn Clock>,
    gc: Arc<RunGc>,
    timeouts: StageTimeouts,
    /// §8.1: global build semaphore — parallel builds OOM the Pi.
    build_sem: Semaphore,
}

/// Wraps a deploy stage with its timeout (§8.1). On expiry the stage future is
/// dropped — kill_on_drop in the process adapter kills the child — and the
/// deploy fails as `timeout: <stage>`.
async fn staged<T>(
    stage: &str,
    secs: u64,
    fut: impl std::future::Future<Output = Result<T, DomainError>>,
) -> Result<T, DomainError> {
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(result) => result,
        Err(_) => Err(DomainError::Timeout {
            stage: stage.to_string(),
            secs,
        }),
    }
}

/// Emits `started` / `ok` / `failed` stage events around a deploy stage,
/// measuring wall-clock elapsed. The stage name is the wire name the CLI
/// renders (deploy-stages spec).
async fn tracked<T>(
    log: &Arc<dyn LogSink>,
    stage: &str,
    fut: impl std::future::Future<Output = Result<T, DomainError>>,
) -> Result<T, DomainError> {
    log.stage(&StageEvent::started(stage));
    let t0 = std::time::Instant::now();
    match fut.await {
        Ok(v) => {
            log.stage(&StageEvent::ok(stage, t0.elapsed()));
            Ok(v)
        }
        Err(e) => {
            log.stage(&StageEvent::failed(stage, t0.elapsed()));
            Err(e)
        }
    }
}

impl DeployProject {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: Arc<dyn Source>,
        runtime: Arc<dyn ContainerRuntime>,
        projects: Arc<dyn ProjectRepository>,
        history: Arc<dyn DeploymentHistory>,
        overrides: Arc<dyn OverrideStore>,
        secrets: Arc<dyn SecretStore>,
        secrets_writer: Arc<dyn SecretsWriter>,
        health: Arc<dyn HealthGate>,
        ingress: Arc<dyn Ingress>,
        host_network: Arc<dyn HostNetwork>,
        clock: Arc<dyn Clock>,
        gc: Arc<RunGc>,
        timeouts: StageTimeouts,
        build_slots: usize,
    ) -> Arc<DeployProject> {
        Arc::new(DeployProject {
            source,
            runtime,
            projects,
            history,
            overrides,
            secrets,
            secrets_writer,
            health,
            ingress,
            host_network,
            clock,
            gc,
            timeouts,
            build_sem: Semaphore::new(build_slots),
        })
    }

    pub async fn execute(
        &self,
        deployment_id: String,
        config: ProjectConfig,
        git_ref: DeployRef,
        sink: Arc<dyn LogSink>,
        cancel: CancellationToken,
    ) -> Result<Deployment, DomainError> {
        // chain: stages write to masker → masks secrets → tail stores masked lines → sink (SSE hub)
        let tail = TailSink::new(Arc::clone(&sink), LOG_TAIL_LINES);
        let masker = MaskingSink::new(tail.clone());
        let log: Arc<dyn LogSink> = masker.clone();
        let mut guard = FinishGuard::new(sink);

        let started_at = self.clock.now_unix();
        if let Err(err) = self.history.mark_running(&deployment_id, started_at).await {
            // DB record stays queued without this; write a Failed terminal row so
            // the record is consistent without waiting for the next startup sweep.
            let _ = self
                .history
                .record_finished(
                    &deployment_id,
                    DeploymentStatus::Failed,
                    None,
                    started_at,
                    &err.to_string(),
                )
                .await;
            return Err(err);
        }
        let mut deployment = Deployment {
            id: deployment_id,
            project: config.name.clone(),
            git_ref: git_ref.as_str().to_string(),
            commit_sha: None,
            status: DeploymentStatus::Running,
            started_at,
            finished_at: None,
            log_tail: String::new(),
        };

        let result = tokio::select! {
            _ = cancel.cancelled() => Err(DomainError::Canceled),
            r = self.run_stages(&config, &git_ref, log.clone(), &masker) => r,
        };
        let finished_at = self.clock.now_unix();

        match result {
            Ok(commit_sha) => {
                deployment.status = DeploymentStatus::Success;
                deployment.commit_sha = Some(commit_sha);
                deployment.finished_at = Some(finished_at);
                deployment.log_tail = tail.tail();
                if let Err(err) = self
                    .projects
                    .mark_deploy_success(
                        &config.name,
                        finished_at,
                        deployment.commit_sha.as_deref(),
                    )
                    .await
                {
                    log.line(&format!(
                        "warning: could not record deploy success time: {err}"
                    ));
                }
                let record_result = self
                    .history
                    .record_finished(
                        &deployment.id,
                        DeploymentStatus::Success,
                        deployment.commit_sha.as_deref(),
                        finished_at,
                        &deployment.log_tail,
                    )
                    .await;
                log.finished(DeploymentStatus::Success);
                guard.disarm();
                record_result?;
                Ok(deployment)
            }
            Err(err) => {
                let status = if matches!(err, DomainError::Canceled) {
                    DeploymentStatus::Canceled
                } else {
                    DeploymentStatus::Failed
                };
                log.line(&format!("deploy {}: {err}", status.as_str()));
                let log_tail = tail.tail();
                let record_result = self
                    .history
                    .record_finished(&deployment.id, status, None, finished_at, &log_tail)
                    .await;
                log.finished(status);
                guard.disarm();
                record_result?;
                Err(err)
            }
        }
    }

    async fn run_stages(
        &self,
        config: &ProjectConfig,
        git_ref: &DeployRef,
        log: Arc<dyn LogSink>,
        masker: &MaskingSink,
    ) -> Result<String, DomainError> {
        let timeouts = self.timeouts.with_overrides(&config.timeouts);

        let project = self.projects.upsert(config).await?;
        log.line(&format!(
            "project '{}': host port {}",
            project.config.name, project.host_port
        ));

        let fetched = tracked(
            &log,
            "fetch",
            staged(
                "fetch",
                timeouts.fetch_secs,
                self.source.fetch(config, git_ref, log.clone()),
            ),
        )
        .await?;
        log.line(&format!("fetched {}", fetched.commit_sha));

        // secret-groups spec: declared groups in order, then this deploy
        // key's own bundle on top, then one merged injection.
        let merged = crate::effective_secrets(self.secrets.as_ref(), config).await?;
        // Arm on the merged bundle: a value contributed by any layer must be
        // masked in container output, not only one that arrived in the push.
        masker.arm(&merged.bundle);
        let provenance: Vec<String> = merged
            .revisions
            .iter()
            .map(|(label, r)| format!("{label}@r{r}"))
            .collect();
        let bundle = merged.bundle;
        if !bundle.is_empty() {
            self.secrets_writer.write(&fetched.workdir, &bundle).await?;
            log.line(&format!(
                "secrets injected ({} keys, {} files, mode {:04o}; groups: {})",
                bundle.vars.len(),
                bundle.files.len(),
                bundle.secret_file_mode(),
                provenance.join(", ")
            ));
        }

        // The RPI_* map is derived from registry state plus the sha this
        // deploy just fetched, so the CLI never has to send it.
        let env = pi_domain::runtimevars::rpi_vars(&project, Some(&fetched.commit_sha));

        // Services are discovered before the override is written, from the
        // compose file plus the repository's own override only. A failure
        // here fails the deploy: a silently missing environment variable is a
        // far worse outcome than an explicit error, and a compose file that
        // cannot be parsed would fail `build` moments later anyway.
        let discovery = ComposeStack {
            project_name: config.name.clone(),
            workdir: fetched.workdir.clone(),
            compose_file: fetched.workdir.join(&config.compose_path),
            override_file: self.overrides.path(&config.name),
            env: env.clone(),
        };
        let services = self.runtime.services(&discovery).await?;
        log.line(&format!(
            "runtime env: {} vars over {} service(s)",
            env.len(),
            services.len()
        ));

        let override_file = self
            .overrides
            .write(
                &config.name,
                &config.service,
                config.expose.bind_addr(),
                project.host_port,
                config.container_port,
                &services,
                &env,
            )
            .await?;

        let stack = ComposeStack {
            project_name: config.name.clone(),
            workdir: fetched.workdir.clone(),
            compose_file: fetched.workdir.join(&config.compose_path),
            override_file,
            env,
        };
        {
            let _build_slot = self
                .build_sem
                .acquire()
                .await
                .map_err(|_| DomainError::Runtime("build semaphore closed".into()))?;
            tracked(
                &log,
                "build",
                staged(
                    "build",
                    timeouts.build_secs,
                    self.runtime.build(&stack, log.clone()),
                ),
            )
            .await?;
        }
        tracked(
            &log,
            "start",
            staged(
                "start",
                timeouts.up_secs,
                self.runtime.up(&stack, log.clone()),
            ),
        )
        .await?;

        // §8: health gate — on failure the deploy is failed, stack stays up
        tracked(
            &log,
            "health",
            self.health.check(config, project.host_port, log.clone()),
        )
        .await?;

        // deploy-stages spec: service count for the CLI stamp; ps failure is
        // non-fatal — the summary is simply not sent.
        if let Ok(services) = self.runtime.ps(&config.name).await {
            log.summary(services.len());
        }

        if config.expose == ExposeMode::Lan {
            let hn = Arc::clone(&self.host_network);
            let ip = tokio::task::spawn_blocking(move || hn.primary_ipv4())
                .await
                .ok()
                .flatten();
            match ip {
                Some(ip) => {
                    log.line(&format!("lan: http://{ip}:{}", project.host_port));
                    if !is_private_ipv4(&ip) {
                        let msg = format!(
                            "lan: host ip {ip} is public (not RFC1918); expose=lan binds 0.0.0.0 \
                             and publishes the service to the public internet, bypassing host \
                             firewalls (Docker manages its own iptables chain)"
                        );
                        log.line(&msg);
                    }
                }
                None => log.line(&format!("lan: {} (ip not detected)", project.host_port)),
            }
        }

        // §11: route hostname only when configured
        let mut ingress_warning: Option<String> = None;
        if let Some(hostname) = &config.hostname {
            log.stage(&StageEvent::started("route"));
            let t0 = std::time::Instant::now();
            match self
                .ingress
                .upsert(hostname, project.host_port, log.clone())
                .await
            {
                Ok(IngressOutcome::Applied) => {
                    log.stage(&StageEvent::ok("route", t0.elapsed()));
                }
                Ok(IngressOutcome::Skipped) => {
                    log.stage(&StageEvent::skipped("route", t0.elapsed()));
                    ingress_warning = Some(format!(
                        "warning: hostname {hostname} is declared but ingress is disabled \
                         on the agent; the app is not publicly reachable — enable it with: \
                         sudo rpi agent setup --with-cloudflared --cf-token-file <path> --domain <zone>"
                    ));
                }
                Err(e) => {
                    log.stage(&StageEvent::failed("route", t0.elapsed()));
                    return Err(e);
                }
            }
        }

        // environments spec: on_create runs once, after the first fully successful
        // deploy of the key (health + route done). Failure fails the deploy; the
        // flag stays false so the next deploy retries.
        if let Some(env) = &config.environment {
            if let (Some(cmd_name), false) = (&env.on_create, project.on_create_done) {
                let spec = config.commands.get(cmd_name).ok_or_else(|| {
                    DomainError::Invalid(format!(
                        "on_create command '{cmd_name}' is not declared in [commands]"
                    ))
                })?;
                let service = spec
                    .service
                    .clone()
                    .unwrap_or_else(|| config.service.clone());
                let secs = config
                    .command_timeout_secs
                    .unwrap_or(crate::command::DEFAULT_COMMAND_TIMEOUT_SECS);
                let argv = spec.argv.clone();
                tracked(
                    &log,
                    "on_create",
                    staged("on_create", secs, async {
                        let code = self
                            .runtime
                            .exec(&stack, &service, &argv, log.clone())
                            .await?;
                        if code != 0 {
                            return Err(DomainError::Runtime(format!(
                                "on_create '{cmd_name}' exited with code {code}"
                            )));
                        }
                        Ok(())
                    }),
                )
                .await?;
                self.projects.set_on_create_done(&config.name, true).await?;
                log.line(&format!("on_create '{cmd_name}' completed"));
            }
        }

        log.stage(&StageEvent::started("gc"));
        let t0 = std::time::Instant::now();
        match staged("gc", GC_TIMEOUT_SECS, self.gc.execute(log.clone())).await {
            Ok(_) => log.stage(&StageEvent::ok("gc", t0.elapsed())),
            Err(err) => {
                log.stage(&StageEvent::skipped("gc", t0.elapsed()));
                log.line(&format!("gc skipped: {err}"));
            }
        }

        // Emitted last so it sits next to the final summary, not mid-stream.
        if let Some(w) = &ingress_warning {
            log.line(w);
        }

        Ok(fetched.commit_sha)
    }
}

/// True for loopback, link-local, and RFC1918 private IPv4 addresses. Used to
/// warn when `expose = "lan"` would publish a service on a publicly routable
/// IPv4 (the trait only returns IPv4, but an unexpected IPv6 is treated as
/// private to avoid false alarms).
fn is_private_ipv4(ip: &std::net::IpAddr) -> bool {
    let std::net::IpAddr::V4(v4) = ip else {
        return true;
    };
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_link_local()
        || o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        IngressOutcome, MockClock, MockContainerRuntime, MockDeploymentHistory, MockDiskProbe,
        MockHealthGate, MockHostNetwork, MockIngress, MockOverrideStore, MockProjectRepository,
        MockSecretStore, MockSecretsWriter, MockSource,
    };
    use pi_domain::entities::{
        DeployRef, DeploymentStatus, EnvironmentMeta, ExposeMode, FetchedSource, HealthcheckConfig,
        Project, ProjectConfig, SecretsBundle, StageTimeoutOverrides, StageTimeouts,
    };
    use pi_domain::error::DomainError;
    use pi_domain::secretgroup::{GroupRef, SecretGroup};
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    pub fn sample_config() -> ProjectConfig {
        ProjectConfig {
            name: "rateme".into(),
            repo: "git@github.com:isskelo/rateme.git".into(),
            branch: "main".into(),
            compose_path: "docker-compose.yml".into(),
            service: "web".into(),
            container_port: 3000,
            hostname: Some("rateme.isskelo.com".into()),
            expose: ExposeMode::default(),
            healthcheck: HealthcheckConfig::default(),
            timeouts: StageTimeoutOverrides::default(),
            commands: Default::default(),
            command_timeout_secs: None,
            environment: None,
            secret_groups: Vec::new(),
        }
    }

    pub struct Mocks {
        pub source: MockSource,
        pub runtime: MockContainerRuntime,
        pub gc_runtime: MockContainerRuntime,
        pub disk: MockDiskProbe,
        pub projects: MockProjectRepository,
        pub history: MockDeploymentHistory,
        pub overrides: MockOverrideStore,
        pub secrets: MockSecretStore,
        pub secrets_writer: MockSecretsWriter,
        pub health: MockHealthGate,
        pub ingress: MockIngress,
        pub host_network: MockHostNetwork,
        pub clock: MockClock,
    }

    pub fn mocks() -> Mocks {
        let mut clock = MockClock::new();
        clock.expect_now_unix().return_const(100i64);
        let mut gc_runtime = MockContainerRuntime::new();
        gc_runtime.expect_prune_images().returning(|_| Ok(()));
        let mut disk = MockDiskProbe::new();
        disk.expect_used_percent().returning(|| Ok(10));
        let mut projects = MockProjectRepository::new();
        projects
            .expect_mark_deploy_success()
            .returning(|_, _, _| Ok(()));
        Mocks {
            source: MockSource::new(),
            runtime: MockContainerRuntime::new(),
            gc_runtime,
            disk,
            projects,
            history: MockDeploymentHistory::new(),
            overrides: MockOverrideStore::new(),
            secrets: MockSecretStore::new(),
            secrets_writer: MockSecretsWriter::new(),
            health: MockHealthGate::new(),
            ingress: MockIngress::new(),
            host_network: MockHostNetwork::new(),
            clock,
        }
    }

    pub fn build(m: Mocks) -> Arc<DeployProject> {
        let gc = RunGc::new(Arc::new(m.gc_runtime), Arc::new(m.disk), 85);
        DeployProject::new(
            Arc::new(m.source),
            Arc::new(m.runtime),
            Arc::new(m.projects),
            Arc::new(m.history),
            Arc::new(m.overrides),
            Arc::new(m.secrets),
            Arc::new(m.secrets_writer),
            Arc::new(m.health),
            Arc::new(m.ingress),
            Arc::new(m.host_network),
            Arc::new(m.clock),
            gc,
            StageTimeouts::default(),
            1,
        )
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[tokio::test]
    async fn happy_path_runs_all_stages_and_records_success() {
        let mut m = mocks();
        let cfg = sample_config();
        let order = Arc::new(Mutex::new(Vec::new()));

        let stage_order = Arc::clone(&order);
        m.projects.expect_upsert().times(1).returning(move |c| {
            stage_order.lock().unwrap().push("upsert");
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        let stage_order = Arc::clone(&order);
        m.source.expect_fetch().times(1).returning(move |_, _, _| {
            stage_order.lock().unwrap().push("fetch");
            Ok(FetchedSource {
                workdir: PathBuf::from("/var/lib/rpi/workdirs/rateme"),
                commit_sha: SHA.into(),
            })
        });
        let stage_order = Arc::clone(&order);
        m.secrets.expect_load().times(1).returning(move |_| {
            stage_order.lock().unwrap().push("secrets");
            Ok(SecretGroup::default())
        });
        // empty bundle -> .env must NOT be written
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        let stage_order = Arc::clone(&order);
        m.overrides
            .expect_write()
            .withf(|p, s, bind, hp, cp, _, _| {
                p == "rateme" && s == "web" && bind == "127.0.0.1" && *hp == 8000 && *cp == 3000
            })
            .times(1)
            .returning(move |_, _, _, _, _, _, _| {
                stage_order.lock().unwrap().push("override");
                Ok(PathBuf::from("/var/lib/rpi/overrides/rateme.yml"))
            });
        let stage_order = Arc::clone(&order);
        m.runtime
            .expect_build()
            .withf(|stack, _| {
                stack.project_name == "rateme"
                    && stack.compose_file
                        == Path::new("/var/lib/rpi/workdirs/rateme/docker-compose.yml")
                    && stack.override_file == Path::new("/var/lib/rpi/overrides/rateme.yml")
            })
            .times(1)
            .returning(move |_, _| {
                stage_order.lock().unwrap().push("build");
                Ok(())
            });
        let stage_order = Arc::clone(&order);
        m.runtime
            .expect_up()
            .withf(|stack, _| {
                stack.project_name == "rateme"
                    && stack.compose_file
                        == Path::new("/var/lib/rpi/workdirs/rateme/docker-compose.yml")
                    && stack.override_file == Path::new("/var/lib/rpi/overrides/rateme.yml")
            })
            .times(1)
            .returning(move |_, _| {
                stage_order.lock().unwrap().push("up");
                Ok(())
            });
        let stage_order = Arc::clone(&order);
        m.health
            .expect_check()
            .withf(|c, hp, _| c.name == "rateme" && *hp == 8000)
            .times(1)
            .returning(move |_, _, _| {
                stage_order.lock().unwrap().push("health");
                Ok(())
            });
        let stage_order = Arc::clone(&order);
        m.ingress
            .expect_upsert()
            .withf(|h, hp, _| h == "rateme.isskelo.com" && *hp == 8000)
            .times(1)
            .returning(move |_, _, _| {
                stage_order.lock().unwrap().push("ingress");
                Ok(IngressOutcome::Applied)
            });
        m.gc_runtime.checkpoint();
        let stage_order = Arc::clone(&order);
        m.gc_runtime
            .expect_prune_images()
            .times(1)
            .returning(move |_| {
                stage_order.lock().unwrap().push("gc");
                Ok(())
            });
        let stage_order = Arc::clone(&order);
        m.history
            .expect_mark_running()
            .withf(|id, started_at| id == "dep-1" && *started_at == 100)
            .times(1)
            .returning(move |_, _| {
                stage_order.lock().unwrap().push("running");
                Ok(())
            });
        let stage_order = Arc::clone(&order);
        m.history
            .expect_record_finished()
            .withf(|id, status, sha, finished_at, tail| {
                id == "dep-1"
                    && *status == DeploymentStatus::Success
                    && sha == &Some(SHA)
                    && *finished_at == 100
                    && tail.contains("project 'rateme': host port 8000")
                    && tail.contains(&format!("fetched {SHA}"))
            })
            .times(1)
            .returning(move |_, _, _, _, _| {
                stage_order.lock().unwrap().push("finished");
                Ok(())
            });
        m.runtime.expect_ps().times(1).returning(|_| {
            Ok(vec![
                pi_domain::entities::ServiceState {
                    service: "web".into(),
                    state: "running".into(),
                    health: None,
                },
                pi_domain::entities::ServiceState {
                    service: "worker".into(),
                    state: "running".into(),
                    health: None,
                },
            ])
        });

        let deploy = build(m);
        let sink = CollectSink::new();
        let result = deploy
            .execute(
                "dep-1".into(),
                cfg,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert_eq!(result.commit_sha.as_deref(), Some(SHA));
        assert_eq!(result.finished_at, Some(100));
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Success]
        );
        assert_eq!(
            *order.lock().unwrap(),
            vec![
                "running", "upsert", "fetch", "secrets", "override", "build", "up", "health",
                "ingress", "gc", "finished"
            ]
        );

        use pi_domain::entities::StageStatus::{Ok as SOk, Started};
        let stages: Vec<(String, pi_domain::entities::StageStatus)> = sink
            .stages
            .lock()
            .unwrap()
            .iter()
            .map(|e| (e.stage.clone(), e.status))
            .collect();
        assert_eq!(
            stages,
            vec![
                ("fetch".into(), Started),
                ("fetch".into(), SOk),
                ("build".into(), Started),
                ("build".into(), SOk),
                ("start".into(), Started),
                ("start".into(), SOk),
                ("health".into(), Started),
                ("health".into(), SOk),
                ("route".into(), Started),
                ("route".into(), SOk),
                ("gc".into(), Started),
                ("gc".into(), SOk),
            ]
        );
        assert!(
            sink.stages
                .lock()
                .unwrap()
                .iter()
                .all(|e| (e.status == pi_domain::entities::StageStatus::Started)
                    == e.elapsed_ms.is_none()),
            "elapsed_ms present exactly on completions"
        );
        assert_eq!(*sink.summaries.lock().unwrap(), vec![2]);
    }

    #[tokio::test]
    async fn lan_expose_writes_override_bound_to_all_interfaces() {
        let mut m = mocks();
        let mut cfg = sample_config();
        cfg.expose = ExposeMode::Lan;
        cfg.hostname = None;

        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .withf(|p, s, bind, hp, cp, _, _| {
                p == "rateme" && s == "web" && bind == "0.0.0.0" && *hp == 8000 && *cp == 3000
            })
            .times(1)
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);
        m.host_network.expect_primary_ipv4().returning(|| None);
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let result = build(m)
            .execute(
                "dep-lan".into(),
                cfg,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn lan_deploy_logs_reachable_url() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);
        m.host_network
            .expect_primary_ipv4()
            .returning(|| Some("192.168.1.50".parse().unwrap()));

        let mut config = sample_config();
        config.expose = ExposeMode::Lan;
        config.hostname = None;
        let sink = CollectSink::new();

        let result = build(m)
            .execute(
                "dep-lan-url".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert!(sink
            .lines
            .lock()
            .unwrap()
            .contains(&"lan: http://192.168.1.50:8000".to_string()));
    }

    #[tokio::test]
    async fn lan_deploy_logs_port_when_ip_not_detected() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);
        m.host_network.expect_primary_ipv4().returning(|| None);

        let mut config = sample_config();
        config.expose = ExposeMode::Lan;
        config.hostname = None;
        let sink = CollectSink::new();

        let result = build(m)
            .execute(
                "dep-lan-no-ip".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert!(sink
            .lines
            .lock()
            .unwrap()
            .contains(&"lan: 8000 (ip not detected)".to_string()));
    }

    #[tokio::test]
    async fn lan_deploy_warns_when_ip_is_public() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);
        m.host_network
            .expect_primary_ipv4()
            .returning(|| Some("93.184.216.34".parse().unwrap()));

        let mut config = sample_config();
        config.expose = ExposeMode::Lan;
        config.hostname = None;
        let sink = CollectSink::new();

        let result = build(m)
            .execute(
                "dep-lan-public".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        let lines = sink.lines.lock().unwrap();
        assert!(
            lines.contains(&"lan: http://93.184.216.34:8000".to_string()),
            "missing url line in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("93.184.216.34 is public")),
            "missing public-ip warning in {lines:?}"
        );
    }

    #[tokio::test]
    async fn mark_running_failure_writes_failed_to_db_and_emits_finished() {
        let mut m = mocks();
        m.history
            .expect_mark_running()
            .returning(|_, _| Err(DomainError::Storage("db locked".into())));
        m.history
            .expect_record_finished()
            .withf(|id, status, _sha, _at, _tail| {
                id == "dep-mr" && *status == DeploymentStatus::Failed
            })
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        let deploy = build(m);
        let sink = CollectSink::new();
        let err = deploy
            .execute(
                "dep-mr".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Storage(_)));
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Failed]
        );
    }

    #[tokio::test]
    async fn build_failure_records_failed_and_emits_finished_failed() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime
            .expect_build()
            .returning(|_, _| Err(DomainError::Runtime("compose build exited with 1".into())));
        m.runtime.expect_up().times(0);
        m.health.expect_check().times(0);
        m.ingress.expect_upsert().times(0);
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .withf(|id, status, sha, _at, tail| {
                id == "dep-2"
                    && *status == DeploymentStatus::Failed
                    && sha.is_none()
                    && tail.contains("compose build exited with 1")
            })
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        let deploy = build(m);
        let sink = CollectSink::new();
        let err = deploy
            .execute(
                "dep-2".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Runtime(_)));
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Failed]
        );
    }

    fn secret_bundle() -> SecretsBundle {
        let mut b = SecretsBundle::default();
        b.vars.insert("DB_PASSWORD".into(), "hunter2-long".into());
        b.files
            .insert("certs/server.pem".into(), b"PEM-BODY".to_vec());
        b
    }

    fn config_with_groups(groups: Vec<String>) -> ProjectConfig {
        ProjectConfig {
            secret_groups: groups,
            ..sample_config()
        }
    }

    fn set_env_base(config: &mut ProjectConfig, base: &str) {
        config.environment = Some(EnvironmentMeta {
            env: "branch".into(),
            base: base.into(),
            slug: Some("x".into()),
            ttl_secs: None,
            on_create: None,
        });
    }

    fn group_with(vars: &[(&str, &str)]) -> SecretGroup {
        let mut objects = SecretsBundle::default();
        for (k, v) in vars {
            objects.vars.insert((*k).into(), (*v).into());
        }
        SecretGroup {
            objects,
            revision: 1,
        }
    }

    fn ok_pre_stages(m: &mut Mocks) {
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        // The discovery stack is built before `write` returns a path, so the
        // deploy asks the store where the override lives.
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));
    }

    /// `ok_pre_stages` minus the override expectation, for tests that assert
    /// the override is never written.
    fn ok_pre_stages_without_override(m: &mut Mocks) {
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        // The discovery stack is built before `write` returns a path, so the
        // deploy asks the store where the override lives even when the deploy
        // never gets far enough to write it.
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));
    }

    struct HangingSource;

    #[async_trait::async_trait]
    impl pi_domain::contracts::Source for HangingSource {
        fn workdir(&self, project_name: &str) -> PathBuf {
            PathBuf::from("/wd").join(project_name)
        }

        async fn fetch(
            &self,
            _p: &ProjectConfig,
            _r: &DeployRef,
            _l: Arc<dyn LogSink>,
        ) -> Result<FetchedSource, DomainError> {
            std::future::pending().await
        }

        async fn cleanup(&self, _project_name: &str) -> Result<(), DomainError> {
            Ok(())
        }

        async fn check_access(
            &self,
            _project_name: &str,
            _repo: &str,
        ) -> Result<pi_domain::contracts::SourceAccess, DomainError> {
            Ok(pi_domain::contracts::SourceAccess::Ok)
        }
    }

    fn build_with_source(
        m: Mocks,
        source: Arc<dyn pi_domain::contracts::Source>,
        timeouts: StageTimeouts,
    ) -> Arc<DeployProject> {
        let gc = RunGc::new(Arc::new(m.gc_runtime), Arc::new(m.disk), 85);
        DeployProject::new(
            source,
            Arc::new(m.runtime),
            Arc::new(m.projects),
            Arc::new(m.history),
            Arc::new(m.overrides),
            Arc::new(m.secrets),
            Arc::new(m.secrets_writer),
            Arc::new(m.health),
            Arc::new(m.ingress),
            Arc::new(m.host_network),
            Arc::new(m.clock),
            gc,
            timeouts,
            1,
        )
    }

    #[tokio::test]
    async fn stored_bundle_is_written_to_workdir_and_masked_in_logs() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets.expect_load().returning(|_| {
            Ok(SecretGroup {
                objects: secret_bundle(),
                revision: 1,
            })
        });
        m.secrets_writer
            .expect_write()
            .withf(|wd, b| {
                wd == Path::new("/wd")
                    && b.vars.contains_key("DB_PASSWORD")
                    && b.files.contains_key("certs/server.pem")
            })
            .times(1)
            .returning(|_, _| Ok(()));
        m.runtime.expect_build().returning(|_, log| {
            log.line("connecting with hunter2-long");
            Ok(())
        });
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));

        let deploy = build(m);
        let sink = CollectSink::new();
        let result = deploy
            .execute(
                "dep-env".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert!(
            result
                .log_tail
                .contains("secrets injected (1 keys, 1 files, mode 0644; groups: key@r1)"),
            "tail: {}",
            result.log_tail
        );
        assert!(
            result.log_tail.contains("***DB_PASSWORD***"),
            "tail: {}",
            result.log_tail
        );
        assert!(
            !result.log_tail.contains("hunter2-long"),
            "secret leaked into tail"
        );
        let lines = sink.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.contains("***DB_PASSWORD***")));
        assert!(
            !lines.iter().any(|l| l.contains("hunter2-long")),
            "secret leaked into stream"
        );
    }

    #[tokio::test]
    async fn health_gate_failure_fails_deploy_and_skips_ingress() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.health
            .expect_check()
            .returning(|_, _, _| Err(DomainError::HealthCheck("timed out after 60s".into())));
        m.ingress.expect_upsert().times(0);

        let deploy = build(m);
        let sink = CollectSink::new();
        let err = deploy
            .execute(
                "dep-hc".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::HealthCheck(_)));
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Failed]
        );
    }

    #[tokio::test]
    async fn project_without_hostname_skips_ingress() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);

        let mut config = sample_config();
        config.hostname = None;

        let deploy = build(m);
        let result = deploy
            .execute(
                "dep-nh".into(),
                config,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn skipped_ingress_emits_final_warning_line() {
        let mut m = mocks();
        let cfg = sample_config(); // hostname = rateme.isskelo.com

        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/w"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/o").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/o.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .times(1)
            .returning(|_, _, _| Ok(IngressOutcome::Skipped));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let deploy = build(m);
        let result = deploy
            .execute(
                "dep-w".into(),
                cfg,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        let last = result.log_tail.lines().last().unwrap_or_default();
        assert!(
            last.starts_with("warning: hostname rateme.isskelo.com"),
            "warning must be the last log line, got: {last}"
        );
        assert!(last.contains("sudo rpi agent setup --with-cloudflared"));
    }

    #[tokio::test]
    async fn expired_fetch_stage_fails_with_timeout_and_stage_name() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .withf(|id, status, _sha, _at, tail| {
                id == "dep-t"
                    && *status == DeploymentStatus::Failed
                    && tail.contains("timeout: fetch")
            })
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        let timeouts = StageTimeouts {
            fetch_secs: 0,
            ..StageTimeouts::default()
        };
        let deploy = build_with_source(m, Arc::new(HangingSource), timeouts);
        let sink = CollectSink::new();
        let err = deploy
            .execute(
                "dep-t".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(&err, DomainError::Timeout { stage, .. } if stage == "fetch"),
            "got: {err}"
        );
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Failed]
        );
        let stages = sink.stages.lock().unwrap();
        assert_eq!(stages.last().unwrap().stage, "fetch");
        assert_eq!(
            stages.last().unwrap().status,
            pi_domain::entities::StageStatus::Failed
        );
    }

    #[tokio::test]
    async fn cancel_token_marks_deployment_canceled() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .withf(|id, status, _sha, _at, _tail| {
                id == "dep-c" && *status == DeploymentStatus::Canceled
            })
            .times(1)
            .returning(|_, _, _, _, _| Ok(()));

        let deploy = build_with_source(m, Arc::new(HangingSource), StageTimeouts::default());
        let sink = CollectSink::new();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let deploy = Arc::clone(&deploy);
            let sink = sink.clone();
            let cancel = cancel.clone();
            async move {
                deploy
                    .execute(
                        "dep-c".into(),
                        sample_config(),
                        DeployRef::Branch("main".into()),
                        sink,
                        cancel,
                    )
                    .await
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let err = task.await.unwrap().unwrap_err();

        assert!(matches!(err, DomainError::Canceled), "got: {err}");
        assert_eq!(
            *sink.finished.lock().unwrap(),
            vec![DeploymentStatus::Canceled]
        );
    }

    struct CountingRuntime {
        active: std::sync::atomic::AtomicUsize,
        max_seen: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl pi_domain::contracts::ContainerRuntime for CountingRuntime {
        async fn build(
            &self,
            _stack: &pi_domain::entities::ComposeStack,
            _log: Arc<dyn LogSink>,
        ) -> Result<(), DomainError> {
            use std::sync::atomic::Ordering::SeqCst;
            let n = self.active.fetch_add(1, SeqCst) + 1;
            self.max_seen.fetch_max(n, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.active.fetch_sub(1, SeqCst);
            Ok(())
        }

        async fn up(
            &self,
            _stack: &pi_domain::entities::ComposeStack,
            _log: Arc<dyn LogSink>,
        ) -> Result<(), DomainError> {
            Ok(())
        }

        async fn ps(
            &self,
            _project_name: &str,
        ) -> Result<Vec<pi_domain::entities::ServiceState>, DomainError> {
            Ok(vec![])
        }

        async fn services(
            &self,
            _: &pi_domain::entities::ComposeStack,
        ) -> Result<Vec<String>, DomainError> {
            Ok(vec![])
        }

        async fn prune_images(&self, _log: Arc<dyn LogSink>) -> Result<(), DomainError> {
            Ok(())
        }

        async fn prune_builder(&self, _log: Arc<dyn LogSink>) -> Result<(), DomainError> {
            Ok(())
        }

        async fn logs(
            &self,
            _project_name: &str,
            _tail: usize,
            _follow: bool,
            _log: Arc<dyn LogSink>,
        ) -> Result<(), DomainError> {
            Ok(())
        }

        async fn stats(
            &self,
            _project_name: &str,
        ) -> Result<Vec<pi_domain::entities::ServiceStats>, DomainError> {
            Ok(vec![])
        }

        async fn lifecycle(
            &self,
            _stack: &pi_domain::entities::ComposeStack,
            _action: pi_domain::entities::LifecycleAction,
            _log: Arc<dyn LogSink>,
        ) -> Result<(), DomainError> {
            Ok(())
        }

        async fn down(
            &self,
            _stack: &pi_domain::entities::ComposeStack,
            _remove_volumes: bool,
            _log: Arc<dyn LogSink>,
        ) -> Result<(), DomainError> {
            Ok(())
        }

        async fn exec(
            &self,
            _stack: &pi_domain::entities::ComposeStack,
            _service: &str,
            _argv: &[String],
            _log: Arc<dyn LogSink>,
        ) -> Result<i32, DomainError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn builds_of_different_projects_are_serialized_by_global_semaphore() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: if c.name == "a" { 8000 } else { 8001 },
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|p, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd").join(&p.name),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        // CountingRuntime resolves `services` itself; only the override path
        // still comes from a mock here.
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|p, _, _, _, _, _, _| Ok(PathBuf::from("/ov").join(p)));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let runtime = Arc::new(CountingRuntime {
            active: std::sync::atomic::AtomicUsize::new(0),
            max_seen: std::sync::atomic::AtomicUsize::new(0),
        });
        let gc = RunGc::new(Arc::clone(&runtime) as _, Arc::new(m.disk), 85);
        let deploy = DeployProject::new(
            Arc::new(m.source),
            Arc::clone(&runtime) as _,
            Arc::new(m.projects),
            Arc::new(m.history),
            Arc::new(m.overrides),
            Arc::new(m.secrets),
            Arc::new(m.secrets_writer),
            Arc::new(m.health),
            Arc::new(m.ingress),
            Arc::new(m.host_network),
            Arc::new(m.clock),
            gc,
            StageTimeouts::default(),
            1,
        );

        let mut config_a = sample_config();
        config_a.name = "a".into();
        config_a.hostname = None;
        let mut config_b = sample_config();
        config_b.name = "b".into();
        config_b.hostname = None;

        let (ra, rb) = tokio::join!(
            deploy.execute(
                "dep-a".into(),
                config_a,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            ),
            deploy.execute(
                "dep-b".into(),
                config_b,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            ),
        );
        ra.unwrap();
        rb.unwrap();
        assert_eq!(
            runtime.max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "two builds must never run concurrently with a semaphore of 1"
        );
    }

    #[tokio::test]
    async fn gc_failure_does_not_fail_the_deploy() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.gc_runtime.checkpoint();
        m.gc_runtime
            .expect_prune_images()
            .returning(|_| Err(DomainError::Runtime("docker daemon hiccup".into())));

        let deploy = build(m);
        let result = deploy
            .execute(
                "dep-gc".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert!(
            result.log_tail.contains("gc skipped"),
            "tail: {}",
            result.log_tail
        );
    }

    #[tokio::test]
    async fn build_failure_emits_build_failed_stage_event() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime
            .expect_build()
            .returning(|_, _| Err(DomainError::Runtime("compose build exited with 1".into())));

        let deploy = build(m);
        let sink = CollectSink::new();
        deploy
            .execute(
                "dep-bf".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        let stages = sink.stages.lock().unwrap();
        let last = stages.last().unwrap();
        assert_eq!(last.stage, "build");
        assert_eq!(last.status, pi_domain::entities::StageStatus::Failed);
        assert!(last.elapsed_ms.is_some());
    }

    #[tokio::test]
    async fn skipped_ingress_emits_route_skipped_event() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Skipped));

        let deploy = build(m);
        let sink = CollectSink::new();
        deploy
            .execute(
                "dep-rs".into(),
                sample_config(), // hostname = rateme.isskelo.com
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(sink
            .stages
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.stage == "route" && e.status == pi_domain::entities::StageStatus::Skipped));
    }

    #[tokio::test]
    async fn gc_failure_emits_gc_skipped_event() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.gc_runtime.checkpoint();
        m.gc_runtime
            .expect_prune_images()
            .returning(|_| Err(DomainError::Runtime("docker daemon hiccup".into())));

        let deploy = build(m);
        let sink = CollectSink::new();
        let result = deploy
            .execute(
                "dep-gs".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
        assert!(sink
            .stages
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.stage == "gc" && e.status == pi_domain::entities::StageStatus::Skipped));
    }

    #[tokio::test]
    async fn project_without_hostname_emits_no_route_stage() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress.expect_upsert().times(0);

        let mut config = sample_config();
        config.hostname = None;

        let deploy = build(m);
        let sink = CollectSink::new();
        deploy
            .execute(
                "dep-nr".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!sink
            .stages
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.stage == "route"));
    }

    fn env_sample_config(on_create: Option<&str>) -> ProjectConfig {
        let mut config = sample_config();
        config.name = "rateme--test".into();
        config.hostname = None;
        config.environment = Some(pi_domain::entities::EnvironmentMeta {
            env: "test".into(),
            base: "rateme".into(),
            slug: None,
            ttl_secs: None,
            on_create: on_create.map(String::from),
        });
        if let Some(cmd) = on_create {
            config.commands.insert(
                cmd.to_string(),
                pi_domain::entities::CommandSpec::new(vec!["node".into(), "seed.js".into()]),
            );
        }
        config
    }

    #[tokio::test]
    async fn on_create_runs_once_after_healthy_deploy_and_marks_done() {
        let mut m = mocks();
        // fresh expectation below replaces the unbounded default from mocks()
        m.projects.checkpoint();
        let cfg = env_sample_config(Some("seed"));

        m.projects.expect_upsert().times(1).returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.runtime
            .expect_exec()
            .withf(|_stack, service, argv, _log| service == "web" && argv == ["node", "seed.js"])
            .times(1)
            .returning(|_, _, _, _| Ok(0));
        m.projects
            .expect_set_on_create_done()
            .withf(|name, done| name == "rateme--test" && *done)
            .times(1)
            .returning(|_, _| Ok(()));
        m.projects
            .expect_mark_deploy_success()
            .withf(|name, at, _| name == "rateme--test" && *at == 100)
            .times(1)
            .returning(|_, _, _| Ok(()));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let result = build(m)
            .execute(
                "dep-oc".into(),
                cfg,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn on_create_skipped_when_already_done() {
        let mut m = mocks();
        let cfg = env_sample_config(Some("seed"));

        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: true,
                last_success_at: Some(50),
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        // on_create already done: exec/set_on_create_done must never fire
        m.runtime.expect_exec().times(0);
        m.projects.expect_set_on_create_done().times(0);
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let result = build(m)
            .execute(
                "dep-oc-done".into(),
                cfg,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn on_create_nonzero_exit_fails_deploy_and_keeps_flag() {
        let mut m = mocks();
        let cfg = env_sample_config(Some("seed"));

        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.runtime.expect_exec().returning(|_, _, _, _| Ok(1));
        m.projects.expect_set_on_create_done().times(0);
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .withf(|_id, status, _sha, _at, _tail| *status == DeploymentStatus::Failed)
            .returning(|_, _, _, _, _| Ok(()));

        let deploy = build(m);
        let sink = CollectSink::new();
        let err = deploy
            .execute(
                "dep-oc-fail".into(),
                cfg,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::Runtime(_)), "got: {err}");
        assert!(
            sink.stages
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.stage == "on_create"
                    && e.status == pi_domain::entities::StageStatus::Failed),
            "expected a failed on_create stage event"
        );
    }

    #[tokio::test]
    async fn successful_base_deploy_marks_last_success() {
        let mut m = mocks();
        // fresh expectation below replaces the unbounded default from mocks()
        m.projects.checkpoint();
        ok_pre_stages(&mut m);
        m.projects
            .expect_mark_deploy_success()
            .withf(|name, at, _| name == "rateme" && *at == 100)
            .times(1)
            .returning(|_, _, _| Ok(()));
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));

        let result = build(m)
            .execute(
                "dep-success-marks".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn override_receives_every_discovered_service_and_the_runtime_env() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.runtime
            .expect_services()
            .times(1)
            .returning(|_| Ok(vec!["web".into(), "worker".into()]));
        m.overrides
            .expect_write()
            .withf(|_, _, _, _, _, services, env| {
                services == ["web".to_string(), "worker".to_string()]
                    && env["RPI_PROJECT"] == "rateme"
                    && env["RPI_BRANCH_NAME"] == "main"
                    && env["RPI_HOST_PORT"] == "8000"
                    && env["RPI_COMMIT_SHA"] == SHA
                    && env["RPI_HOSTNAME"] == "rateme.isskelo.com"
            })
            .times(1)
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime
            .expect_build()
            .withf(|stack, _| stack.env["RPI_COMMIT_SHA"] == SHA)
            .returning(|_, _| Ok(()));
        m.runtime
            .expect_up()
            .withf(|stack, _| stack.env["RPI_PROJECT"] == "rateme")
            .returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let result = build(m)
            .execute(
                "dep-env-vars".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn service_discovery_failure_fails_the_deploy_before_build() {
        let mut m = mocks();
        ok_pre_stages_without_override(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.runtime
            .expect_services()
            .returning(|_| Err(DomainError::Runtime("compose config: no such file".into())));
        m.overrides.expect_write().times(0);
        m.runtime.expect_build().times(0);
        m.runtime.expect_up().times(0);

        let err = build(m)
            .execute(
                "dep-svc".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Runtime(_)), "got: {err}");
    }

    #[tokio::test]
    async fn successful_deploy_records_the_commit_sha_on_the_project() {
        let mut m = mocks();
        m.projects.checkpoint();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.projects
            .expect_mark_deploy_success()
            .withf(|name, _, sha| name == "rateme" && *sha == Some(SHA))
            .times(1)
            .returning(|_, _, _| Ok(()));
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        build(m)
            .execute(
                "dep-sha".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn declared_groups_are_injected_in_order_under_the_key_bundle() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        let mut config = config_with_groups(vec!["common".into(), "preview".into()]);
        config.name = "myapp--branch--x".into();
        config.hostname = None;
        set_env_base(&mut config, "myapp");

        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { base, name } if base == "myapp" && name == "common" => {
                Ok(group_with(&[("A", "from-common"), ("B", "from-common")]))
            }
            GroupRef::Named { base, name } if base == "myapp" && name == "preview" => {
                Ok(group_with(&[("B", "from-preview")]))
            }
            GroupRef::Key(key) if key == "myapp--branch--x" => Ok(group_with(&[("B", "from-key")])),
            other => panic!("unexpected load({other:?})"),
        });
        m.secrets_writer
            .expect_write()
            .withf(|_, b| b.vars["A"] == "from-common" && b.vars["B"] == "from-key")
            .times(1)
            .returning(|_, _| Ok(()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));

        let deploy = build(m);
        let sink = CollectSink::new();
        deploy
            .execute(
                "dep-groups-order".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("groups: common@r1, preview@r1, key@r1")),
            "provenance must be in the log: {lines:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_declared_group_fails_the_deploy_at_the_secrets_stage() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        let mut config = config_with_groups(vec!["preview".into()]);
        config.hostname = None;
        set_env_base(&mut config, "myapp");
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretGroup::default()));
        m.secrets_writer.expect_write().times(0);
        m.runtime.expect_build().times(0);

        let deploy = build(m);
        let err = deploy
            .execute(
                "dep-missing-group".into(),
                config,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
        assert!(err.to_string().contains("preview"), "got: {err}");
    }

    #[tokio::test]
    async fn masking_is_armed_on_values_that_came_from_a_group() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        let mut config = config_with_groups(vec!["preview".into()]);
        config.hostname = None;
        set_env_base(&mut config, "myapp");
        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { .. } => Ok(group_with(&[("DB_PASSWORD", "group-secret-value")])),
            GroupRef::Key(_) => Ok(SecretGroup::default()),
        });
        m.secrets_writer.expect_write().returning(|_, _| Ok(()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, log| {
            log.line("starting with group-secret-value");
            Ok(())
        });
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));

        let deploy = build(m);
        let sink = CollectSink::new();
        deploy
            .execute(
                "dep-mask-group".into(),
                config,
                DeployRef::Branch("main".into()),
                sink.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(
            !lines.iter().any(|l| l.contains("group-secret-value")),
            "a group's value leaked into container output: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("***DB_PASSWORD***")));
    }

    #[tokio::test]
    async fn a_base_project_resolves_groups_under_its_own_name() {
        let mut m = mocks();
        ok_pre_stages(&mut m);
        let mut config = config_with_groups(vec!["common".into()]);
        config.name = "myapp".into();
        config.environment = None;
        config.hostname = None;
        m.secrets.expect_load().returning(|r| match r {
            GroupRef::Named { base, .. } if base == "myapp" => Ok(group_with(&[("A", "1")])),
            GroupRef::Key(_) => Ok(SecretGroup::default()),
            other => panic!("unexpected load({other:?})"),
        });
        m.secrets_writer.expect_write().returning(|_, _| Ok(()));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));

        let deploy = build(m);
        let result = deploy
            .execute(
                "dep-base-group".into(),
                config,
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, DeploymentStatus::Success);
    }
}

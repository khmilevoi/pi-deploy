use std::sync::Arc;

use pi_domain::contracts::{
    Clock, ContainerRuntime, DeploymentHistory, LogSink, OverrideStore, ProjectRepository, Source,
};
use pi_domain::entities::{ComposeStack, DeploymentStatus, Project};
use pi_domain::error::DomainError;

use crate::remove::RemoveProject;

/// `GET /v1/environments` (`rpi env ls`, environment-overlays spec): pure
/// pass-through of `ProjectRepository::list_environments`, kept as a
/// use-case so the HTTP layer never talks to the registry directly.
pub struct ListEnvironments {
    projects: Arc<dyn ProjectRepository>,
}

impl ListEnvironments {
    pub fn new(projects: Arc<dyn ProjectRepository>) -> Arc<ListEnvironments> {
        Arc::new(ListEnvironments { projects })
    }

    pub async fn execute(&self, base: Option<&str>) -> Result<Vec<Project>, DomainError> {
        self.projects.list_environments(base).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestroyOutcome {
    pub key: String,
    pub already_absent: bool,
}

/// `DELETE /v1/environments/{key}` (`rpi env rm`): tears an environment
/// overlay down completely. A missing key is not an error (idempotent
/// delete); a base-project key is rejected — `rpi rm` owns that path. The
/// active-deploy conflict check lives entirely in [`RemoveProject`], which
/// does the actual teardown — this use-case must not duplicate it.
pub struct DestroyEnvironment {
    projects: Arc<dyn ProjectRepository>,
    remove: Arc<RemoveProject>,
}

impl DestroyEnvironment {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        remove: Arc<RemoveProject>,
    ) -> Arc<DestroyEnvironment> {
        Arc::new(DestroyEnvironment { projects, remove })
    }

    pub async fn execute(
        &self,
        key: &str,
        log: Arc<dyn LogSink>,
    ) -> Result<DestroyOutcome, DomainError> {
        let Some(existing) = self.projects.get(key).await? else {
            return Ok(DestroyOutcome {
                key: key.to_string(),
                already_absent: true,
            });
        };
        if existing.config.environment.is_none() {
            return Err(DomainError::Conflict(format!(
                "'{key}' is a base project, not an environment - use `rpi rm` for base projects"
            )));
        }
        self.remove.execute(key, true, log).await?;
        Ok(DestroyOutcome {
            key: key.to_string(),
            already_absent: false,
        })
    }
}

/// `POST /v1/environments/{key}/reset-data` (`rpi env reset-data`): drops
/// the overlay's containers and named volumes without deregistering the
/// project, and clears `on_create_done` so the next deploy re-runs the
/// overlay's `on_create` hook against a clean database. Unlike
/// [`DestroyEnvironment`], this use-case owns its own active-deploy guard —
/// there is no `RemoveProject` delegate here to do it for us.
pub struct ResetEnvironmentData {
    projects: Arc<dyn ProjectRepository>,
    history: Arc<dyn DeploymentHistory>,
    runtime: Arc<dyn ContainerRuntime>,
    source: Arc<dyn Source>,
    overrides: Arc<dyn OverrideStore>,
}

impl ResetEnvironmentData {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        history: Arc<dyn DeploymentHistory>,
        runtime: Arc<dyn ContainerRuntime>,
        source: Arc<dyn Source>,
        overrides: Arc<dyn OverrideStore>,
    ) -> Arc<ResetEnvironmentData> {
        Arc::new(ResetEnvironmentData {
            projects,
            history,
            runtime,
            source,
            overrides,
        })
    }

    pub async fn execute(&self, key: &str, log: Arc<dyn LogSink>) -> Result<(), DomainError> {
        let Some(existing) = self.projects.get(key).await? else {
            return Err(DomainError::NotFound(format!("environment {key}")));
        };
        if existing.config.environment.is_none() {
            return Err(DomainError::Conflict(format!(
                "'{key}' is a base project, not an environment"
            )));
        }
        if !self.history.active(key).await?.is_empty() {
            return Err(DomainError::Conflict(format!(
                "environment {key} has an active deployment; wait for it or cancel it first"
            )));
        }
        let workdir = self.source.workdir(key);
        let compose_file = workdir.join(&existing.config.compose_path);
        if compose_file.exists() {
            let stack = ComposeStack {
                project_name: existing.config.name.clone(),
                workdir,
                compose_file,
                override_file: self.overrides.path(key),
            };
            self.runtime.down(&stack, true, Arc::clone(&log)).await?;
        }
        self.projects.set_on_create_done(key, false).await?;
        Ok(())
    }
}

/// Local `LogSink` for [`ReapEnvironments`]'s background sweep. There is no
/// live viewer for it (unlike a CLI-initiated destroy), so container output
/// just goes to the agent log via `tracing::info!`. Defined here rather than
/// reusing the bin crate's `TracingSink` (`agent/http.rs`) because
/// `pi-application` must never depend on bin-crate code.
struct ReaperSink;

impl LogSink for ReaperSink {
    fn line(&self, line: &str) {
        tracing::info!("{line}");
    }
    fn finished(&self, _status: DeploymentStatus) {}
}

/// `[environments].reap_interval` tick (environment-overlays spec): tears
/// down environment overlays whose TTL has elapsed since their last
/// successful deploy (or since creation, if they never deployed
/// successfully). `agent/run.rs` calls [`ReapEnvironments::execute`] on a
/// timer; one call is one sweep. Listing errors bubble (the sweep can't
/// proceed without the list); everything else is per-environment
/// best-effort — a failure there is logged and retried on the next tick
/// rather than aborting the whole sweep.
pub struct ReapEnvironments {
    projects: Arc<dyn ProjectRepository>,
    history: Arc<dyn DeploymentHistory>,
    destroy: Arc<DestroyEnvironment>,
    clock: Arc<dyn Clock>,
}

impl ReapEnvironments {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        history: Arc<dyn DeploymentHistory>,
        destroy: Arc<DestroyEnvironment>,
        clock: Arc<dyn Clock>,
    ) -> Arc<ReapEnvironments> {
        Arc::new(ReapEnvironments {
            projects,
            history,
            destroy,
            clock,
        })
    }

    /// One sweep. Returns the keys destroyed this tick.
    pub async fn execute(&self) -> Result<Vec<String>, DomainError> {
        let now = self.clock.now_unix();
        let mut destroyed = Vec::new();
        for p in self.projects.list_environments(None).await? {
            let Some(meta) = &p.config.environment else {
                continue;
            };
            let Some(ttl) = meta.ttl_secs else {
                continue;
            };
            let anchor = p.last_success_at.unwrap_or(p.created_at);
            let deadline = anchor.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));
            if deadline > now {
                continue;
            }
            let key = p.config.name.clone();
            match self.history.active(&key).await {
                Ok(active) if !active.is_empty() => continue, // retry next tick
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!("reaper: cannot check active deploys of {key}: {err}");
                    continue;
                }
            }
            // TOCTOU guard: the listing above and the active-deploy check
            // just did can both be stale by the time we get here — a
            // redeploy that started and finished successfully in between
            // would refresh `last_success_at` without ever showing up as
            // "active" at the instant we checked. Re-fetch the row fresh
            // and recompute expiry from it before actually destroying
            // anything; a stale anchor here would tear down an environment
            // that was just redeployed.
            let fresh = match self.projects.get(&key).await {
                Ok(Some(p)) => p,
                Ok(None) => continue, // already gone
                Err(err) => {
                    tracing::warn!("reaper: cannot re-check {key}: {err}");
                    continue;
                }
            };
            let Some(fresh_meta) = &fresh.config.environment else {
                continue;
            };
            let Some(fresh_ttl) = fresh_meta.ttl_secs else {
                continue;
            };
            let fresh_anchor = fresh.last_success_at.unwrap_or(fresh.created_at);
            if fresh_anchor.saturating_add(i64::try_from(fresh_ttl).unwrap_or(i64::MAX)) > now {
                continue; // redeployed while we were sweeping
            }
            match self.destroy.execute(&key, Arc::new(ReaperSink)).await {
                Ok(_) => {
                    tracing::info!(
                        "reaper: environment {key} expired (ttl {ttl}s) and was removed"
                    );
                    destroyed.push(key);
                }
                Err(err) => tracing::warn!("reaper: destroying {key} failed: {err} (will retry)"),
            }
        }
        Ok(destroyed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        Clock, MockClock, MockContainerRuntime, MockDeploymentHistory, MockIngress,
        MockOverrideStore, MockProjectRepository, MockSecretStore, MockSource,
    };
    use pi_domain::entities::{Deployment, DeploymentStatus, EnvironmentMeta, ProjectConfig};
    use std::path::PathBuf;

    fn env_meta() -> EnvironmentMeta {
        EnvironmentMeta {
            env: "test".into(),
            base: "myapp".into(),
            slug: None,
            ttl_secs: Some(3600),
            on_create: None,
        }
    }

    fn project(name: &str, environment: Option<EnvironmentMeta>) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: "https://github.com/x/y.git".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 3000,
                hostname: Some("app.example.com".into()),
                expose: Default::default(),
                healthcheck: Default::default(),
                timeouts: Default::default(),
                commands: Default::default(),
                command_timeout_secs: None,
                environment,
                secret_groups: Vec::new(),
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
        }
    }

    /// `RemoveProject` wired with mocks that carry no expectations — used
    /// where `DestroyEnvironment` must short-circuit before ever delegating
    /// to it (missing key / base-project key). Calling any of these mocks
    /// unexpectedly is a test failure (mockall panics), which is exactly the
    /// assertion we want: the delegate was never touched.
    fn untouched_remove(projects: Arc<dyn ProjectRepository>) -> Arc<RemoveProject> {
        RemoveProject::new(
            projects,
            Arc::new(MockDeploymentHistory::new()),
            Arc::new(MockContainerRuntime::new()),
            Arc::new(MockIngress::new()),
            Arc::new(MockSource::new()),
            Arc::new(MockSecretStore::new()),
            Arc::new(MockOverrideStore::new()),
        )
    }

    /// A real (unique) temp directory containing a `docker-compose.yml`, so
    /// `compose_file.exists()` is true for the stack-teardown path. Cleaned
    /// up best-effort on drop of the returned guard.
    struct ComposeDir(PathBuf);
    impl ComposeDir {
        fn new(label: &str) -> ComposeDir {
            let dir = std::env::temp_dir().join(format!(
                "pi-application-environments-test-{label}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("docker-compose.yml"), "services: {}\n").unwrap();
            ComposeDir(dir)
        }
    }
    impl Drop for ComposeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn list_delegates_to_project_repository_with_base_filter() {
        let expected = vec![project("myapp--test", Some(env_meta()))];
        let returned = expected.clone();
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list_environments()
            .withf(|base: &Option<&str>| *base == Some("myapp"))
            .times(1)
            .returning(move |_| Ok(returned.clone()));

        let list = ListEnvironments::new(Arc::new(projects));
        let got = list.execute(Some("myapp")).await.unwrap();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn destroy_missing_key_is_ok_already_absent() {
        let mut projects = MockProjectRepository::new();
        projects.expect_get().times(1).returning(|_| Ok(None));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let remove = untouched_remove(Arc::clone(&projects));
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let outcome = destroy
            .execute("ghost--test", CollectSink::new())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            DestroyOutcome {
                key: "ghost--test".into(),
                already_absent: true,
            }
        );
    }

    #[tokio::test]
    async fn destroy_base_key_is_conflict() {
        let proj = project("myapp", None);
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .times(1)
            .returning(move |_| Ok(Some(proj.clone())));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let remove = untouched_remove(Arc::clone(&projects));
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let err = destroy
            .execute("myapp", CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
    }

    #[tokio::test]
    async fn destroy_environment_delegates_to_remove_with_volumes() {
        let dir = ComposeDir::new("destroy");
        let key = "myapp--test";
        let proj = project(key, Some(env_meta()));

        let mut projects = MockProjectRepository::new();
        let get_proj = proj.clone();
        // Called once by DestroyEnvironment's own kind guard, once more
        // inside RemoveProject::execute.
        projects
            .expect_get()
            .times(2)
            .returning(move |_| Ok(Some(get_proj.clone())));
        projects.expect_remove().times(1).returning(|_| Ok(()));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(1).returning(|_| Ok(vec![]));
        history
            .expect_remove_project()
            .times(1)
            .returning(|_| Ok(()));
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_down()
            .withf(|_, remove_volumes, _| *remove_volumes)
            .times(1)
            .returning(|_, _, _| Ok(()));
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);

        let mut ingress = MockIngress::new();
        ingress
            .expect_remove()
            .withf(|hostname, _| hostname == "app.example.com")
            .times(1)
            .returning(|_, _| Ok(()));
        let ingress: Arc<dyn pi_domain::contracts::Ingress> = Arc::new(ingress);

        let mut source = MockSource::new();
        let workdir = dir.0.clone();
        source.expect_workdir().returning(move |_| workdir.clone());
        source.expect_cleanup().times(1).returning(|_| Ok(()));
        let source: Arc<dyn Source> = Arc::new(source);

        let mut secrets = MockSecretStore::new();
        secrets.expect_remove().times(1).returning(|_| Ok(()));
        let secrets: Arc<dyn pi_domain::contracts::SecretStore> = Arc::new(secrets);

        let mut overrides = MockOverrideStore::new();
        overrides
            .expect_path()
            .returning(|name| PathBuf::from("/overrides").join(name));
        overrides.expect_remove().times(1).returning(|_| Ok(()));
        let overrides: Arc<dyn OverrideStore> = Arc::new(overrides);

        let remove = RemoveProject::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            runtime,
            ingress,
            source,
            secrets,
            overrides,
        );
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let outcome = destroy.execute(key, CollectSink::new()).await.unwrap();
        assert_eq!(
            outcome,
            DestroyOutcome {
                key: key.into(),
                already_absent: false,
            }
        );
    }

    #[tokio::test]
    async fn reset_data_downs_volumes_and_clears_flag() {
        let dir = ComposeDir::new("reset-ok");
        let key = "myapp--test";
        let proj = project(key, Some(env_meta()));

        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .times(1)
            .returning(move |_| Ok(Some(proj.clone())));
        projects
            .expect_set_on_create_done()
            .withf(|k, done| k == "myapp--test" && !*done)
            .times(1)
            .returning(|_, _| Ok(()));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(1).returning(|_| Ok(vec![]));
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_down()
            .withf(|_, remove_volumes, _| *remove_volumes)
            .times(1)
            .returning(|_, _, _| Ok(()));
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);

        let mut source = MockSource::new();
        let workdir = dir.0.clone();
        source.expect_workdir().returning(move |_| workdir.clone());
        let source: Arc<dyn Source> = Arc::new(source);

        let mut overrides = MockOverrideStore::new();
        overrides
            .expect_path()
            .returning(|name| PathBuf::from("/overrides").join(name));
        let overrides: Arc<dyn OverrideStore> = Arc::new(overrides);

        let reset = ResetEnvironmentData::new(projects, history, runtime, source, overrides);
        reset.execute(key, CollectSink::new()).await.unwrap();
    }

    #[tokio::test]
    async fn reset_data_with_active_deploy_is_conflict() {
        let key = "myapp--test";
        let proj = project(key, Some(env_meta()));
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .times(1)
            .returning(move |_| Ok(Some(proj.clone())));
        projects.expect_set_on_create_done().times(0);
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(1).returning(|_| {
            Ok(vec![Deployment {
                id: "dep-1".into(),
                project: "myapp--test".into(),
                git_ref: "main".into(),
                commit_sha: None,
                status: DeploymentStatus::Running,
                started_at: 1,
                finished_at: None,
                log_tail: String::new(),
            }])
        });
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let mut runtime = MockContainerRuntime::new();
        runtime.expect_down().times(0);
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);
        let source: Arc<dyn Source> = Arc::new(MockSource::new());
        let overrides: Arc<dyn OverrideStore> = Arc::new(MockOverrideStore::new());

        let reset = ResetEnvironmentData::new(projects, history, runtime, source, overrides);
        let err = reset.execute(key, CollectSink::new()).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
    }

    #[tokio::test]
    async fn reset_data_on_base_key_is_conflict() {
        let proj = project("myapp", None);
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .times(1)
            .returning(move |_| Ok(Some(proj.clone())));
        projects.expect_set_on_create_done().times(0);
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(0);
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(MockContainerRuntime::new());
        let source: Arc<dyn Source> = Arc::new(MockSource::new());
        let overrides: Arc<dyn OverrideStore> = Arc::new(MockOverrideStore::new());

        let reset = ResetEnvironmentData::new(projects, history, runtime, source, overrides);
        let err = reset
            .execute("myapp", CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
    }

    /// An environment project with `hostname: None` (so `RemoveProject`
    /// never has to touch ingress) and a caller-controlled TTL/anchor, for
    /// the `ReapEnvironments` tests below.
    fn reaper_project(
        name: &str,
        ttl_secs: Option<u64>,
        last_success_at: Option<i64>,
        created_at: i64,
    ) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: "https://github.com/x/y.git".into(),
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
                environment: Some(EnvironmentMeta {
                    env: "test".into(),
                    base: "myapp".into(),
                    slug: None,
                    ttl_secs,
                    on_create: None,
                }),
                secret_groups: Vec::new(),
            },
            host_port: 8000,
            created_at,
            on_create_done: false,
            last_success_at,
        }
    }

    fn fixed_clock(now: i64) -> Arc<dyn Clock> {
        let mut clock = MockClock::new();
        clock.expect_now_unix().returning(move || now);
        Arc::new(clock)
    }

    #[tokio::test]
    async fn reaper_destroys_only_expired_environments() {
        let now: i64 = 1_000_000;
        let key_a = "myapp--a";
        let key_b = "myapp--b";
        let key_c = "myapp--c";
        let key_d = "myapp--d";

        // A: ttl 100, last_success_at now-200 -> anchor+ttl = now-100 <= now -> expired.
        let proj_a = reaper_project(key_a, Some(100), Some(now - 200), now - 500);
        // B: ttl 100, last_success_at now-50 -> anchor+ttl = now+50 > now -> kept.
        let proj_b = reaper_project(key_b, Some(100), Some(now - 50), now - 500);
        // C: no ttl -> kept regardless of age.
        let proj_c = reaper_project(key_c, None, None, now - 500);
        // D: ttl 100, never deployed -> anchor = created_at = now-200 -> expired.
        let proj_d = reaper_project(key_d, Some(100), None, now - 200);

        let listed = vec![
            proj_a.clone(),
            proj_b.clone(),
            proj_c.clone(),
            proj_d.clone(),
        ];
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list_environments()
            .times(1)
            .returning(move |_| Ok(listed.clone()));
        let get_a = proj_a.clone();
        let get_d = proj_d.clone();
        projects.expect_get().returning(move |key| match key {
            "myapp--a" => Ok(Some(get_a.clone())),
            "myapp--d" => Ok(Some(get_d.clone())),
            other => panic!("unexpected get({other}) - only expired envs should be fetched"),
        });
        projects.expect_remove().times(2).returning(|_| Ok(()));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().returning(|_| Ok(vec![]));
        history
            .expect_remove_project()
            .times(2)
            .returning(|_| Ok(()));
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let mut source = MockSource::new();
        source
            .expect_workdir()
            .returning(|_| std::env::temp_dir().join("pi-reaper-test-missing-compose"));
        source.expect_cleanup().times(2).returning(|_| Ok(()));
        let source: Arc<dyn Source> = Arc::new(source);

        let mut secrets = MockSecretStore::new();
        secrets.expect_remove().times(2).returning(|_| Ok(()));
        let secrets: Arc<dyn pi_domain::contracts::SecretStore> = Arc::new(secrets);

        let mut overrides = MockOverrideStore::new();
        overrides.expect_remove().times(2).returning(|_| Ok(()));
        let overrides: Arc<dyn OverrideStore> = Arc::new(overrides);

        // No compose file at the returned workdir, so `down` is never
        // invoked; hostname is None, so ingress is never touched.
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(MockContainerRuntime::new());
        let ingress: Arc<dyn pi_domain::contracts::Ingress> = Arc::new(MockIngress::new());

        let remove = RemoveProject::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            runtime,
            ingress,
            source,
            secrets,
            overrides,
        );
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let reaper = ReapEnvironments::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            destroy,
            fixed_clock(now),
        );
        let destroyed = reaper.execute().await.unwrap();
        assert_eq!(destroyed, vec![key_a.to_string(), key_d.to_string()]);
    }

    #[tokio::test]
    async fn reaper_skips_environments_with_active_deploys() {
        let now: i64 = 1_000_000;
        let key = "myapp--busy";
        let proj = reaper_project(key, Some(100), Some(now - 200), now - 500);

        let mut projects = MockProjectRepository::new();
        let listed = vec![proj.clone()];
        projects
            .expect_list_environments()
            .times(1)
            .returning(move |_| Ok(listed.clone()));
        // Destroy must never even look the project up: the active-deploy
        // check short-circuits before `DestroyEnvironment::execute` runs.
        projects.expect_get().times(0);
        projects.expect_remove().times(0);
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(1).returning(move |_| {
            Ok(vec![Deployment {
                id: "dep-1".into(),
                project: key.into(),
                git_ref: "main".into(),
                commit_sha: None,
                status: DeploymentStatus::Running,
                started_at: 1,
                finished_at: None,
                log_tail: String::new(),
            }])
        });
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let remove = untouched_remove(Arc::clone(&projects));
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let reaper = ReapEnvironments::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            destroy,
            fixed_clock(now),
        );
        let destroyed = reaper.execute().await.unwrap();
        assert!(destroyed.is_empty(), "got: {destroyed:?}");
    }

    #[tokio::test]
    async fn reaper_continues_after_one_failed_destroy() {
        let now: i64 = 1_000_000;
        let key1 = "myapp--fail";
        let key2 = "myapp--ok";
        let proj1 = reaper_project(key1, Some(100), Some(now - 200), now - 500);
        let proj2 = reaper_project(key2, Some(100), Some(now - 200), now - 500);

        // key1's workdir has a real compose file, so `down` actually runs
        // (and is made to fail below); key2's workdir has none, so its
        // teardown skips straight past the container-runtime step.
        let dir = ComposeDir::new("reaper-continue");
        let dir1 = dir.0.clone();
        let missing = std::env::temp_dir().join("pi-reaper-test-missing-compose-ok");

        let mut projects = MockProjectRepository::new();
        let listed = vec![proj1.clone(), proj2.clone()];
        projects
            .expect_list_environments()
            .times(1)
            .returning(move |_| Ok(listed.clone()));
        let g1 = proj1.clone();
        let g2 = proj2.clone();
        projects.expect_get().returning(move |key| match key {
            "myapp--fail" => Ok(Some(g1.clone())),
            "myapp--ok" => Ok(Some(g2.clone())),
            other => panic!("unexpected get({other})"),
        });
        // Only key2's teardown reaches the final registry removal.
        projects.expect_remove().times(1).returning(|_| Ok(()));
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().returning(|_| Ok(vec![]));
        history
            .expect_remove_project()
            .times(1)
            .returning(|_| Ok(()));
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let mut source = MockSource::new();
        source.expect_workdir().returning(move |key| {
            if key == "myapp--fail" {
                dir1.clone()
            } else {
                missing.clone()
            }
        });
        source.expect_cleanup().times(1).returning(|_| Ok(()));
        let source: Arc<dyn Source> = Arc::new(source);

        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_down()
            .times(1)
            .returning(|_, _, _| Err(DomainError::Runtime("boom".into())));
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);

        let ingress: Arc<dyn pi_domain::contracts::Ingress> = Arc::new(MockIngress::new());

        let mut secrets = MockSecretStore::new();
        secrets.expect_remove().times(1).returning(|_| Ok(()));
        let secrets: Arc<dyn pi_domain::contracts::SecretStore> = Arc::new(secrets);

        let mut overrides = MockOverrideStore::new();
        // key1's compose file exists, so `RemoveProject` builds a
        // `ComposeStack` (and thus calls `path`) before `down` fails.
        overrides
            .expect_path()
            .returning(|name| PathBuf::from("/overrides").join(name));
        overrides.expect_remove().times(1).returning(|_| Ok(()));
        let overrides: Arc<dyn OverrideStore> = Arc::new(overrides);

        let remove = RemoveProject::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            runtime,
            ingress,
            source,
            secrets,
            overrides,
        );
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let reaper = ReapEnvironments::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            destroy,
            fixed_clock(now),
        );
        let destroyed = reaper.execute().await.unwrap();
        assert_eq!(destroyed, vec![key2.to_string()]);
    }

    #[tokio::test]
    async fn reaper_keeps_environment_with_huge_ttl() {
        let now: i64 = 1_000_000;
        let key = "myapp--huge";
        // u64::MAX ttl with old last_success_at should NOT expire
        let proj = reaper_project(key, Some(u64::MAX), Some(now - 200), now - 500);

        let listed = vec![proj.clone()];
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list_environments()
            .times(1)
            .returning(move |_| Ok(listed.clone()));
        // Destroy must never be called for this environment
        projects.expect_get().times(0);
        projects.expect_remove().times(0);
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(0);
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let remove = untouched_remove(Arc::clone(&projects));
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let reaper = ReapEnvironments::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            destroy,
            fixed_clock(now),
        );
        let destroyed = reaper.execute().await.unwrap();
        assert!(
            destroyed.is_empty(),
            "huge ttl environment must not be destroyed"
        );
    }

    /// TOCTOU guard (I4): the listing snapshot says the environment expired,
    /// but a fresh re-fetch right before destroying shows it was redeployed
    /// (recent `last_success_at`) while the sweep was in progress — the
    /// stale anchor from the listing must not be used to destroy it.
    #[tokio::test]
    async fn reaper_skips_environment_whose_fresh_recheck_shows_recent_deploy() {
        let now: i64 = 1_000_000;
        let key = "myapp--fresh";
        // Listing snapshot: ttl 100, last_success_at now-200 -> looks expired.
        let stale_listed = reaper_project(key, Some(100), Some(now - 200), now - 500);
        // Fresh re-fetch: redeployed mid-sweep, last_success_at now-10 -> no
        // longer expired.
        let fresh = reaper_project(key, Some(100), Some(now - 10), now - 500);

        let mut projects = MockProjectRepository::new();
        let listed = vec![stale_listed];
        projects
            .expect_list_environments()
            .times(1)
            .returning(move |_| Ok(listed.clone()));
        projects
            .expect_get()
            .withf(move |k| k == key)
            .times(1)
            .returning(move |_| Ok(Some(fresh.clone())));
        // Destroy must never be reached: the fresh re-check finds it no
        // longer expired.
        projects.expect_remove().times(0);
        let projects: Arc<dyn ProjectRepository> = Arc::new(projects);

        let mut history = MockDeploymentHistory::new();
        history.expect_active().times(1).returning(|_| Ok(vec![]));
        let history: Arc<dyn DeploymentHistory> = Arc::new(history);

        let remove = untouched_remove(Arc::clone(&projects));
        let destroy = DestroyEnvironment::new(Arc::clone(&projects), remove);

        let reaper = ReapEnvironments::new(
            Arc::clone(&projects),
            Arc::clone(&history),
            destroy,
            fixed_clock(now),
        );
        let destroyed = reaper.execute().await.unwrap();
        assert!(
            destroyed.is_empty(),
            "environment redeployed mid-sweep must not be destroyed: {destroyed:?}"
        );
    }
}

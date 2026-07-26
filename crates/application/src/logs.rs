use std::sync::Arc;

use pi_domain::contracts::{ContainerRuntime, LogSink, ProjectRepository, SecretStore};
use pi_domain::entities::{ProjectConfig, SecretsBundle};
use pi_domain::error::DomainError;

use crate::mask::MaskingSink;

pub const DEFAULT_LOG_TAIL: usize = 100;

pub struct StreamLogs {
    projects: Arc<dyn ProjectRepository>,
    secrets: Arc<dyn SecretStore>,
    runtime: Arc<dyn ContainerRuntime>,
}

impl StreamLogs {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        secrets: Arc<dyn SecretStore>,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> Arc<StreamLogs> {
        Arc::new(StreamLogs {
            projects,
            secrets,
            runtime,
        })
    }

    pub async fn ensure_project(&self, project: &str) -> Result<(), DomainError> {
        if self.projects.get(project).await?.is_none() {
            return Err(DomainError::NotFound(format!("project {project}")));
        }
        Ok(())
    }

    /// Everything a deploy would have injected into this container, so
    /// masking covers a value contributed by a *group* exactly as it covers
    /// one from the deploy key's own bundle. Arming on the key bundle alone
    /// would mean that moving a secret into a shared group — the whole point
    /// of groups — silently unmasked it here.
    ///
    /// Deliberately not `crate::effective_secrets`: that fails with
    /// `NotFound` on a declared-but-empty group, which is right for an
    /// injection and wrong for a reader. `rpi logs` must not stop working
    /// because a group was declared but never pushed — the same tolerance
    /// `ListSecrets` has, expressed the same way (`require_non_empty:
    /// false`). Nothing here is written anywhere: the stack is resolved only
    /// to know what to redact.
    async fn masking_bundle(&self, config: &ProjectConfig) -> Result<SecretsBundle, DomainError> {
        let loaded = crate::load_layer_stack(
            self.secrets.as_ref(),
            crate::group_base(config),
            &config.name,
            &config.secret_groups,
            false,
        )
        .await?;
        Ok(crate::merge_loaded_layers(&loaded)?.bundle)
    }

    pub async fn execute(
        &self,
        project: &str,
        tail: usize,
        follow: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        // One lookup, not `ensure_project` plus a second read: the registered
        // config is exactly what says which groups to resolve, and an
        // unregistered project is the same `NotFound` it has always been.
        let Some(registered) = self.projects.get(project).await? else {
            return Err(DomainError::NotFound(format!("project {project}")));
        };
        let mask = MaskingSink::new(log);
        mask.arm(&self.masking_bundle(&registered.config).await?);
        self.runtime.logs(project, tail, follow, mask).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{MockContainerRuntime, MockProjectRepository, MockSecretStore};
    use pi_domain::entities::{
        EnvironmentMeta, ExposeMode, HealthcheckConfig, Project, ProjectConfig,
        StageTimeoutOverrides,
    };
    use pi_domain::secretgroup::{GroupRef, SecretGroup};

    fn project(name: &str, base: Option<&str>, groups: &[&str]) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: "git@github.com:x/y.git".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 3000,
                hostname: None,
                expose: ExposeMode::default(),
                healthcheck: HealthcheckConfig::default(),
                timeouts: StageTimeoutOverrides::default(),
                commands: Default::default(),
                command_timeout_secs: None,
                environment: base.map(|base| EnvironmentMeta {
                    env: "preview".into(),
                    base: base.to_string(),
                    slug: None,
                    ttl_secs: None,
                    on_create: None,
                }),
                secret_groups: groups.iter().map(|g| (*g).to_string()).collect(),
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
            last_commit_sha: None,
        }
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

    fn build(
        projects: MockProjectRepository,
        secrets: MockSecretStore,
        runtime: MockContainerRuntime,
    ) -> Arc<StreamLogs> {
        StreamLogs::new(Arc::new(projects), Arc::new(secrets), Arc::new(runtime))
    }

    /// The regression this whole feature would otherwise cause: a secret
    /// moved out of the deploy key's own bundle and into a shared group must
    /// still be masked in `rpi logs`, or the act of adopting groups
    /// un-redacts it in streamed container output. Modeled on
    /// `deploy.rs`'s `masking_is_armed_on_values_that_came_from_a_group`.
    #[tokio::test]
    async fn masking_is_armed_on_a_value_that_came_only_from_a_group() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .withf(|n| n == "rateme")
            .returning(|_| Ok(Some(project("rateme", None, &["common"]))));
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "common"))
            .returning(|_| Ok(group_with(&[("DB_PASSWORD", "group-secret-value")])));
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| Ok(SecretGroup::default()));
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_logs().times(1).returning(|_, _, _, log| {
            log.line("app started with group-secret-value");
            Ok(())
        });

        let sink = CollectSink::new();
        build(projects, secrets, runtime)
            .execute("rateme", 100, false, sink.clone())
            .await
            .unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(
            !lines.iter().any(|l| l.contains("group-secret-value")),
            "a group's value leaked into streamed logs: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("***DB_PASSWORD***")),
            "lines: {lines:?}"
        );
    }

    /// An environment overlay's declared groups belong to its *base*, not to
    /// the derived deploy key: resolving them under the key would find
    /// nothing and quietly stop masking those values.
    #[tokio::test]
    async fn an_environments_groups_resolve_under_its_base_project() {
        let mut projects = MockProjectRepository::new();
        projects.expect_get().returning(|_| {
            Ok(Some(project(
                "rateme--preview",
                Some("rateme"),
                &["common"],
            )))
        });
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "common"))
            .returning(|_| Ok(group_with(&[("SHARED", "shared-secret-value")])));
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme--preview"))
            .returning(|_| Ok(SecretGroup::default()));
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_logs().returning(|_, _, _, log| {
            log.line("boot: shared-secret-value");
            Ok(())
        });

        let sink = CollectSink::new();
        build(projects, secrets, runtime)
            .execute("rateme--preview", 100, false, sink.clone())
            .await
            .unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("***SHARED***")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("shared-secret-value")));
    }

    /// `rpi logs` is a reader, not an injection: a group declared but never
    /// pushed must not take the log stream down with it (`effective_secrets`
    /// would return `NotFound` here). The key bundle's values stay masked.
    #[tokio::test]
    async fn a_declared_but_empty_group_does_not_break_the_log_stream() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .returning(|_| Ok(Some(project("rateme", None, &["missing"]))));
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "missing"))
            .returning(|_| Ok(SecretGroup::default()));
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| Ok(group_with(&[("TOKEN", "own-secret-value")])));
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_logs().times(1).returning(|_, _, _, log| {
            log.line("using own-secret-value");
            Ok(())
        });

        let sink = CollectSink::new();
        build(projects, secrets, runtime)
            .execute("rateme", 100, false, sink.clone())
            .await
            .unwrap();

        let lines = sink.lines.lock().unwrap();
        assert!(lines.iter().any(|l| l.contains("***TOKEN***")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("own-secret-value")));
    }

    #[tokio::test]
    async fn an_unregistered_project_is_not_found() {
        let mut projects = MockProjectRepository::new();
        projects.expect_get().returning(|_| Ok(None));
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_logs().times(0);

        let err = build(projects, MockSecretStore::new(), runtime)
            .execute("ghost", 100, false, CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
    }
}

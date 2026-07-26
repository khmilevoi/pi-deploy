use std::sync::Arc;

use pi_domain::contracts::{
    ContainerRuntime, DeploymentHistory, Ingress, LogSink, OverrideStore, ProjectRepository,
    SecretStore, Source,
};
use pi_domain::entities::ComposeStack;
use pi_domain::error::DomainError;
use pi_domain::secretgroup::GroupRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    pub project: String,
    pub hostname: Option<String>,
    pub volumes_removed: bool,
}

pub struct RemoveProject {
    projects: Arc<dyn ProjectRepository>,
    history: Arc<dyn DeploymentHistory>,
    runtime: Arc<dyn ContainerRuntime>,
    ingress: Arc<dyn Ingress>,
    source: Arc<dyn Source>,
    secrets: Arc<dyn SecretStore>,
    overrides: Arc<dyn OverrideStore>,
}

impl RemoveProject {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        history: Arc<dyn DeploymentHistory>,
        runtime: Arc<dyn ContainerRuntime>,
        ingress: Arc<dyn Ingress>,
        source: Arc<dyn Source>,
        secrets: Arc<dyn SecretStore>,
        overrides: Arc<dyn OverrideStore>,
    ) -> Arc<RemoveProject> {
        Arc::new(RemoveProject {
            projects,
            history,
            runtime,
            ingress,
            source,
            secrets,
            overrides,
        })
    }

    pub async fn execute(
        &self,
        project: &str,
        remove_volumes: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<RemoveReport, DomainError> {
        let existing = self
            .projects
            .get(project)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("project {project}")))?;
        let active = self.history.active(project).await?;
        if !active.is_empty() {
            return Err(DomainError::Conflict(format!(
                "project {project} has active deployment; cancel it first using `rpi deploy --cancel`"
            )));
        }

        let workdir = self.source.workdir(project);
        let compose_file = workdir.join(&existing.config.compose_path);
        if compose_file.exists() {
            let stack = ComposeStack {
                project_name: existing.config.name.clone(),
                workdir,
                compose_file,
                override_file: self.overrides.path(project),
                // No deploy is in flight, so RPI_COMMIT_SHA comes from the
                // registry's record of the last successful one.
                env: pi_domain::runtimevars::rpi_vars(&existing, None),
            };
            self.runtime
                .down(&stack, remove_volumes, Arc::clone(&log))
                .await?;
        }
        if let Some(hostname) = &existing.config.hostname {
            self.ingress.remove(hostname, Arc::clone(&log)).await?;
        }
        self.source.cleanup(project).await?;
        self.secrets.remove(&GroupRef::key(project)).await?;
        // Declared groups belong to the base project, so they go with it —
        // but never with an environment, whose teardown must leave the shared
        // groups its siblings attach untouched.
        if existing.config.environment.is_none() {
            self.secrets.remove_base(project).await?;
        }
        self.overrides.remove(project).await?;
        self.history.remove_project(project).await?;
        self.projects.remove(project).await?;

        Ok(RemoveReport {
            project: project.to_string(),
            hostname: existing.config.hostname,
            volumes_removed: remove_volumes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        MockContainerRuntime, MockDeploymentHistory, MockIngress, MockOverrideStore,
        MockProjectRepository, MockSecretStore, MockSource,
    };
    use pi_domain::entities::{EnvironmentMeta, Project, ProjectConfig};

    fn env_meta() -> EnvironmentMeta {
        EnvironmentMeta {
            env: "test".into(),
            base: "myapp".into(),
            slug: None,
            ttl_secs: None,
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
                hostname: None,
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
            last_commit_sha: None,
        }
    }

    /// `RemoveProject::execute` wired with the given `secrets` store and
    /// permissive mocks for every other dependency — `hostname: None` keeps
    /// ingress untouched and the workdir points at a directory with no
    /// compose file, so the container-runtime step is skipped too. Only the
    /// secrets-store expectations set by the caller are asserted.
    async fn remove_project_with(
        secrets: MockSecretStore,
        proj: Project,
    ) -> Result<RemoveReport, DomainError> {
        let key = proj.config.name.clone();

        let mut projects = MockProjectRepository::new();
        let get_proj = proj.clone();
        projects
            .expect_get()
            .returning(move |_| Ok(Some(get_proj.clone())));
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
        source
            .expect_workdir()
            .returning(|_| std::env::temp_dir().join("pi-application-remove-test-missing-compose"));
        source.expect_cleanup().times(1).returning(|_| Ok(()));
        let source: Arc<dyn Source> = Arc::new(source);

        let mut overrides = MockOverrideStore::new();
        overrides.expect_remove().times(1).returning(|_| Ok(()));
        let overrides: Arc<dyn OverrideStore> = Arc::new(overrides);

        let runtime: Arc<dyn ContainerRuntime> = Arc::new(MockContainerRuntime::new());
        let ingress: Arc<dyn Ingress> = Arc::new(MockIngress::new());

        let remove = RemoveProject::new(
            projects,
            history,
            runtime,
            ingress,
            source,
            Arc::new(secrets),
            overrides,
        );
        remove.execute(&key, false, CollectSink::new()).await
    }

    #[tokio::test]
    async fn removing_a_base_project_drops_its_declared_groups() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_remove()
            .withf(|r| *r == GroupRef::key("myapp"))
            .times(1)
            .returning(|_| Ok(()));
        secrets
            .expect_remove_base()
            .withf(|base| base == "myapp")
            .times(1)
            .returning(|_| Ok(()));

        remove_project_with(secrets, project("myapp", None))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn removing_an_environment_keeps_the_base_groups() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_remove()
            .withf(|r| *r == GroupRef::key("myapp--branch--x"))
            .times(1)
            .returning(|_| Ok(()));
        // The whole point of groups: tearing down one environment must not
        // take the shared secrets every other environment attaches.
        secrets.expect_remove_base().times(0);

        remove_project_with(secrets, project("myapp--branch--x", Some(env_meta())))
            .await
            .unwrap();
    }
}

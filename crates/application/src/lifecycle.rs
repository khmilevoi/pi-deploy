use std::sync::Arc;

use pi_domain::contracts::{ContainerRuntime, LogSink, OverrideStore, ProjectRepository, Source};
use pi_domain::entities::{ComposeStack, LifecycleAction};
use pi_domain::error::DomainError;

pub struct ControlLifecycle {
    projects: Arc<dyn ProjectRepository>,
    runtime: Arc<dyn ContainerRuntime>,
    source: Arc<dyn Source>,
    overrides: Arc<dyn OverrideStore>,
}

impl ControlLifecycle {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        runtime: Arc<dyn ContainerRuntime>,
        source: Arc<dyn Source>,
        overrides: Arc<dyn OverrideStore>,
    ) -> Arc<ControlLifecycle> {
        Arc::new(ControlLifecycle {
            projects,
            runtime,
            source,
            overrides,
        })
    }

    pub async fn execute(
        &self,
        project: &str,
        action: LifecycleAction,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        let registered = self
            .projects
            .get(project)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("project {project}")))?;
        let workdir = self.source.workdir(project);
        let compose_file = workdir.join(&registered.config.compose_path);
        let override_file = self.overrides.path(project);
        let stack = ComposeStack {
            project_name: registered.config.name.clone(),
            workdir,
            compose_file,
            override_file,
            // No deploy is in flight, so RPI_COMMIT_SHA comes from the
            // registry's record of the last successful one.
            env: pi_domain::runtimevars::rpi_vars(&registered, None),
        };
        self.runtime.lifecycle(&stack, action, log).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        MockContainerRuntime, MockOverrideStore, MockProjectRepository, MockSource,
    };
    use pi_domain::entities::{Project, ProjectConfig};
    use std::path::PathBuf;

    fn project(name: &str) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
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
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
            last_commit_sha: None,
        }
    }

    fn deps_with(runtime: MockContainerRuntime, proj: Project) -> Arc<ControlLifecycle> {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .returning(move |_| Ok(Some(proj.clone())));
        let mut source = MockSource::new();
        source
            .expect_workdir()
            .returning(|name| PathBuf::from("/data").join(name));
        let mut overrides = MockOverrideStore::new();
        overrides
            .expect_path()
            .returning(|name| PathBuf::from("/overrides").join(name));
        ControlLifecycle::new(
            Arc::new(projects),
            Arc::new(runtime),
            Arc::new(source),
            Arc::new(overrides),
        )
    }

    #[tokio::test]
    async fn lifecycle_carries_the_runtime_environment() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_lifecycle()
            .withf(|stack, _, _| stack.env["RPI_PROJECT"] == "rateme")
            .times(1)
            .returning(|_, _, _| Ok(()));
        let uc = deps_with(runtime, project("rateme"));
        uc.execute("rateme", LifecycleAction::Restart, CollectSink::new())
            .await
            .unwrap();
    }
}

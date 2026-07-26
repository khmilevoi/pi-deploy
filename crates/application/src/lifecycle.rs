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
            env: Default::default(),
        };
        self.runtime.lifecycle(&stack, action, log).await
    }
}

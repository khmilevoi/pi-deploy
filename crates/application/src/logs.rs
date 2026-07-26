use std::sync::Arc;

use pi_domain::contracts::{ContainerRuntime, LogSink, ProjectRepository, SecretStore};
use pi_domain::error::DomainError;
use pi_domain::secretgroup::GroupRef;

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

    pub async fn execute(
        &self,
        project: &str,
        tail: usize,
        follow: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        self.ensure_project(project).await?;
        let mask = MaskingSink::new(log);
        mask.arm(&self.secrets.load(&GroupRef::key(project)).await?.objects);
        self.runtime.logs(project, tail, follow, mask).await
    }
}

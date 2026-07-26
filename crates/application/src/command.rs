use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pi_domain::contracts::{ContainerRuntime, LogSink, OverrideStore, ProjectRepository, Source};
use pi_domain::entities::{CommandSpec, ComposeStack, Project};
use pi_domain::error::DomainError;

/// Agent-side budget for one command run when the project sets no
/// [timeouts].command override.
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 600;

/// Runs a [commands] entry inside the project's service container (§spec).
/// Allowlist semantics: only commands registered at deploy time exist here —
/// there is deliberately no way to execute an arbitrary argv.
pub struct RunCommand {
    projects: Arc<dyn ProjectRepository>,
    runtime: Arc<dyn ContainerRuntime>,
    source: Arc<dyn Source>,
    overrides: Arc<dyn OverrideStore>,
}

impl RunCommand {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        runtime: Arc<dyn ContainerRuntime>,
        source: Arc<dyn Source>,
        overrides: Arc<dyn OverrideStore>,
    ) -> Arc<RunCommand> {
        Arc::new(RunCommand {
            projects,
            runtime,
            source,
            overrides,
        })
    }

    /// Deployed commands of a project — `rpi command` list mode.
    pub async fn list(&self, project: &str) -> Result<BTreeMap<String, CommandSpec>, DomainError> {
        Ok(self.registered(project).await?.config.commands)
    }

    /// Existence check so the HTTP layer can 404 before opening the SSE
    /// stream (same idea as StreamLogs::ensure_project).
    pub async fn resolve(&self, project: &str, command: &str) -> Result<(), DomainError> {
        self.lookup(project, command).await.map(|_| ())
    }

    /// Runs declared argv + extra args in the service container.
    /// Returns the in-container exit code; nonzero is data, not an error.
    pub async fn execute(
        &self,
        project: &str,
        command: &str,
        extra_args: &[String],
        log: Arc<dyn LogSink>,
    ) -> Result<i32, DomainError> {
        let (registered, spec) = self.lookup(project, command).await?;
        let mut argv = spec.argv.clone();
        argv.extend(extra_args.iter().cloned());
        let service = spec
            .service
            .as_deref()
            .unwrap_or(&registered.config.service)
            .to_string();
        let workdir = self.source.workdir(project);
        let compose_file = workdir.join(&registered.config.compose_path);
        let override_file = self.overrides.path(project);
        let stack = ComposeStack {
            project_name: registered.config.name.clone(),
            workdir,
            compose_file,
            override_file,
        };
        let secs = registered
            .config
            .command_timeout_secs
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        tokio::time::timeout(
            Duration::from_secs(secs),
            self.runtime.exec(&stack, &service, &argv, log),
        )
        .await
        .map_err(|_| DomainError::Timeout {
            stage: "command".into(),
            secs,
        })?
    }

    async fn registered(&self, project: &str) -> Result<Project, DomainError> {
        self.projects
            .get(project)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("project {project}")))
    }

    async fn lookup(
        &self,
        project: &str,
        command: &str,
    ) -> Result<(Project, CommandSpec), DomainError> {
        let registered = self.registered(project).await?;
        match registered.config.commands.get(command) {
            Some(spec) => {
                let spec = spec.clone();
                Ok((registered, spec))
            }
            None => {
                let available: Vec<&str> = registered
                    .config
                    .commands
                    .keys()
                    .map(String::as_str)
                    .collect();
                Err(DomainError::NotFound(if available.is_empty() {
                    format!(
                        "command '{command}' (project has no deployed [commands]; declare it in rpi.toml and run `rpi deploy`)"
                    )
                } else {
                    format!("command '{command}' (available: {})", available.join(", "))
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        MockContainerRuntime, MockOverrideStore, MockProjectRepository, MockSource,
    };
    use pi_domain::entities::CommandSpec;
    use pi_domain::entities::ProjectConfig;
    use std::path::PathBuf;

    fn project(name: &str) -> Project {
        let mut config = ProjectConfig {
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
        };
        config.commands.insert(
            "create-invite".into(),
            CommandSpec::new(vec!["node".into(), "scripts/create-invite.js".into()]),
        );
        Project {
            config,
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
        }
    }

    fn deps_with(runtime: MockContainerRuntime, proj: Project) -> Arc<RunCommand> {
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
        RunCommand::new(
            Arc::new(projects),
            Arc::new(runtime),
            Arc::new(source),
            Arc::new(overrides),
        )
    }

    #[tokio::test]
    async fn executes_declared_argv_plus_extra_args_in_service() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_exec()
            .withf(|stack, service, argv, _| {
                stack.project_name == "rateme"
                    && service == "web"
                    && argv == ["node", "scripts/create-invite.js", "--email", "x@y.com"]
            })
            .returning(|_, _, _, _| Ok(42));

        let run = deps_with(runtime, project("rateme"));
        let code = run
            .execute(
                "rateme",
                "create-invite",
                &["--email".into(), "x@y.com".into()],
                CollectSink::new(),
            )
            .await
            .unwrap();
        assert_eq!(code, 42, "exit code propagates untouched");
    }

    #[tokio::test]
    async fn execs_into_pinned_service_when_set() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_exec()
            .withf(|_, service, _, _| service == "server")
            .returning(|_, _, _, _| Ok(0));

        let mut proj = project("rateme");
        proj.config.commands.insert(
            "create-invite".into(),
            CommandSpec {
                argv: vec!["node".into(), "create-invite.cjs".into()],
                service: Some("server".into()),
            },
        );
        let run = deps_with(runtime, proj);
        let code = run
            .execute("rateme", "create-invite", &[], CollectSink::new())
            .await
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn unknown_project_is_not_found() {
        let mut projects = MockProjectRepository::new();
        projects.expect_get().returning(|_| Ok(None));
        let run = RunCommand::new(
            Arc::new(projects),
            Arc::new(MockContainerRuntime::new()),
            Arc::new(MockSource::new()),
            Arc::new(MockOverrideStore::new()),
        );
        let err = run
            .execute("ghost", "x", &[], CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)));
    }

    #[tokio::test]
    async fn unknown_command_lists_available_names() {
        let run = deps_with(MockContainerRuntime::new(), project("rateme"));
        let err = run
            .execute("rateme", "nope", &[], CollectSink::new())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("available"), "got: {msg}");
        assert!(msg.contains("create-invite"), "got: {msg}");
    }

    #[tokio::test]
    async fn list_returns_deployed_commands() {
        let run = deps_with(MockContainerRuntime::new(), project("rateme"));
        let commands = run.list("rateme").await.unwrap();
        assert!(commands.contains_key("create-invite"));
    }

    #[tokio::test]
    async fn timeout_kills_the_run_and_reports_stage_command() {
        struct HangingRuntime;
        #[async_trait::async_trait]
        impl ContainerRuntime for HangingRuntime {
            async fn build(
                &self,
                _: &ComposeStack,
                _: Arc<dyn LogSink>,
            ) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn up(&self, _: &ComposeStack, _: Arc<dyn LogSink>) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn ps(
                &self,
                _: &str,
            ) -> Result<Vec<pi_domain::entities::ServiceState>, DomainError> {
                unimplemented!()
            }
            async fn prune_images(&self, _: Arc<dyn LogSink>) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn prune_builder(&self, _: Arc<dyn LogSink>) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn logs(
                &self,
                _: &str,
                _: usize,
                _: bool,
                _: Arc<dyn LogSink>,
            ) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn stats(
                &self,
                _: &str,
            ) -> Result<Vec<pi_domain::entities::ServiceStats>, DomainError> {
                unimplemented!()
            }
            async fn lifecycle(
                &self,
                _: &ComposeStack,
                _: pi_domain::entities::LifecycleAction,
                _: Arc<dyn LogSink>,
            ) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn down(
                &self,
                _: &ComposeStack,
                _: bool,
                _: Arc<dyn LogSink>,
            ) -> Result<(), DomainError> {
                unimplemented!()
            }
            async fn exec(
                &self,
                _: &ComposeStack,
                _: &str,
                _: &[String],
                _: Arc<dyn LogSink>,
            ) -> Result<i32, DomainError> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(0)
            }
        }

        let mut proj = project("rateme");
        proj.config.command_timeout_secs = Some(0);
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
        let run = RunCommand::new(
            Arc::new(projects),
            Arc::new(HangingRuntime),
            Arc::new(source),
            Arc::new(overrides),
        );

        let err = run
            .execute("rateme", "create-invite", &[], CollectSink::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, DomainError::Timeout { ref stage, .. } if stage == "command"),
            "got: {err}"
        );
    }
}

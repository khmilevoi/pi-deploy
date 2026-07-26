use std::sync::Arc;

use pi_domain::contracts::{
    ContainerRuntime, LogSink, OverrideStore, ProjectRepository, SecretStore, SecretsWriter, Source,
};
use pi_domain::entities::{ComposeStack, SecretsBundle};
use pi_domain::error::DomainError;
use pi_domain::secretgroup::GroupRef;

use crate::mask::MaskingSink;

/// Result of `rpi secrets send` (§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsSaved {
    pub keys: usize,
    pub files: usize,
    pub applied: bool,
}

/// Accept and store a SecretsBundle; with `apply` re-injects `.env` + secret
/// files and runs `up -d` so compose recreates only the affected services
/// (§7, §10).
pub struct SendSecrets {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
    source: Arc<dyn Source>,
    writer: Arc<dyn SecretsWriter>,
    overrides: Arc<dyn OverrideStore>,
    runtime: Arc<dyn ContainerRuntime>,
}

impl SendSecrets {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
        source: Arc<dyn Source>,
        writer: Arc<dyn SecretsWriter>,
        overrides: Arc<dyn OverrideStore>,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> Arc<SendSecrets> {
        Arc::new(SendSecrets {
            secrets,
            projects,
            source,
            writer,
            overrides,
            runtime,
        })
    }

    pub async fn execute(
        &self,
        project: &str,
        bundle: SecretsBundle,
        apply: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<SecretsSaved, DomainError> {
        if bundle.is_empty() {
            return Err(DomainError::Invalid("secrets bundle is empty".into()));
        }
        self.secrets
            .save(&GroupRef::key(project), &bundle, None)
            .await?;
        let keys = bundle.vars.len();
        let files = bundle.files.len();
        if !apply {
            return Ok(SecretsSaved {
                keys,
                files,
                applied: false,
            });
        }

        let registered = self.projects.get(project).await?.ok_or_else(|| {
            DomainError::NotFound(format!(
                "project '{project}' is not deployed yet; run `rpi deploy` first"
            ))
        })?;
        let config = &registered.config;

        // mask the freshly received values in the `up` output (§8.1)
        let masker = MaskingSink::new(log);
        masker.arm(&bundle);
        let log: Arc<dyn LogSink> = masker;

        let workdir = self.source.workdir(project);
        self.writer.write(&workdir, &bundle).await?;
        log.line(&format!(
            "secrets applied ({} keys, {} files, mode {:04o})",
            keys,
            files,
            bundle.secret_file_mode()
        ));
        let override_file = self
            .overrides
            .write(
                project,
                &config.service,
                registered.config.expose.bind_addr(),
                registered.host_port,
                config.container_port,
            )
            .await?;
        let stack = ComposeStack {
            project_name: config.name.clone(),
            workdir: workdir.clone(),
            compose_file: workdir.join(&config.compose_path),
            override_file,
        };
        self.runtime.up(&stack, log).await?;
        Ok(SecretsSaved {
            keys,
            files,
            applied: true,
        })
    }
}

/// Key names and file paths only, never values or file contents (§10:
/// `rpi secrets ls`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredSecrets {
    pub keys: Vec<String>,
    pub files: Vec<String>,
    pub file_mode: u32,
}

pub struct ListSecrets {
    secrets: Arc<dyn SecretStore>,
}

impl ListSecrets {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<ListSecrets> {
        Arc::new(ListSecrets { secrets })
    }

    pub async fn execute(&self, project: &str) -> Result<StoredSecrets, DomainError> {
        let bundle = self.secrets.load(&GroupRef::key(project)).await?.objects;
        Ok(StoredSecrets {
            keys: bundle.keys(),
            files: bundle.file_paths(),
            file_mode: bundle.secret_file_mode(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::CollectSink;
    use pi_domain::contracts::{
        MockContainerRuntime, MockOverrideStore, MockProjectRepository, MockSecretStore,
        MockSecretsWriter, MockSource,
    };
    use pi_domain::entities::{
        ExposeMode, HealthcheckConfig, Project, ProjectConfig, StageTimeoutOverrides,
    };
    use pi_domain::secretgroup::SecretGroup;
    use std::path::{Path, PathBuf};

    fn bundle() -> SecretsBundle {
        let mut b = SecretsBundle::default();
        b.vars.insert("DB_PASSWORD".into(), "hunter2-long".into());
        b.vars.insert("PORT".into(), "3000".into());
        b.files
            .insert("certs/server.pem".into(), b"PEM-BODY".to_vec());
        b
    }

    fn registered() -> Project {
        Project {
            config: ProjectConfig {
                name: "rateme".into(),
                repo: "git@github.com:x/rateme.git".into(),
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
                environment: None,
                secret_groups: Vec::new(),
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
        }
    }

    struct Mocks {
        secrets: MockSecretStore,
        projects: MockProjectRepository,
        source: MockSource,
        writer: MockSecretsWriter,
        overrides: MockOverrideStore,
        runtime: MockContainerRuntime,
    }

    fn mocks() -> Mocks {
        Mocks {
            secrets: MockSecretStore::new(),
            projects: MockProjectRepository::new(),
            source: MockSource::new(),
            writer: MockSecretsWriter::new(),
            overrides: MockOverrideStore::new(),
            runtime: MockContainerRuntime::new(),
        }
    }

    fn build(m: Mocks) -> Arc<SendSecrets> {
        SendSecrets::new(
            Arc::new(m.secrets),
            Arc::new(m.projects),
            Arc::new(m.source),
            Arc::new(m.writer),
            Arc::new(m.overrides),
            Arc::new(m.runtime),
        )
    }

    #[tokio::test]
    async fn save_without_apply_only_stores_bundle() {
        let mut m = mocks();
        m.secrets
            .expect_save()
            .withf(|r, b, _| {
                *r == GroupRef::key("rateme") && b.vars.len() == 2 && b.files.len() == 1
            })
            .times(1)
            .returning(|_, _, _| Ok(1));

        let saved = build(m)
            .execute("rateme", bundle(), false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(
            saved,
            SecretsSaved {
                keys: 2,
                files: 1,
                applied: false
            }
        );
    }

    #[tokio::test]
    async fn empty_bundle_is_invalid_and_not_saved() {
        let mut m = mocks();
        m.secrets.expect_save().times(0);
        let err = build(m)
            .execute(
                "rateme",
                SecretsBundle::default(),
                false,
                CollectSink::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
    }

    #[tokio::test]
    async fn files_only_bundle_is_saved() {
        let mut m = mocks();
        m.secrets.expect_save().times(1).returning(|_, _, _| Ok(1));
        let mut b = SecretsBundle::default();
        b.files.insert("id_rsa".into(), b"key".to_vec());
        let saved = build(m)
            .execute("rateme", b, false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(
            saved,
            SecretsSaved {
                keys: 0,
                files: 1,
                applied: false
            }
        );
    }

    #[tokio::test]
    async fn apply_reinjects_env_and_runs_up_with_masked_logs() {
        let mut m = mocks();
        m.secrets.expect_save().returning(|_, _, _| Ok(1));
        m.projects
            .expect_get()
            .withf(|n| n == "rateme")
            .returning(|_| Ok(Some(registered())));
        m.source
            .expect_workdir()
            .withf(|n| n == "rateme")
            .returning(|_| PathBuf::from("/wd/rateme"));
        m.writer
            .expect_write()
            .withf(|wd, b| wd == Path::new("/wd/rateme") && b.vars.len() == 2 && b.files.len() == 1)
            .times(1)
            .returning(|_, _| Ok(()));
        m.overrides
            .expect_write()
            .withf(|p, s, bind, hp, cp| {
                p == "rateme" && s == "web" && bind == "127.0.0.1" && *hp == 8000 && *cp == 3000
            })
            .times(1)
            .returning(|_, _, _, _, _| Ok(PathBuf::from("/ov/rateme.yml")));
        m.runtime
            .expect_up()
            .withf(|stack, _| {
                stack.project_name == "rateme"
                    && stack.workdir == Path::new("/wd/rateme")
                    && stack.compose_file == Path::new("/wd/rateme/docker-compose.yml")
                    && stack.override_file == Path::new("/ov/rateme.yml")
            })
            .times(1)
            .returning(|_, log| {
                log.line("recreating with hunter2-long");
                Ok(())
            });

        let sink = CollectSink::new();
        let saved = build(m)
            .execute("rateme", bundle(), true, sink.clone())
            .await
            .unwrap();

        assert_eq!(
            saved,
            SecretsSaved {
                keys: 2,
                files: 1,
                applied: true
            }
        );
        let lines = sink.lines.lock().unwrap();
        assert!(
            lines.iter().any(|l| l.contains("***DB_PASSWORD***")),
            "lines: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("hunter2-long")),
            "secret leaked"
        );
        assert!(
            lines.iter().any(|l| l.contains("mode 0644")),
            "the applied mode must be visible in the log: {lines:?}"
        );
    }

    #[tokio::test]
    async fn apply_for_unknown_project_is_not_found_after_save() {
        let mut m = mocks();
        m.secrets.expect_save().times(1).returning(|_, _, _| Ok(1));
        m.projects.expect_get().returning(|_| Ok(None));
        m.writer.expect_write().times(0);
        m.runtime.expect_up().times(0);

        let err = build(m)
            .execute("rateme", bundle(), true, CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
    }

    #[tokio::test]
    async fn list_secrets_returns_key_names_and_file_paths_only() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: bundle(),
                    revision: 1,
                })
            });
        let stored = ListSecrets::new(Arc::new(secrets))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(
            stored.keys,
            vec!["DB_PASSWORD".to_string(), "PORT".to_string()]
        );
        assert_eq!(stored.files, vec!["certs/server.pem".to_string()]);
    }

    #[tokio::test]
    async fn list_secrets_reports_the_effective_file_mode() {
        let mut secrets = MockSecretStore::new();
        secrets.expect_load().returning(|_| {
            Ok(SecretGroup {
                objects: bundle(),
                revision: 1,
            })
        });
        let stored = ListSecrets::new(Arc::new(secrets))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(stored.file_mode, 0o644);
    }
}

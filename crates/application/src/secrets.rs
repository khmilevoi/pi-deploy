use std::sync::Arc;

use pi_domain::contracts::{
    ContainerRuntime, LogSink, OverrideStore, ProjectRepository, SecretStore, SecretsWriter, Source,
};
use pi_domain::entities::{ComposeStack, SecretsBundle};
use pi_domain::error::DomainError;
use pi_domain::secretgroup::{merge_layers, GroupHead, GroupRef, Layer};

use crate::mask::MaskingSink;

/// Result of `rpi secrets send` (§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsSaved {
    pub keys: usize,
    pub files: usize,
    pub applied: bool,
    pub revision: u64,
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
        expected: Option<u64>,
        apply: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<SecretsSaved, DomainError> {
        if bundle.is_empty() {
            return Err(DomainError::Invalid("secrets bundle is empty".into()));
        }
        let revision = self
            .secrets
            .save(&GroupRef::key(project), &bundle, expected)
            .await?;
        let keys = bundle.vars.len();
        let files = bundle.files.len();
        if !apply {
            return Ok(SecretsSaved {
                keys,
                files,
                applied: false,
                revision,
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
            revision,
        })
    }
}

/// Re-injects a deploy key's effective secrets (declared groups merged under
/// its own bundle) and runs `up -d` so compose recreates only the affected
/// services. Used by `rpi secrets push --apply` on both the group and the
/// per-key path — `SendSecrets` delegates here after storing, rather than
/// keeping a second copy of this orchestration.
pub struct ApplySecrets {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
    source: Arc<dyn Source>,
    writer: Arc<dyn SecretsWriter>,
    overrides: Arc<dyn OverrideStore>,
    runtime: Arc<dyn ContainerRuntime>,
}

impl ApplySecrets {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
        source: Arc<dyn Source>,
        writer: Arc<dyn SecretsWriter>,
        overrides: Arc<dyn OverrideStore>,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> Arc<ApplySecrets> {
        Arc::new(ApplySecrets {
            secrets,
            projects,
            source,
            writer,
            overrides,
            runtime,
        })
    }

    /// Returns the merged key/file counts actually written.
    pub async fn execute(
        &self,
        project: &str,
        log: Arc<dyn LogSink>,
    ) -> Result<(usize, usize), DomainError> {
        let registered = self.projects.get(project).await?.ok_or_else(|| {
            DomainError::NotFound(format!(
                "project '{project}' is not deployed yet; run `rpi deploy` first"
            ))
        })?;
        let config = &registered.config;
        let merged = crate::effective_secrets(self.secrets.as_ref(), config).await?;

        let masker = MaskingSink::new(log);
        masker.arm(&merged.bundle);
        let log: Arc<dyn LogSink> = masker;

        let workdir = self.source.workdir(project);
        self.writer.write(&workdir, &merged.bundle).await?;
        let provenance: Vec<String> = merged
            .revisions
            .iter()
            .map(|(label, r)| format!("{label}@r{r}"))
            .collect();
        log.line(&format!(
            "secrets injected ({} keys, {} files, mode {:04o}; groups: {})",
            merged.bundle.vars.len(),
            merged.bundle.files.len(),
            merged.bundle.secret_file_mode(),
            provenance.join(", ")
        ));
        let override_file = self
            .overrides
            .write(
                project,
                &config.service,
                config.expose.bind_addr(),
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
        Ok((merged.bundle.vars.len(), merged.bundle.files.len()))
    }
}

/// Key names and file paths only, never values or file contents (§10:
/// `rpi secrets ls`). `keys`/`files`/`file_mode` are the *effective* (merged)
/// view — every declared group plus the deploy key's own bundle, later layers
/// winning — matching what a deploy would inject.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredSecrets {
    pub keys: Vec<String>,
    pub files: Vec<String>,
    pub file_mode: u32,
    /// One entry per layer, in the order the deploy would apply them: label,
    /// revision, and that layer's *own* var/file names (not just the ones
    /// that survive the merge) — the raw membership `rpi secrets ls` needs to
    /// show which layer shadowed which.
    pub layers: Vec<(String, u64, Vec<String>, Vec<String>)>,
}

pub struct ListSecrets {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
}

impl ListSecrets {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Arc<ListSecrets> {
        Arc::new(ListSecrets { secrets, projects })
    }

    /// Resolves the same layers `rpi deploy` would (declared groups against
    /// the base project, then the deploy key's own bundle last) via
    /// `crate::load_layer_stack` — the same primitive `crate::effective_secrets`
    /// is built on, so the two can never resolve layers differently — but
    /// with `require_non_empty: false`: unlike an injection, a read-only
    /// listing must not fail when a declared group is empty, so an operator
    /// checking why a group is missing must still see everything else.
    ///
    /// This does not call `crate::effective_secrets` itself: that function
    /// always fails fast on an empty declared group (correct for an
    /// injection) and only returns the *winning* name per layer, not each
    /// layer's own raw membership, which is what the effective view's
    /// per-layer columns need.
    pub async fn execute(&self, project: &str) -> Result<StoredSecrets, DomainError> {
        let registered = self.projects.get(project).await?;
        let (base, declared_groups) = match &registered {
            Some(p) => (
                p.config
                    .environment
                    .as_ref()
                    .map(|e| e.base.clone())
                    .unwrap_or_else(|| p.config.name.clone()),
                p.config.secret_groups.clone(),
            ),
            None => (project.to_string(), Vec::new()),
        };

        let loaded = crate::load_layer_stack(
            self.secrets.as_ref(),
            &base,
            project,
            &declared_groups,
            false,
        )
        .await?;

        let layers: Vec<Layer<'_>> = loaded
            .iter()
            .map(|(label, group)| Layer::new(label, group))
            .collect();
        let merged = merge_layers(&layers, crate::MAX_MERGED_SECRET_BYTES)?;

        let layer_rows = loaded
            .into_iter()
            .map(|(label, group)| {
                let vars = group.objects.keys();
                let files = group.objects.file_paths();
                (label, group.revision, vars, files)
            })
            .collect();

        Ok(StoredSecrets {
            keys: merged.bundle.keys(),
            files: merged.bundle.file_paths(),
            file_mode: merged.bundle.secret_file_mode(),
            layers: layer_rows,
        })
    }
}

/// `GET /v1/projects/{name}/secrets/head` — metadata of a deploy key's own
/// bundle only (never the groups it merges with). Kept one line so the HTTP
/// layer never touches the store directly.
pub struct HeadKeySecrets {
    secrets: Arc<dyn SecretStore>,
}

impl HeadKeySecrets {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<HeadKeySecrets> {
        Arc::new(HeadKeySecrets { secrets })
    }

    pub async fn execute(&self, project: &str) -> Result<GroupHead, DomainError> {
        self.secrets.head(&GroupRef::key(project)).await
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
        EnvironmentMeta, ExposeMode, HealthcheckConfig, Project, ProjectConfig,
        StageTimeoutOverrides,
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

    fn build_apply(m: Mocks) -> Arc<ApplySecrets> {
        ApplySecrets::new(
            Arc::new(m.secrets),
            Arc::new(m.projects),
            Arc::new(m.source),
            Arc::new(m.writer),
            Arc::new(m.overrides),
            Arc::new(m.runtime),
        )
    }

    /// Like `registered()`, but declaring `groups` in `[secrets].groups` so
    /// `effective_secrets` has layers to load beyond the key's own bundle.
    fn registered_with_groups(groups: &[&str]) -> Project {
        let mut p = registered();
        p.config.secret_groups = groups.iter().map(|g| (*g).to_string()).collect();
        p
    }

    /// An environment-overlay deploy key (`deploy_key`) whose `environment`
    /// block names the base project (`base`) that owns its declared groups —
    /// distinct from `registered_with_groups`, whose `config.name` and the
    /// implicit base are the same string and so cannot catch a resolver that
    /// mixes the two up.
    fn registered_environment_with_groups(
        deploy_key: &str,
        base: &str,
        groups: &[&str],
    ) -> Project {
        let mut p = registered_with_groups(groups);
        p.config.name = deploy_key.to_string();
        p.config.environment = Some(EnvironmentMeta {
            env: "preview".into(),
            base: base.to_string(),
            slug: None,
            ttl_secs: None,
            on_create: None,
        });
        p
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
            .execute("rateme", bundle(), None, false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(
            saved,
            SecretsSaved {
                keys: 2,
                files: 1,
                applied: false,
                revision: 1,
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
                None,
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
            .execute("rateme", b, None, false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(
            saved,
            SecretsSaved {
                keys: 0,
                files: 1,
                applied: false,
                revision: 1,
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
            .execute("rateme", bundle(), None, true, sink.clone())
            .await
            .unwrap();

        assert_eq!(
            saved,
            SecretsSaved {
                keys: 2,
                files: 1,
                applied: true,
                revision: 1,
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
            .execute("rateme", bundle(), None, true, CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
    }

    /// `ListSecrets::new` needs a `ProjectRepository` to resolve declared
    /// groups; a project that was never deployed simply has none, so a
    /// not-found `get` must not change the pre-groups behavior of these two
    /// tests (project's own bundle only).
    fn mock_projects_returning_none() -> MockProjectRepository {
        let mut projects = MockProjectRepository::new();
        projects.expect_get().returning(|_| Ok(None));
        projects
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
        let stored = ListSecrets::new(Arc::new(secrets), Arc::new(mock_projects_returning_none()))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(
            stored.keys,
            vec!["DB_PASSWORD".to_string(), "PORT".to_string()]
        );
        assert_eq!(stored.files, vec!["certs/server.pem".to_string()]);
        assert_eq!(
            stored.layers,
            vec![(
                "key".to_string(),
                1,
                vec!["DB_PASSWORD".to_string(), "PORT".to_string()],
                vec!["certs/server.pem".to_string()]
            )]
        );
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
        let stored = ListSecrets::new(Arc::new(secrets), Arc::new(mock_projects_returning_none()))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(stored.file_mode, 0o644);
    }

    /// `rpi secrets ls` must show the same merged, layered view `rpi deploy`
    /// injects — not just the deploy key's own bundle — with the declared
    /// group ordered before the key so a later layer visibly wins.
    #[tokio::test]
    async fn list_secrets_merges_declared_groups_under_the_key_bundle_and_reports_layers() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .withf(|n| n == "rateme")
            .returning(|_| Ok(Some(registered_with_groups(&["common"]))));
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "common"))
            .returning(|_| {
                let mut objects = SecretsBundle::default();
                objects.vars.insert("SHARED".into(), "shared-long".into());
                Ok(SecretGroup {
                    objects,
                    revision: 3,
                })
            });
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: bundle(),
                    revision: 5,
                })
            });

        let stored = ListSecrets::new(Arc::new(secrets), Arc::new(projects))
            .execute("rateme")
            .await
            .unwrap();

        assert_eq!(
            stored.keys,
            vec![
                "DB_PASSWORD".to_string(),
                "PORT".to_string(),
                "SHARED".to_string()
            ]
        );
        assert_eq!(stored.files, vec!["certs/server.pem".to_string()]);
        assert_eq!(
            stored.layers,
            vec![
                ("common".to_string(), 3, vec!["SHARED".to_string()], vec![]),
                (
                    "key".to_string(),
                    5,
                    vec!["DB_PASSWORD".to_string(), "PORT".to_string()],
                    vec!["certs/server.pem".to_string()]
                ),
            ]
        );
    }

    /// `registered_with_groups`'s deploy key and base are the same string, so
    /// it cannot catch a resolver that reads `config.name` where it should
    /// read `config.environment.base` (or vice versa). An environment
    /// overlay's declared groups belong to the *base* project, not the
    /// derived deploy key `execute` is called with — the deploy key's own
    /// bundle is still looked up by the deploy key itself.
    #[tokio::test]
    async fn list_secrets_loads_declared_groups_under_the_environment_base_not_the_deploy_key() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .withf(|n| n == "rateme--preview")
            .returning(|_| {
                Ok(Some(registered_environment_with_groups(
                    "rateme--preview",
                    "rateme",
                    &["common"],
                )))
            });
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "common"))
            .returning(|_| {
                let mut objects = SecretsBundle::default();
                objects.vars.insert("SHARED".into(), "shared-long".into());
                Ok(SecretGroup {
                    objects,
                    revision: 3,
                })
            });
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme--preview"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: bundle(),
                    revision: 5,
                })
            });

        let stored = ListSecrets::new(Arc::new(secrets), Arc::new(projects))
            .execute("rateme--preview")
            .await
            .unwrap();

        assert_eq!(
            stored.layers[0],
            ("common".to_string(), 3, vec!["SHARED".to_string()], vec![])
        );
        assert_eq!(
            stored.layers[1].0, "key",
            "the deploy key's own bundle is still keyed by the deploy key, not the base"
        );
    }

    /// Unlike `ApplySecrets`/`effective_secrets` (which must fail fast so a
    /// deploy never starts with a partially-injected bundle), `rpi secrets
    /// ls` is read-only: an operator debugging exactly this problem — "I
    /// declared a group but never pushed it" — must still see the rest of
    /// the effective view, not a 404.
    #[tokio::test]
    async fn list_secrets_does_not_fail_when_a_declared_group_is_empty() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_get()
            .returning(|_| Ok(Some(registered_with_groups(&["missing"]))));
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "missing"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: SecretsBundle::default(),
                    revision: 0,
                })
            });
        secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: bundle(),
                    revision: 1,
                })
            });

        let stored = ListSecrets::new(Arc::new(secrets), Arc::new(projects))
            .execute("rateme")
            .await
            .unwrap();

        assert_eq!(
            stored.keys,
            vec!["DB_PASSWORD".to_string(), "PORT".to_string()]
        );
        assert_eq!(stored.layers[0], ("missing".to_string(), 0, vec![], vec![]));
    }

    /// `ApplySecrets` must resolve the *same* effective view `rpi deploy`
    /// does — declared groups, then the key's own bundle, merged — not just
    /// re-inject whatever the caller last uploaded. It must also log the
    /// same shape of provenance line `deploy.rs` logs, so the agent journal
    /// says which layers (and revisions) actually landed.
    #[tokio::test]
    async fn apply_merges_declared_groups_under_the_key_bundle_and_logs_provenance() {
        let mut m = mocks();
        m.projects
            .expect_get()
            .withf(|n| n == "rateme")
            .returning(|_| Ok(Some(registered_with_groups(&["common"]))));
        m.secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "common"))
            .returning(|_| {
                let mut objects = SecretsBundle::default();
                objects.vars.insert("SHARED".into(), "shared-long".into());
                Ok(SecretGroup {
                    objects,
                    revision: 3,
                })
            });
        m.secrets
            .expect_load()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: bundle(),
                    revision: 5,
                })
            });
        m.source
            .expect_workdir()
            .withf(|n| n == "rateme")
            .returning(|_| PathBuf::from("/wd/rateme"));
        m.writer
            .expect_write()
            .withf(|wd, b| {
                wd == Path::new("/wd/rateme")
                    && b.vars.get("SHARED").map(String::as_str) == Some("shared-long")
                    && b.vars.get("DB_PASSWORD").map(String::as_str) == Some("hunter2-long")
                    && b.vars.len() == 3
                    && b.files.len() == 1
            })
            .times(1)
            .returning(|_, _| Ok(()));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _| Ok(PathBuf::from("/ov/rateme.yml")));
        m.runtime.expect_up().times(1).returning(|_, log| {
            log.line("recreating with hunter2-long and shared-long");
            Ok(())
        });

        let sink = CollectSink::new();
        let (keys, files) = build_apply(m)
            .execute("rateme", sink.clone())
            .await
            .unwrap();
        assert_eq!((keys, files), (3, 1));

        let lines = sink.lines.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("secrets injected") && l.contains("groups: common@r3, key@r5")),
            "lines: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("***DB_PASSWORD***")),
            "the merged bundle must be masked too: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("hunter2-long") || l.contains("shared-long")),
            "secret leaked: {lines:?}"
        );
    }

    /// A group named in `[secrets].groups` that was never pushed (or is
    /// empty) must fail `--apply`/`POST .../secrets/apply` the same way it
    /// fails `rpi deploy` — before anything is written to the checkout —
    /// rather than silently applying a partial, misconfigured set.
    #[tokio::test]
    async fn apply_fails_when_a_declared_group_has_no_secrets() {
        let mut m = mocks();
        m.projects
            .expect_get()
            .returning(|_| Ok(Some(registered_with_groups(&["missing"]))));
        m.secrets
            .expect_load()
            .withf(|r| *r == GroupRef::named("rateme", "missing"))
            .returning(|_| {
                Ok(SecretGroup {
                    objects: SecretsBundle::default(),
                    revision: 0,
                })
            });
        m.writer.expect_write().times(0);
        m.runtime.expect_up().times(0);

        let err = build_apply(m)
            .execute("rateme", CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound(_)), "got: {err}");
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[tokio::test]
    async fn head_key_secrets_returns_metadata_of_the_keys_own_bundle_only() {
        let mut secrets = MockSecretStore::new();
        secrets
            .expect_head()
            .withf(|r| *r == GroupRef::key("rateme"))
            .returning(|_| {
                let mut vars = std::collections::BTreeMap::new();
                vars.insert("DB_PASSWORD".to_string(), "abc123def456".to_string());
                Ok(GroupHead {
                    revision: 2,
                    vars,
                    files: Default::default(),
                    file_mode: None,
                })
            });

        let head = HeadKeySecrets::new(Arc::new(secrets))
            .execute("rateme")
            .await
            .unwrap();
        assert_eq!(head.revision, 2);
        assert_eq!(
            head.vars.get("DB_PASSWORD").map(String::as_str),
            Some("abc123def456")
        );
    }

    #[tokio::test]
    async fn send_passes_the_expected_revision_through_to_the_store() {
        let mut m = mocks();
        m.secrets
            .expect_save()
            .withf(|r, _, expected| *r == GroupRef::key("rateme") && *expected == Some(4))
            .times(1)
            .returning(|_, _, _| Ok(5));

        let saved = build(m)
            .execute("rateme", bundle(), Some(4), false, CollectSink::new())
            .await
            .unwrap();
        assert_eq!(saved.revision, 5);
    }

    #[tokio::test]
    async fn a_conflict_from_the_store_surfaces_unchanged() {
        let mut m = mocks();
        m.secrets.expect_save().returning(|_, _, _| {
            Err(DomainError::Conflict(
                "secret group changed since revision 1".into(),
            ))
        });
        let err = build(m)
            .execute("rateme", bundle(), Some(1), false, CollectSink::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
    }
}

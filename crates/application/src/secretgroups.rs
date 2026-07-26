//! Group CRUD (secret-groups spec: CLI surface, Wire protocol). Separate from
//! `secrets.rs`, which owns the per-deploy-key path and its `--apply`
//! orchestration: these use-cases never touch containers, and they are the
//! only place that joins the vault with the registry.

use std::sync::Arc;

use pi_domain::contracts::{ProjectRepository, SecretStore};
use pi_domain::entities::SecretsBundle;
use pi_domain::error::DomainError;
use pi_domain::secretgroup::{GroupHead, GroupRef, GroupSummary};

/// `PUT /v1/projects/{base}/secret-groups/{group}`.
pub struct PushSecretGroup {
    secrets: Arc<dyn SecretStore>,
}

impl PushSecretGroup {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<PushSecretGroup> {
        Arc::new(PushSecretGroup { secrets })
    }

    /// `merge` upserts the incoming objects onto what is stored; the default
    /// replaces the group wholesale, which is what keeps the group equal to
    /// the local sources it was pushed from. Either way the write is
    /// conditional on `expected` — `merge` weakens what is written, not the
    /// guard.
    pub async fn execute(
        &self,
        base: &str,
        name: &str,
        objects: SecretsBundle,
        expected: Option<u64>,
        merge: bool,
    ) -> Result<u64, DomainError> {
        if objects.is_empty() {
            return Err(DomainError::Invalid("secret group payload is empty".into()));
        }
        let r = GroupRef::named(base, name);
        let objects = if merge {
            let mut stored = self.secrets.load(&r).await?.objects;
            stored.vars.extend(objects.vars);
            stored.files.extend(objects.files);
            if objects.file_mode.is_some() {
                stored.file_mode = objects.file_mode;
            }
            stored
        } else {
            objects
        };
        self.secrets.save(&r, &objects, expected).await
    }
}

/// `GET /v1/projects/{base}/secret-groups/{group}` — metadata only.
pub struct ShowSecretGroup {
    secrets: Arc<dyn SecretStore>,
}

impl ShowSecretGroup {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Arc<ShowSecretGroup> {
        Arc::new(ShowSecretGroup { secrets })
    }

    pub async fn execute(&self, base: &str, name: &str) -> Result<GroupHead, DomainError> {
        let head = self.secrets.head(&GroupRef::named(base, name)).await?;
        if head.revision == 0 {
            return Err(DomainError::NotFound(format!(
                "secret group '{name}' of project '{base}'"
            )));
        }
        Ok(head)
    }
}

/// One row of `rpi secrets group ls`: the store's summary plus the registry
/// join the store cannot do itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedGroup {
    pub summary: GroupSummary,
    pub attached_by: Vec<String>,
}

/// `GET /v1/projects/{base}/secret-groups`.
pub struct ListSecretGroups {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
}

impl ListSecretGroups {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Arc<ListSecretGroups> {
        Arc::new(ListSecretGroups { secrets, projects })
    }

    pub async fn execute(&self, base: &str) -> Result<Vec<AttachedGroup>, DomainError> {
        let summaries = self.secrets.list(base).await?;
        let projects = self.projects.list().await?;
        Ok(summaries
            .into_iter()
            .map(|summary| {
                let attached_by = attachers(&projects, base, &summary.name);
                AttachedGroup {
                    summary,
                    attached_by,
                }
            })
            .collect())
    }
}

/// `DELETE /v1/projects/{base}/secret-groups/{group}`.
pub struct RemoveSecretGroup {
    secrets: Arc<dyn SecretStore>,
    projects: Arc<dyn ProjectRepository>,
}

impl RemoveSecretGroup {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Arc<RemoveSecretGroup> {
        Arc::new(RemoveSecretGroup { secrets, projects })
    }

    pub async fn execute(&self, base: &str, name: &str, force: bool) -> Result<(), DomainError> {
        if !force {
            let attached = attachers(&self.projects.list().await?, base, name);
            if !attached.is_empty() {
                return Err(DomainError::Conflict(format!(
                    "secret group '{name}' is declared by {}; \
                     their next deploy would fail without it (pass --force to delete anyway)",
                    attached.join(", ")
                )));
            }
        }
        self.secrets.remove(&GroupRef::named(base, name)).await
    }
}

/// Registered project keys whose config declares `name` and whose base is
/// `base`. A base project's own key counts: it declares groups too.
fn attachers(projects: &[pi_domain::entities::Project], base: &str, name: &str) -> Vec<String> {
    projects
        .iter()
        .filter(|p| {
            let p_base = p
                .config
                .environment
                .as_ref()
                .map(|e| e.base.as_str())
                .unwrap_or(p.config.name.as_str());
            p_base == base && p.config.secret_groups.iter().any(|g| g == name)
        })
        .map(|p| p.config.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_domain::contracts::{MockProjectRepository, MockSecretStore};
    use pi_domain::entities::{
        EnvironmentMeta, ExposeMode, HealthcheckConfig, Project, ProjectConfig,
        StageTimeoutOverrides,
    };
    use pi_domain::secretgroup::SecretGroup;

    fn objects(vars: &[(&str, &str)]) -> SecretsBundle {
        let mut b = SecretsBundle::default();
        for (k, v) in vars {
            b.vars.insert((*k).into(), (*v).into());
        }
        b
    }

    /// A registered project keyed `key`, declaring `groups` in its config.
    /// When `key` contains `--` (the environment-key separator), the part
    /// before it becomes the project's base via `environment.base` — the
    /// same convention `attachers` reads to scope groups to one base
    /// project's family. A key with no `--` is a base project itself.
    fn project_declaring(key: &str, groups: &[&str]) -> Project {
        let environment = key.split_once("--").map(|(base, env)| EnvironmentMeta {
            env: env.to_string(),
            base: base.to_string(),
            slug: None,
            ttl_secs: None,
            on_create: None,
        });
        Project {
            config: ProjectConfig {
                name: key.into(),
                repo: "https://github.com/x/y.git".into(),
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
                environment,
                secret_groups: groups.iter().map(|s| (*s).to_string()).collect(),
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
        }
    }

    #[tokio::test]
    async fn push_replaces_wholesale_by_default() {
        let mut store = MockSecretStore::new();
        store
            .expect_save()
            .withf(|r, b, expected| {
                *r == GroupRef::named("myapp", "preview")
                    && b.vars.len() == 1
                    && b.vars.contains_key("NEW")
                    && *expected == Some(3)
            })
            .times(1)
            .returning(|_, _, _| Ok(4));

        let revision = PushSecretGroup::new(Arc::new(store))
            .execute("myapp", "preview", objects(&[("NEW", "1")]), Some(3), false)
            .await
            .unwrap();
        assert_eq!(revision, 4);
    }

    #[tokio::test]
    async fn push_with_merge_upserts_onto_the_stored_objects() {
        let mut store = MockSecretStore::new();
        store.expect_load().returning(|_| {
            Ok(SecretGroup {
                objects: objects(&[("OLD", "keep"), ("NEW", "stale")]),
                revision: 2,
            })
        });
        store
            .expect_save()
            .withf(|_, b, _| {
                b.vars["OLD"] == "keep" && b.vars["NEW"] == "fresh" && b.vars.len() == 2
            })
            .times(1)
            .returning(|_, _, _| Ok(3));

        PushSecretGroup::new(Arc::new(store))
            .execute(
                "myapp",
                "preview",
                objects(&[("NEW", "fresh")]),
                Some(2),
                true,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_rejects_an_empty_group() {
        let mut store = MockSecretStore::new();
        store.expect_save().times(0);
        let err = PushSecretGroup::new(Arc::new(store))
            .execute("myapp", "preview", SecretsBundle::default(), Some(0), false)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
    }

    #[tokio::test]
    async fn list_joins_the_registry_to_report_who_attaches_each_group() {
        let mut store = MockSecretStore::new();
        store.expect_list().withf(|b| b == "myapp").returning(|_| {
            Ok(vec![
                GroupSummary {
                    name: "common".into(),
                    revision: 2,
                    keys: 3,
                    files: 0,
                    bytes: 0,
                    updated_at: 100,
                },
                GroupSummary {
                    name: "orphan".into(),
                    revision: 1,
                    keys: 1,
                    files: 0,
                    bytes: 0,
                    updated_at: 90,
                },
            ])
        });
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["common"])]));

        let listed = ListSecretGroups::new(Arc::new(store), Arc::new(projects))
            .execute("myapp")
            .await
            .unwrap();
        assert_eq!(listed[0].summary.name, "common");
        assert_eq!(listed[0].attached_by, vec!["myapp--test".to_string()]);
        assert!(
            listed[1].attached_by.is_empty(),
            "a group nobody declares reports no attachments, not an error"
        );
    }

    #[tokio::test]
    async fn remove_refuses_while_a_project_declares_the_group() {
        let mut store = MockSecretStore::new();
        store.expect_remove().times(0);
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["preview"])]));

        let err = RemoveSecretGroup::new(Arc::new(store), Arc::new(projects))
            .execute("myapp", "preview", false)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)), "got: {err}");
        assert!(err.to_string().contains("myapp--test"), "got: {err}");
    }

    #[tokio::test]
    async fn remove_with_force_deletes_an_attached_group() {
        let mut store = MockSecretStore::new();
        store
            .expect_remove()
            .withf(|r| *r == GroupRef::named("myapp", "preview"))
            .times(1)
            .returning(|_| Ok(()));
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project_declaring("myapp--test", &["preview"])]));

        RemoveSecretGroup::new(Arc::new(store), Arc::new(projects))
            .execute("myapp", "preview", true)
            .await
            .unwrap();
    }
}

use std::sync::Arc;

use async_trait::async_trait;
use pi_domain::contracts::ProjectRepository;
use pi_domain::entities::{
    EnvironmentMeta, ExposeMode, HealthcheckConfig, Project, ProjectConfig, StageTimeoutOverrides,
};
use pi_domain::error::DomainError;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::sqlite::{storage_err, Db};

const SELECT: &str = "SELECT name, repo, branch, compose_path, service, container_port, hostname, host_port, created_at, expose, commands, command_timeout_secs, env_name, env_base, env_slug, env_ttl_secs, env_on_create, env_on_create_done, last_success_at, last_commit_sha, secret_groups FROM projects";

pub struct SqliteProjectRepo {
    db: Db,
    port_min: u16,
    port_max: u16,
}

impl SqliteProjectRepo {
    pub fn new(db: Db, port_min: u16, port_max: u16) -> Arc<SqliteProjectRepo> {
        Arc::new(SqliteProjectRepo {
            db,
            port_min,
            port_max,
        })
    }
}

fn row_to_project(row: &rusqlite::Row<'_>) -> Result<Project, rusqlite::Error> {
    let environment = match row.get::<_, Option<String>>(12)? {
        Some(env) => Some(EnvironmentMeta {
            env,
            base: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
            slug: row.get(14)?,
            ttl_secs: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
            on_create: row.get(16)?,
        }),
        None => None,
    };
    Ok(Project {
        config: ProjectConfig {
            name: row.get(0)?,
            repo: row.get(1)?,
            branch: row.get(2)?,
            compose_path: row.get(3)?,
            service: row.get(4)?,
            container_port: row.get(5)?,
            hostname: row.get(6)?,
            expose: ExposeMode::parse(&row.get::<_, String>(9)?).unwrap_or_default(),
            healthcheck: HealthcheckConfig::default(), // per-deploy input, not stored
            timeouts: StageTimeoutOverrides::default(), // per-deploy input, not stored
            commands: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
            command_timeout_secs: row.get(11)?,
            environment,
            secret_groups: serde_json::from_str(&row.get::<_, String>(20)?).unwrap_or_default(),
        },
        host_port: row.get(7)?,
        created_at: row.get(8)?,
        on_create_done: row.get::<_, i64>(17)? != 0,
        last_success_at: row.get(18)?,
        last_commit_sha: row.get(19)?,
    })
}

fn allocate_port(conn: &Connection, min: u16, max: u16) -> Result<u16, DomainError> {
    let mut stmt = conn
        .prepare("SELECT host_port FROM projects ORDER BY host_port")
        .map_err(storage_err)?;
    let used: Vec<u16> = stmt
        .query_map([], |r| r.get(0))
        .map_err(storage_err)?
        .collect::<Result<_, _>>()
        .map_err(storage_err)?;

    let mut candidate = min;
    for port in used {
        if port == candidate {
            candidate = candidate.saturating_add(1);
        } else if port > candidate {
            break;
        }
    }
    if candidate > max {
        return Err(DomainError::Invalid(format!(
            "no free host ports in {min}-{max}"
        )));
    }
    Ok(candidate)
}

#[async_trait]
impl ProjectRepository for SqliteProjectRepo {
    async fn upsert(&self, config: &ProjectConfig) -> Result<Project, DomainError> {
        let config = config.clone();
        let (min, max) = (self.port_min, self.port_max);
        self.db
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_err)?;
                let exists: Option<u16> = tx
                    .query_row(
                        "SELECT host_port FROM projects WHERE name = ?1",
                        params![&config.name],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_err)?;
                let (env_name, env_base, env_slug, env_ttl_secs, env_on_create): (
                    Option<&str>,
                    Option<&str>,
                    Option<&str>,
                    Option<i64>,
                    Option<&str>,
                ) = match &config.environment {
                    Some(meta) => (
                        Some(meta.env.as_str()),
                        Some(meta.base.as_str()),
                        meta.slug.as_deref(),
                        meta.ttl_secs.map(|v| v as i64),
                        meta.on_create.as_deref(),
                    ),
                    None => (None, None, None, None, None),
                };
                let secret_groups = serde_json::to_string(&config.secret_groups)
                    .unwrap_or_else(|_| "[]".into());
                if exists.is_some() {
                    tx.execute(
                        "UPDATE projects SET repo=?2, branch=?3, compose_path=?4, service=?5, container_port=?6, hostname=?7, expose=?8, commands=?9, command_timeout_secs=?10, env_name=?11, env_base=?12, env_slug=?13, env_ttl_secs=?14, env_on_create=?15, secret_groups=?16 WHERE name=?1",
                        params![
                            &config.name,
                            &config.repo,
                            &config.branch,
                            &config.compose_path,
                            &config.service,
                            config.container_port,
                            &config.hostname,
                            config.expose.as_str(),
                            serde_json::to_string(&config.commands)
                                .unwrap_or_else(|_| "{}".into()),
                            config.command_timeout_secs,
                            env_name,
                            env_base,
                            env_slug,
                            env_ttl_secs,
                            env_on_create,
                            secret_groups,
                        ],
                    )
                    .map_err(storage_err)?;
                } else {
                    let port = allocate_port(&tx, min, max)?;
                    tx.execute(
                        "INSERT INTO projects (name, repo, branch, compose_path, service, container_port, hostname, host_port, created_at, expose, commands, command_timeout_secs, env_name, env_base, env_slug, env_ttl_secs, env_on_create, secret_groups)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch(), ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                        params![
                            &config.name,
                            &config.repo,
                            &config.branch,
                            &config.compose_path,
                            &config.service,
                            config.container_port,
                            &config.hostname,
                            port,
                            config.expose.as_str(),
                            serde_json::to_string(&config.commands)
                                .unwrap_or_else(|_| "{}".into()),
                            config.command_timeout_secs,
                            env_name,
                            env_base,
                            env_slug,
                            env_ttl_secs,
                            env_on_create,
                            secret_groups,
                        ],
                    )
                    .map_err(storage_err)?;
                }
                let project = tx
                    .query_row(
                        &format!("{SELECT} WHERE name = ?1"),
                        params![&config.name],
                        row_to_project,
                    )
                    .map_err(storage_err)?;
                tx.commit().map_err(storage_err)?;
                Ok(project)
            })
            .await
    }

    async fn get(&self, name: &str) -> Result<Option<Project>, DomainError> {
        let name = name.to_string();
        self.db
            .call(move |conn| {
                conn.query_row(
                    &format!("{SELECT} WHERE name = ?1"),
                    params![name],
                    row_to_project,
                )
                .optional()
                .map_err(storage_err)
            })
            .await
    }

    async fn list(&self) -> Result<Vec<Project>, DomainError> {
        self.db
            .call(|conn| {
                let mut stmt = conn
                    .prepare(&format!("{SELECT} ORDER BY name"))
                    .map_err(storage_err)?;
                let projects = stmt
                    .query_map([], row_to_project)
                    .map_err(storage_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(storage_err)?;
                Ok(projects)
            })
            .await
    }

    async fn remove(&self, name: &str) -> Result<(), DomainError> {
        let name = name.to_string();
        self.db
            .call(move |conn| {
                conn.execute("DELETE FROM projects WHERE name = ?1", params![name])
                    .map(|_| ())
                    .map_err(storage_err)
            })
            .await
    }

    async fn list_environments<'a>(
        &self,
        base: Option<&'a str>,
    ) -> Result<Vec<Project>, DomainError> {
        let base = base.map(|b| b.to_string());
        self.db
            .call(move |conn| {
                let projects = match &base {
                    Some(base) => {
                        let mut stmt = conn
                            .prepare(&format!(
                                "{SELECT} WHERE env_name IS NOT NULL AND env_base = ?1 ORDER BY name"
                            ))
                            .map_err(storage_err)?;
                        let rows = stmt
                            .query_map(params![base], row_to_project)
                            .map_err(storage_err)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(storage_err)?;
                        rows
                    }
                    None => {
                        let mut stmt = conn
                            .prepare(&format!("{SELECT} WHERE env_name IS NOT NULL ORDER BY name"))
                            .map_err(storage_err)?;
                        let rows = stmt
                            .query_map([], row_to_project)
                            .map_err(storage_err)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(storage_err)?;
                        rows
                    }
                };
                Ok(projects)
            })
            .await
    }

    async fn mark_deploy_success<'a>(
        &self,
        name: &str,
        at: i64,
        commit_sha: Option<&'a str>,
    ) -> Result<(), DomainError> {
        let name = name.to_string();
        let commit_sha = commit_sha.map(str::to_string);
        self.db
            .call(move |conn| {
                conn.execute(
                    "UPDATE projects SET last_success_at = ?2, last_commit_sha = COALESCE(?3, last_commit_sha) WHERE name = ?1",
                    params![name, at, commit_sha],
                )
                .map(|_| ())
                .map_err(storage_err)
            })
            .await
    }

    async fn set_on_create_done(&self, name: &str, done: bool) -> Result<(), DomainError> {
        let name = name.to_string();
        self.db
            .call(move |conn| {
                conn.execute(
                    "UPDATE projects SET env_on_create_done = ?2 WHERE name = ?1",
                    params![name, done],
                )
                .map(|_| ())
                .map_err(storage_err)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Db;
    use pi_domain::contracts::ProjectRepository;
    use pi_domain::entities::{
        ExposeMode, HealthcheckConfig, ProjectConfig, StageTimeoutOverrides,
    };
    use pi_domain::error::DomainError;
    use std::sync::Arc;

    fn cfg(name: &str) -> ProjectConfig {
        ProjectConfig {
            name: name.into(),
            repo: format!("git@github.com:x/{name}.git"),
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
        }
    }

    fn repo(dir: &tempfile::TempDir, min: u16, max: u16) -> Arc<SqliteProjectRepo> {
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        SqliteProjectRepo::new(db, min, max)
    }

    #[tokio::test]
    async fn allocates_sequential_ports_from_range() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        assert_eq!(repo.upsert(&cfg("a")).await.unwrap().host_port, 8000);
        assert_eq!(repo.upsert(&cfg("b")).await.unwrap().host_port, 8001);
    }

    #[tokio::test]
    async fn upsert_existing_keeps_port_and_updates_config() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("a")).await.unwrap();

        let mut updated = cfg("a");
        updated.branch = "develop".into();
        let project = repo.upsert(&updated).await.unwrap();

        assert_eq!(
            project.host_port, 8000,
            "host port is stable across re-deploys"
        );
        assert_eq!(project.config.branch, "develop");
        assert_eq!(repo.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_returns_upserted_project_and_none_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("a")).await.unwrap();
        let found = repo.get("a").await.unwrap().unwrap();
        assert_eq!(found.config.name, "a");
        assert_eq!(found.host_port, 8000);
        assert!(repo.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn roundtrips_expose_mode() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let mut c = cfg("a");
        c.expose = pi_domain::entities::ExposeMode::Lan;
        repo.upsert(&c).await.unwrap();
        let got = repo.get("a").await.unwrap().unwrap();
        assert_eq!(got.config.expose, pi_domain::entities::ExposeMode::Lan);
    }

    #[tokio::test]
    async fn port_range_exhaustion_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8000);
        repo.upsert(&cfg("a")).await.unwrap();
        let err = repo.upsert(&cfg("b")).await.unwrap_err();
        assert!(matches!(err, DomainError::Invalid(_)), "got: {err}");
    }

    #[tokio::test]
    async fn allocates_first_gap_in_port_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.call(|conn| {
            conn.execute(
                "INSERT INTO projects
                 (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('a', 'repo-a', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(storage_err)?;
            conn.execute(
                "INSERT INTO projects
                 (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('c', 'repo-c', 'main', 'docker-compose.yml', 'web', 3000, 8002, 1)",
                [],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .unwrap();

        let repo = SqliteProjectRepo::new(db, 8000, 8999);
        assert_eq!(repo.upsert(&cfg("b")).await.unwrap().host_port, 8001);
    }

    #[tokio::test]
    async fn persists_commands_and_command_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let mut config = cfg("a");
        config.commands.insert(
            "migrate".into(),
            pi_domain::entities::CommandSpec::new(vec!["npx".into(), "prisma".into()]),
        );
        config.command_timeout_secs = Some(1800);

        repo.upsert(&config).await.unwrap();
        let loaded = repo.get("a").await.unwrap().unwrap();
        assert_eq!(loaded.config.commands, config.commands);
        assert_eq!(loaded.config.command_timeout_secs, Some(1800));

        // update path: replacing commands persists the new set
        config.commands.clear();
        config.commands.insert(
            "seed".into(),
            pi_domain::entities::CommandSpec::new(vec!["node".into(), "seed.js".into()]),
        );
        config.command_timeout_secs = None;
        repo.upsert(&config).await.unwrap();
        let loaded = repo.get("a").await.unwrap().unwrap();
        assert_eq!(loaded.config.commands.len(), 1);
        assert!(loaded.config.commands.contains_key("seed"));
        assert_eq!(loaded.config.command_timeout_secs, None);
    }

    #[tokio::test]
    async fn loads_legacy_array_shaped_commands_row() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let mut config = cfg("legacy");
        config.commands.insert(
            "migrate".into(),
            pi_domain::entities::CommandSpec::new(vec!["npx".into()]),
        );
        repo.upsert(&config).await.unwrap();
        let loaded = repo.get("legacy").await.unwrap().unwrap();
        assert_eq!(
            loaded.config.commands.get("migrate").unwrap().service,
            None,
            "service-less command decodes with no service"
        );
    }

    #[tokio::test]
    async fn list_is_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("zeta")).await.unwrap();
        repo.upsert(&cfg("alpha")).await.unwrap();
        let names: Vec<String> = repo
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.config.name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    fn env_cfg(name: &str, base: &str, env: &str) -> ProjectConfig {
        let mut config = cfg(name);
        config.environment = Some(pi_domain::entities::EnvironmentMeta {
            env: env.into(),
            base: base.into(),
            slug: None,
            ttl_secs: Some(3600),
            on_create: Some("seed".into()),
        });
        config
    }

    #[tokio::test]
    async fn upsert_persists_environment_meta_and_flags_survive_update() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let p = repo
            .upsert(&env_cfg("myapp--test", "myapp", "test"))
            .await
            .unwrap();
        let meta = p.config.environment.as_ref().unwrap();
        assert_eq!((meta.env.as_str(), meta.base.as_str()), ("test", "myapp"));
        assert!(!p.on_create_done);
        assert_eq!(p.last_success_at, None);

        repo.set_on_create_done("myapp--test", true).await.unwrap();
        repo.mark_deploy_success("myapp--test", 12345, None)
            .await
            .unwrap();
        // re-upsert (new deploy) must NOT reset the runtime flags
        let p = repo
            .upsert(&env_cfg("myapp--test", "myapp", "test"))
            .await
            .unwrap();
        assert!(p.on_create_done);
        assert_eq!(p.last_success_at, Some(12345));
    }

    #[tokio::test]
    async fn mark_deploy_success_stores_the_sha_and_never_erases_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("a")).await.unwrap();

        repo.mark_deploy_success("a", 100, Some("sha-one"))
            .await
            .unwrap();
        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_success_at, Some(100));
        assert_eq!(p.last_commit_sha.as_deref(), Some("sha-one"));

        repo.mark_deploy_success("a", 200, None).await.unwrap();
        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_success_at, Some(200));
        assert_eq!(
            p.last_commit_sha.as_deref(),
            Some("sha-one"),
            "a call with no sha must not erase the stored one"
        );
    }

    #[tokio::test]
    async fn list_environments_filters_by_base_and_excludes_base_projects() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("myapp")).await.unwrap();
        repo.upsert(&env_cfg("myapp--test", "myapp", "test"))
            .await
            .unwrap();
        repo.upsert(&env_cfg("other--test", "other", "test"))
            .await
            .unwrap();
        let all = repo.list_environments(None).await.unwrap();
        assert_eq!(all.len(), 2);
        let mine = repo.list_environments(Some("myapp")).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].config.name, "myapp--test");
    }

    #[tokio::test]
    async fn secret_groups_round_trip_and_default_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let mut config = cfg("a");
        config.secret_groups = vec!["common".into(), "preview".into()];
        repo.upsert(&config).await.unwrap();
        let loaded = repo.get("a").await.unwrap().unwrap();
        assert_eq!(
            loaded.config.secret_groups,
            vec!["common".to_string(), "preview".to_string()]
        );

        // Replacing the list wholesale is how an overlay detaches groups.
        config.secret_groups.clear();
        repo.upsert(&config).await.unwrap();
        assert!(repo
            .get("a")
            .await
            .unwrap()
            .unwrap()
            .config
            .secret_groups
            .is_empty());
    }

    /// `last_commit_sha` and `secret_groups` are the last two columns of
    /// `SELECT`, added by two different features at adjacent indices. Reading
    /// either from the other's index still type-checks (both decode from a
    /// column), so only distinct values on the same row can catch a swap.
    #[tokio::test]
    async fn the_two_last_columns_do_not_cross_over() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        let mut config = cfg("a");
        config.secret_groups = vec!["common".into()];
        repo.upsert(&config).await.unwrap();
        repo.mark_deploy_success("a", 7, Some("deadbeef"))
            .await
            .unwrap();

        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_commit_sha.as_deref(), Some("deadbeef"));
        assert_eq!(p.config.secret_groups, vec!["common".to_string()]);

        // A re-upsert rewrites the groups without touching the sha: the two
        // columns must still be independent after the UPDATE path, not only
        // after the INSERT one.
        config.secret_groups = vec!["preview".into()];
        repo.upsert(&config).await.unwrap();
        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_commit_sha.as_deref(), Some("deadbeef"));
        assert_eq!(p.config.secret_groups, vec!["preview".to_string()]);
        assert_eq!(p.last_success_at, Some(7));
    }

    #[tokio::test]
    async fn a_row_written_before_the_column_existed_reads_as_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.call(|c| {
            c.execute(
                "INSERT INTO projects (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('legacy', 'r', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .unwrap();

        let repo = SqliteProjectRepo::new(db, 8000, 8999);
        let loaded = repo.get("legacy").await.unwrap().unwrap();
        assert!(loaded.config.secret_groups.is_empty());
    }
}

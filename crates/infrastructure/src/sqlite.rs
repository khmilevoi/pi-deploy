use std::path::Path;
use std::sync::{Arc, Mutex};

use pi_domain::error::DomainError;
use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(
            r#"
        CREATE TABLE projects (
            name           TEXT PRIMARY KEY,
            repo           TEXT NOT NULL,
            branch         TEXT NOT NULL,
            compose_path   TEXT NOT NULL,
            service        TEXT NOT NULL,
            container_port INTEGER NOT NULL,
            hostname       TEXT,
            host_port      INTEGER NOT NULL UNIQUE,
            created_at     INTEGER NOT NULL
        );
        CREATE TABLE deployments (
            id          TEXT PRIMARY KEY,
            project     TEXT NOT NULL,
            git_ref     TEXT NOT NULL,
            commit_sha  TEXT,
            status      TEXT NOT NULL,
            started_at  INTEGER NOT NULL,
            finished_at INTEGER,
            log_tail    TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX idx_deployments_project ON deployments(project, started_at DESC);
        "#,
        ),
        M::up("ALTER TABLE projects ADD COLUMN expose TEXT NOT NULL DEFAULT 'private';"),
        M::up(
            r#"
        ALTER TABLE projects ADD COLUMN commands TEXT NOT NULL DEFAULT '{}';
        ALTER TABLE projects ADD COLUMN command_timeout_secs INTEGER;
        "#,
        ),
        M::up(
            r#"
        CREATE TABLE applied_migrations (
            id         TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );
        "#,
        ),
        M::up(
            r#"
        ALTER TABLE projects ADD COLUMN env_name TEXT;
        ALTER TABLE projects ADD COLUMN env_base TEXT;
        ALTER TABLE projects ADD COLUMN env_slug TEXT;
        ALTER TABLE projects ADD COLUMN env_ttl_secs INTEGER;
        ALTER TABLE projects ADD COLUMN env_on_create TEXT;
        ALTER TABLE projects ADD COLUMN env_on_create_done INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE projects ADD COLUMN last_success_at INTEGER;
        "#,
        ),
        M::up("ALTER TABLE projects ADD COLUMN secret_groups TEXT NOT NULL DEFAULT '[]';"),
    ])
}

pub(crate) fn storage_err(e: rusqlite::Error) -> DomainError {
    DomainError::Storage(e.to_string())
}

/// Wrapper around rusqlite: WAL and migrations at open, all calls through spawn_blocking.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db, DomainError> {
        let mut conn = Connection::open(path).map_err(storage_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(storage_err)?;
        // Future-facing: v0.1 records deployment start before project upsert, so
        // deployments.project is intentionally not FK-constrained yet.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_err)?;
        migrations()
            .to_latest(&mut conn)
            .map_err(|e| DomainError::Storage(format!("migrations: {e}")))?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn call<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, DomainError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn
                .lock()
                .map_err(|_| DomainError::Storage("db mutex poisoned".into()))?;
            f(&mut conn)
        })
        .await
        .map_err(|e| DomainError::Storage(format!("join error: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_db_in_wal_mode_with_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        let mode: String = db
            .call(|c| {
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(storage_err)
            })
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let n: i64 = db
            .call(|c| {
                c.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
                    .map_err(storage_err)
            })
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap();
        let n: i64 = db
            .call(|c| {
                c.query_row("SELECT count(*) FROM deployments", [], |r| r.get(0))
                    .map_err(storage_err)
            })
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn projects_schema_enforces_name_primary_key_and_unique_host_port() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();

        db.call(|c| {
            c.execute(
                "INSERT INTO projects
                 (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('a', 'repo-a', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .unwrap();

        let duplicate_name = db
            .call(|c| {
                c.execute(
                    "INSERT INTO projects
                     (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                     VALUES ('a', 'repo-b', 'main', 'docker-compose.yml', 'web', 3000, 8001, 1)",
                    [],
                )
                .map_err(storage_err)?;
                Ok(())
            })
            .await;
        assert!(matches!(duplicate_name, Err(DomainError::Storage(_))));

        let duplicate_port = db
            .call(|c| {
                c.execute(
                    "INSERT INTO projects
                     (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                     VALUES ('b', 'repo-b', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                    [],
                )
                .map_err(storage_err)?;
                Ok(())
            })
            .await;
        assert!(matches!(duplicate_port, Err(DomainError::Storage(_))));
    }

    #[tokio::test]
    async fn migration_adds_expose_column_defaulting_private() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.call(|c| {
            c.execute(
                "INSERT INTO projects
                 (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('a', 'repo-a', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .unwrap();
        let expose: String = db
            .call(|c| {
                c.query_row("SELECT expose FROM projects WHERE name='a'", [], |r| {
                    r.get(0)
                })
                .map_err(storage_err)
            })
            .await
            .unwrap();
        assert_eq!(expose, "private");
    }

    #[tokio::test]
    async fn migration_adds_commands_columns_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        db.call(|c| {
            c.execute(
                "INSERT INTO projects
                 (name, repo, branch, compose_path, service, container_port, host_port, created_at)
                 VALUES ('a', 'repo-a', 'main', 'docker-compose.yml', 'web', 3000, 8000, 1)",
                [],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .unwrap();
        let (commands, timeout): (String, Option<i64>) = db
            .call(|c| {
                c.query_row(
                    "SELECT commands, command_timeout_secs FROM projects WHERE name = 'a'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(storage_err)
            })
            .await
            .unwrap();
        assert_eq!(commands, "{}");
        assert_eq!(timeout, None);
    }
}

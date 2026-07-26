//! The `RPI_*` runtime variables (spec 2026-07-26).
//!
//! These exist only inside containers and exec'd processes — never in a TOML
//! file. The whole set derives from registry state the agent already holds,
//! which is why nothing about them travels on the wire: computing them in one
//! place makes it impossible for the CLI and the agent to disagree.

use std::collections::BTreeMap;

use crate::entities::Project;

/// The variables a deploy of `project` exports.
///
/// `commit_sha` is the sha of the deploy currently in flight; outside a
/// deploy pass `None` and the project's last successful sha is used. A value
/// that does not exist is omitted from the map rather than set to an empty
/// string, so `${RPI_ENV:-prod}` works inside a container.
///
/// "Does not exist" covers an empty string as well as `None`: `--vars B=` is
/// legal and `branch = "${B}"` then reaches the registry with an empty
/// branch, and `RPI_BRANCH_NAME=""` would silently defeat every
/// `${RPI_BRANCH_NAME:-fallback}` in the stack.
pub fn rpi_vars(project: &Project, commit_sha: Option<&str>) -> BTreeMap<String, String> {
    let config = &project.config;
    let mut vars = BTreeMap::new();
    let mut put = |key: &str, value: String| {
        vars.insert(key.to_string(), value);
    };
    let present = |s: &str| (!s.is_empty()).then(|| s.to_string());

    put("RPI_PROJECT", config.name.clone());
    match &config.environment {
        Some(env) => {
            put("RPI_PROJECT_BASE", env.base.clone());
            put("RPI_ENV", env.env.clone());
            if let Some(slug) = env.slug.as_deref().and_then(present) {
                put("RPI_ENV_SLUG", slug);
            }
        }
        None => put("RPI_PROJECT_BASE", config.name.clone()),
    }
    if let Some(branch) = present(&config.branch) {
        put("RPI_BRANCH_NAME", branch);
    }
    if let Some(hostname) = config.hostname.as_deref().and_then(present) {
        put("RPI_HOSTNAME", hostname);
    }
    put("RPI_HOST_PORT", project.host_port.to_string());
    let sha = commit_sha
        .and_then(present)
        .or_else(|| project.last_commit_sha.as_deref().and_then(present));
    if let Some(sha) = sha {
        put("RPI_COMMIT_SHA", sha);
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{
        EnvironmentMeta, ExposeMode, HealthcheckConfig, ProjectConfig, StageTimeoutOverrides,
    };

    fn project(name: &str) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: "git@github.com:acme/myapp.git".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 3000,
                hostname: Some("app.example.com".into()),
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
            last_commit_sha: None,
        }
    }

    #[test]
    fn plain_project_exports_the_base_set() {
        let vars = rpi_vars(&project("myapp"), Some("abc123"));
        assert_eq!(vars["RPI_PROJECT"], "myapp");
        assert_eq!(vars["RPI_PROJECT_BASE"], "myapp");
        assert_eq!(vars["RPI_BRANCH_NAME"], "main");
        assert_eq!(vars["RPI_HOSTNAME"], "app.example.com");
        assert_eq!(vars["RPI_HOST_PORT"], "8000");
        assert_eq!(vars["RPI_COMMIT_SHA"], "abc123");
        assert!(!vars.contains_key("RPI_ENV"), "no environment: {vars:?}");
        assert!(!vars.contains_key("RPI_ENV_SLUG"), "{vars:?}");
    }

    #[test]
    fn environment_project_adds_env_base_and_slug() {
        let mut p = project("myapp--branch--feature-login");
        p.config.branch = "feature/login".into();
        p.config.environment = Some(EnvironmentMeta {
            env: "branch".into(),
            base: "myapp".into(),
            slug: Some("feature-login".into()),
            ttl_secs: None,
            on_create: None,
        });
        let vars = rpi_vars(&p, None);
        assert_eq!(vars["RPI_PROJECT"], "myapp--branch--feature-login");
        assert_eq!(vars["RPI_PROJECT_BASE"], "myapp");
        assert_eq!(vars["RPI_ENV"], "branch");
        assert_eq!(vars["RPI_ENV_SLUG"], "feature-login");
        assert_eq!(vars["RPI_BRANCH_NAME"], "feature/login");
    }

    #[test]
    fn absent_values_are_omitted_rather_than_empty() {
        let mut p = project("myapp");
        p.config.hostname = None;
        let vars = rpi_vars(&p, None);
        for absent in ["RPI_HOSTNAME", "RPI_COMMIT_SHA", "RPI_ENV", "RPI_ENV_SLUG"] {
            assert!(
                !vars.contains_key(absent),
                "{absent} must be omitted: {vars:?}"
            );
        }
        assert!(vars.values().all(|v| !v.is_empty()), "{vars:?}");
    }

    #[test]
    fn an_empty_value_is_as_absent_as_none() {
        // `--vars B=` is legal, so `branch = "${B}"` reaches the registry
        // with an empty branch; exporting RPI_BRANCH_NAME="" would defeat
        // every ${RPI_BRANCH_NAME:-fallback} inside a container.
        let mut p = project("myapp");
        p.config.branch = String::new();
        p.config.hostname = Some(String::new());
        p.last_commit_sha = Some(String::new());
        p.config.environment = Some(EnvironmentMeta {
            env: "branch".into(),
            base: "myapp".into(),
            slug: Some(String::new()),
            ttl_secs: None,
            on_create: None,
        });
        let vars = rpi_vars(&p, Some(""));
        for absent in [
            "RPI_BRANCH_NAME",
            "RPI_HOSTNAME",
            "RPI_COMMIT_SHA",
            "RPI_ENV_SLUG",
        ] {
            assert!(
                !vars.contains_key(absent),
                "{absent} must be omitted, not empty: {vars:?}"
            );
        }
        assert!(vars.values().all(|v| !v.is_empty()), "{vars:?}");

        // An in-flight empty sha must not shadow a usable stored one either.
        p.last_commit_sha = Some("stored".into());
        assert_eq!(rpi_vars(&p, Some(""))["RPI_COMMIT_SHA"], "stored");
    }

    #[test]
    fn the_registry_sha_is_the_fallback_when_no_deploy_is_in_flight() {
        let mut p = project("myapp");
        p.last_commit_sha = Some("stored".into());
        assert_eq!(rpi_vars(&p, None)["RPI_COMMIT_SHA"], "stored");
        assert_eq!(
            rpi_vars(&p, Some("fresh"))["RPI_COMMIT_SHA"],
            "fresh",
            "the in-flight deploy wins over the stored value"
        );
    }

    #[test]
    fn every_exported_name_uses_the_reserved_prefix() {
        let vars = rpi_vars(&project("myapp"), Some("abc"));
        assert!(vars.keys().all(|k| k.starts_with("RPI_")), "{vars:?}");
    }
}

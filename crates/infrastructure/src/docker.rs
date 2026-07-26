use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pi_domain::contracts::{ContainerRuntime, LogSink};
use pi_domain::entities::{ComposeStack, LifecycleAction, ServiceState, ServiceStats};
use pi_domain::error::DomainError;
use tokio::process::Command;

use crate::process::{run_capture, run_streamed, run_streamed_code};

/// Age filter for `docker builder prune` (§8.1): recent cache survives so
/// rebuilds stay fast; only the disk threshold is configurable (§22).
pub(crate) const BUILDER_PRUNE_MAX_AGE: &str = "24h";

pub(crate) fn prune_images_args() -> Vec<String> {
    vec!["image".to_string(), "prune".to_string(), "-f".to_string()]
}

pub(crate) fn prune_builder_args() -> Vec<String> {
    vec![
        "builder".to_string(),
        "prune".to_string(),
        "-f".to_string(),
        "--filter".to_string(),
        format!("until={BUILDER_PRUNE_MAX_AGE}"),
    ]
}

pub(crate) fn logs_args(project_name: &str, tail: usize, follow: bool) -> Vec<String> {
    let mut args = vec![
        "compose".to_string(),
        "-p".to_string(),
        project_name.to_string(),
        "logs".to_string(),
        "--tail".to_string(),
        tail.to_string(),
    ];
    if follow {
        args.push("-f".to_string());
    }
    args
}

pub(crate) fn exec_tail<'a>(
    service: &'a str,
    argv: &'a [String],
    env: &'a std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut tail = vec!["exec".to_string(), "-T".to_string()];
    // A container started by an earlier deploy carries that deploy's
    // environment; `-e` makes the exec'd process see current values.
    for (key, value) in env {
        tail.push("-e".to_string());
        tail.push(format!("{key}={value}"));
    }
    tail.push(service.to_string());
    tail.extend(argv.iter().cloned());
    tail
}

pub(crate) fn file_chain(stack: &ComposeStack) -> Vec<PathBuf> {
    let mut files = vec![stack.compose_file.clone()];
    let repo_override = stack.workdir.join("docker-compose.override.yml");
    if repo_override.exists() && repo_override != stack.compose_file {
        files.push(repo_override);
    }
    if stack.override_file.exists() {
        files.push(stack.override_file.clone());
    }
    files
}

/// Files consulted when *listing* services, as opposed to acting on them.
/// The generated override is excluded on purpose: it is the file about to be
/// rewritten, and a service it still carries from a previous deploy would be
/// re-created as a phantom with an `environment:` block and no image.
pub(crate) fn discovery_chain(stack: &ComposeStack) -> Vec<PathBuf> {
    let mut files = vec![stack.compose_file.clone()];
    let repo_override = stack.workdir.join("docker-compose.override.yml");
    if repo_override.exists() && repo_override != stack.compose_file {
        files.push(repo_override);
    }
    files
}

pub(crate) fn compose_args(project_name: &str, files: &[PathBuf], tail: &[&str]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["compose".into(), "-p".into(), project_name.into()];
    for f in files {
        args.push("-f".into());
        args.push(f.clone().into());
    }
    args.extend(tail.iter().map(|s| OsString::from(*s)));
    args
}

pub(crate) fn parse_ps_json(output: &str) -> Vec<ServiceState> {
    json_lines(output)
        .iter()
        .filter_map(service_state)
        .collect()
}

/// One JSON document per line (modern compose/docker) or a legacy array.
pub(crate) fn json_lines(output: &str) -> Vec<serde_json::Value> {
    let trimmed = output.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<serde_json::Value>>(trimmed).unwrap_or_default();
    }
    output
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn service_state(v: &serde_json::Value) -> Option<ServiceState> {
    let health = v
        .get("Health")
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    Some(ServiceState {
        service: v.get("Service")?.as_str()?.to_string(),
        state: v.get("State")?.as_str()?.to_string(),
        health,
    })
}

pub(crate) fn container_names(ps_output: &str) -> Vec<String> {
    json_lines(ps_output)
        .iter()
        .filter_map(|v| v.get("Name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

pub(crate) fn parse_percent(s: &str) -> Option<f64> {
    s.strip_suffix('%')?.parse().ok()
}

pub(crate) fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().ok()?;
    let factor = match unit.trim() {
        "B" | "" => 1.0,
        "kB" | "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * factor) as u64)
}

pub(crate) fn parse_mem_usage(s: &str) -> Option<(u64, u64)> {
    let (used, limit) = s.split_once('/')?;
    Some((parse_size(used.trim())?, parse_size(limit.trim())?))
}

pub(crate) fn parse_stats_json(ps_output: &str, stats_output: &str) -> Vec<ServiceStats> {
    // Name -> (service, state, health) from the ps JSON.
    let services: HashMap<String, (String, String, Option<String>)> = json_lines(ps_output)
        .iter()
        .filter_map(|v| {
            let state = v
                .get("State")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let health = v
                .get("Health")
                .and_then(|h| h.as_str())
                .filter(|h| !h.is_empty())
                .map(str::to_string);
            Some((
                v.get("Name")?.as_str()?.to_string(),
                (v.get("Service")?.as_str()?.to_string(), state, health),
            ))
        })
        .collect();

    let mut out = Vec::new();
    for v in json_lines(stats_output) {
        let Some(name) = v.get("Name").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some((service, state, health)) = services.get(name) else {
            continue;
        };
        let Some(cpu_percent) = v
            .get("CPUPerc")
            .and_then(|p| p.as_str())
            .and_then(parse_percent)
        else {
            continue;
        };
        let Some((mem_used_bytes, mem_limit_bytes)) = v
            .get("MemUsage")
            .and_then(|m| m.as_str())
            .and_then(parse_mem_usage)
        else {
            continue;
        };
        out.push(ServiceStats {
            service: service.clone(),
            cpu_percent,
            mem_used_bytes,
            mem_limit_bytes,
            state: state.clone(),
            health: health.clone(),
        });
    }
    out
}

pub struct DockerComposeRuntime;

impl DockerComposeRuntime {
    pub fn new() -> Arc<DockerComposeRuntime> {
        Arc::new(DockerComposeRuntime)
    }

    fn compose(&self, stack: &ComposeStack, tail: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(compose_args(&stack.project_name, &file_chain(stack), tail));
        cmd.current_dir(&stack.workdir);
        // BuildKit's fancy multi-line, cursor-redrawing progress UI corrupts
        // captured output even when stdout is a pipe, not a TTY — plain mode
        // emits ordinary newline-terminated lines instead. Applies uniformly
        // to every subcommand; a no-op for ones that don't build anything.
        cmd.env("BUILDKIT_PROGRESS", "plain");
        // Compose gives the shell environment precedence over `.env`, so this
        // makes `${RPI_*}` interpolate inside the project's compose file
        // without touching the secrets bundle that owns `.env`.
        cmd.envs(&stack.env);
        cmd
    }
}

#[async_trait]
impl ContainerRuntime for DockerComposeRuntime {
    async fn build(&self, stack: &ComposeStack, log: Arc<dyn LogSink>) -> Result<(), DomainError> {
        log.line("docker compose build ...");
        run_streamed(self.compose(stack, &["build"]), log)
            .await
            .map_err(DomainError::Runtime)
    }

    async fn up(&self, stack: &ComposeStack, log: Arc<dyn LogSink>) -> Result<(), DomainError> {
        log.line("docker compose up -d ...");
        run_streamed(self.compose(stack, &["up", "-d", "--remove-orphans"]), log)
            .await
            .map_err(DomainError::Runtime)
    }

    async fn ps(&self, project_name: &str) -> Result<Vec<ServiceState>, DomainError> {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-p", project_name, "ps", "--format", "json"]);
        let out = run_capture(cmd).await.map_err(DomainError::Runtime)?;
        Ok(parse_ps_json(&out))
    }

    async fn services(&self, stack: &ComposeStack) -> Result<Vec<String>, DomainError> {
        let mut cmd = Command::new("docker");
        cmd.args(compose_args(
            &stack.project_name,
            &discovery_chain(stack),
            &["config", "--services"],
        ));
        cmd.current_dir(&stack.workdir);
        cmd.envs(&stack.env);
        let out = run_capture(cmd).await.map_err(DomainError::Runtime)?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn prune_images(&self, log: Arc<dyn LogSink>) -> Result<(), DomainError> {
        log.line("docker image prune -f ...");
        let mut cmd = Command::new("docker");
        cmd.args(prune_images_args());
        run_streamed(cmd, log).await.map_err(DomainError::Runtime)
    }

    async fn prune_builder(&self, log: Arc<dyn LogSink>) -> Result<(), DomainError> {
        log.line(&format!(
            "docker builder prune -f --filter until={BUILDER_PRUNE_MAX_AGE} ..."
        ));
        let mut cmd = Command::new("docker");
        cmd.args(prune_builder_args());
        run_streamed(cmd, log).await.map_err(DomainError::Runtime)
    }

    async fn logs(
        &self,
        project_name: &str,
        tail: usize,
        follow: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        log.line(&format!("docker compose logs --tail {tail} ..."));
        let mut cmd = Command::new("docker");
        cmd.args(logs_args(project_name, tail, follow));
        run_streamed(cmd, log).await.map_err(DomainError::Runtime)
    }

    async fn stats(&self, project_name: &str) -> Result<Vec<ServiceStats>, DomainError> {
        let mut ps = Command::new("docker");
        ps.args(["compose", "-p", project_name, "ps", "--format", "json"]);
        let ps_output = run_capture(ps).await.map_err(DomainError::Runtime)?;
        let names = container_names(&ps_output);
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut stats = Command::new("docker");
        stats.args(["stats", "--no-stream", "--format", "json"]);
        stats.args(names);
        let stats_output = run_capture(stats).await.map_err(DomainError::Runtime)?;
        Ok(parse_stats_json(&ps_output, &stats_output))
    }

    async fn lifecycle(
        &self,
        stack: &ComposeStack,
        action: LifecycleAction,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        log.line(&format!("docker compose {} ...", action.as_str()));
        run_streamed(self.compose(stack, &[action.as_str()]), log)
            .await
            .map_err(DomainError::Runtime)
    }

    async fn down(
        &self,
        stack: &ComposeStack,
        remove_volumes: bool,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        log.line("docker compose down ...");
        let mut tail = vec!["down", "--remove-orphans"];
        if remove_volumes {
            tail.push("--volumes");
        }
        run_streamed(self.compose(stack, &tail), log)
            .await
            .map_err(DomainError::Runtime)
    }

    async fn exec(
        &self,
        stack: &ComposeStack,
        service: &str,
        argv: &[String],
        log: Arc<dyn LogSink>,
    ) -> Result<i32, DomainError> {
        log.line(&format!("docker compose exec -T {service} ..."));
        let tail = exec_tail(service, argv, &stack.env);
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        run_streamed_code(self.compose(stack, &tail), log)
            .await
            .map_err(DomainError::Runtime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn stack(workdir: &std::path::Path) -> ComposeStack {
        ComposeStack {
            project_name: "rateme".into(),
            workdir: workdir.to_path_buf(),
            compose_file: workdir.join("docker-compose.yml"),
            override_file: PathBuf::from("/var/lib/rpi/overrides/rateme.yml"),
            env: Default::default(),
        }
    }

    fn stack_with_env(workdir: &std::path::Path) -> ComposeStack {
        let mut s = stack(workdir);
        s.env = [
            ("RPI_PROJECT".to_string(), "rateme".to_string()),
            ("RPI_BRANCH_NAME".to_string(), "main".to_string()),
        ]
        .into_iter()
        .collect();
        s
    }

    #[test]
    fn file_chain_without_repo_override() {
        let dir = tempfile::tempdir().unwrap();
        let s = stack(dir.path());
        assert_eq!(file_chain(&s), vec![s.compose_file.clone()]);
    }

    #[test]
    fn file_chain_includes_repo_override_between_compose_and_pi_override() {
        let dir = tempfile::tempdir().unwrap();
        let repo_override = dir.path().join("docker-compose.override.yml");
        std::fs::write(&repo_override, "services: {}").unwrap();
        let pi_override = dir.path().join("pi-override.yml");
        std::fs::write(&pi_override, "services: {}").unwrap();
        let s = ComposeStack {
            project_name: "rateme".into(),
            workdir: dir.path().to_path_buf(),
            compose_file: dir.path().join("docker-compose.yml"),
            override_file: pi_override.clone(),
            env: Default::default(),
        };
        assert_eq!(
            file_chain(&s),
            vec![s.compose_file.clone(), repo_override, pi_override,]
        );
    }

    #[test]
    fn service_discovery_ignores_the_generated_override() {
        // The generated override is what we are about to write; including it
        // would let a service left over from a previous deploy reappear in
        // the list and be re-created as a phantom with no image.
        let dir = tempfile::tempdir().unwrap();
        let repo_override = dir.path().join("docker-compose.override.yml");
        std::fs::write(&repo_override, "services: {}").unwrap();
        let pi_override = dir.path().join("pi-override.yml");
        std::fs::write(&pi_override, "services: {}").unwrap();
        let s = ComposeStack {
            project_name: "rateme".into(),
            workdir: dir.path().to_path_buf(),
            compose_file: dir.path().join("docker-compose.yml"),
            override_file: pi_override,
            env: Default::default(),
        };
        assert_eq!(
            discovery_chain(&s),
            vec![s.compose_file.clone(), repo_override],
            "the pi override must not take part in discovery"
        );
        assert!(file_chain(&s).len() > discovery_chain(&s).len());
    }

    #[test]
    fn compose_args_shape() {
        let files = vec![PathBuf::from("a.yml"), PathBuf::from("b.yml")];
        let args = compose_args("rateme", &files, &["up", "-d"]);
        let expected: Vec<std::ffi::OsString> = [
            "compose", "-p", "rateme", "-f", "a.yml", "-f", "b.yml", "up", "-d",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn compose_sets_buildkit_progress_plain() {
        let dir = tempfile::tempdir().unwrap();
        let s = stack(dir.path());
        let runtime = DockerComposeRuntime;
        let cmd = runtime.compose(&s, &["build"]);
        let value = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("BUILDKIT_PROGRESS"))
            .and_then(|(_, v)| v);
        assert_eq!(value, Some(std::ffi::OsStr::new("plain")));
    }

    #[test]
    fn compose_exports_the_runtime_environment_to_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = DockerComposeRuntime.compose(&stack_with_env(dir.path()), &["up", "-d"]);
        let envs: HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        assert_eq!(envs["RPI_PROJECT"], "rateme");
        assert_eq!(envs["RPI_BRANCH_NAME"], "main");
        assert_eq!(
            envs["BUILDKIT_PROGRESS"], "plain",
            "the existing export must survive"
        );
    }

    #[test]
    fn prune_args_shapes() {
        assert_eq!(
            prune_images_args(),
            vec!["image".to_string(), "prune".to_string(), "-f".to_string()]
        );
        assert_eq!(
            prune_builder_args(),
            vec![
                "builder".to_string(),
                "prune".to_string(),
                "-f".to_string(),
                "--filter".to_string(),
                format!("until={BUILDER_PRUNE_MAX_AGE}"),
            ]
        );
    }

    #[test]
    fn exec_tail_shape() {
        let argv = strings(&["node", "scripts/create-invite.js", "--email", "x@y.com"]);
        assert_eq!(
            exec_tail("web", &argv, &Default::default()),
            vec![
                "exec",
                "-T",
                "web",
                "node",
                "scripts/create-invite.js",
                "--email",
                "x@y.com"
            ]
        );
    }

    #[test]
    fn exec_passes_the_runtime_environment_as_e_flags() {
        let env: std::collections::BTreeMap<String, String> = [
            ("RPI_BRANCH_NAME".to_string(), "main".to_string()),
            ("RPI_PROJECT".to_string(), "rateme".to_string()),
        ]
        .into_iter()
        .collect();
        let argv = strings(&["node", "seed.js"]);
        assert_eq!(
            exec_tail("web", &argv, &env),
            vec![
                "exec",
                "-T",
                "-e",
                "RPI_BRANCH_NAME=main",
                "-e",
                "RPI_PROJECT=rateme",
                "web",
                "node",
                "seed.js",
            ],
            "flags precede the service name, in BTreeMap order"
        );
        assert_eq!(
            exec_tail("web", &argv, &Default::default()),
            vec!["exec", "-T", "web", "node", "seed.js"],
            "an empty map must produce the original argv shape"
        );
    }

    #[test]
    fn logs_args_shapes() {
        assert_eq!(
            logs_args("rateme", 100, false),
            strings(&["compose", "-p", "rateme", "logs", "--tail", "100"])
        );
        assert_eq!(
            logs_args("rateme", 50, true),
            strings(&["compose", "-p", "rateme", "logs", "--tail", "50", "-f"])
        );
    }

    #[test]
    fn parse_docker_sizes_and_percents() {
        assert_eq!(parse_percent("0.50%"), Some(0.5));
        assert_eq!(parse_percent("nope"), None);
        assert_eq!(parse_size("512B"), Some(512));
        assert_eq!(parse_size("1.5KiB"), Some(1536));
        assert_eq!(parse_size("12.5MiB"), Some(13_107_200));
        assert_eq!(parse_size("1.9GiB"), Some(2_040_109_465));
        assert_eq!(parse_size("2MB"), Some(2_000_000));
        assert_eq!(parse_size("weird"), None);
        assert_eq!(
            parse_mem_usage("12.5MiB / 1.9GiB"),
            Some((13_107_200, 2_040_109_465))
        );
    }

    #[test]
    fn parse_stats_json_joins_services_by_container_name() {
        let ps = concat!(
            r#"{"Name":"rateme-web-1","Service":"web","State":"running","Health":"healthy"}"#,
            "\n",
            r#"{"Name":"rateme-db-1","Service":"db","State":"running"}"#,
            "\n",
        );
        let stats = concat!(
            r#"{"Name":"rateme-web-1","CPUPerc":"1.25%","MemUsage":"100MiB / 1GiB"}"#,
            "\n",
            r#"{"Name":"rateme-db-1","CPUPerc":"0.00%","MemUsage":"50MiB / 1GiB"}"#,
            "\n",
            r#"{"Name":"other-app-1","CPUPerc":"9.99%","MemUsage":"1MiB / 1GiB"}"#,
            "\n",
        );
        let out = parse_stats_json(ps, stats);
        assert_eq!(out.len(), 2, "foreign containers are ignored");
        assert_eq!(out[0].service, "web");
        assert_eq!(out[0].cpu_percent, 1.25);
        assert_eq!(out[0].mem_used_bytes, 100 * 1024 * 1024);
        assert_eq!(out[0].mem_limit_bytes, 1024 * 1024 * 1024);
        assert_eq!(out[0].state, "running");
        assert_eq!(out[0].health.as_deref(), Some("healthy"));
        assert_eq!(out[1].service, "db");
        assert_eq!(out[1].state, "running");
        assert_eq!(out[1].health, None);
    }

    #[test]
    fn container_names_reads_both_ndjson_and_array() {
        let ndjson = "{\"Name\":\"a-web-1\",\"Service\":\"web\"}\n";
        assert_eq!(container_names(ndjson), vec!["a-web-1".to_string()]);
        let array = r#"[{"Name":"a-web-1","Service":"web"}]"#;
        assert_eq!(container_names(array), vec!["a-web-1".to_string()]);
    }

    #[test]
    fn parse_ps_json_reads_ndjson_lines() {
        let out = concat!(
            r#"{"Service":"web","State":"running","Name":"rateme-web-1"}"#,
            "\n",
            r#"{"Service":"db","State":"exited","Name":"rateme-db-1"}"#,
            "\n",
            "garbage-line\n"
        );
        assert_eq!(
            parse_ps_json(out),
            vec![
                ServiceState {
                    service: "web".into(),
                    state: "running".into(),
                    health: None
                },
                ServiceState {
                    service: "db".into(),
                    state: "exited".into(),
                    health: None
                },
            ]
        );
    }

    #[test]
    fn parse_ps_json_reads_health_field() {
        let out = concat!(
            r#"{"Service":"web","State":"running","Health":"healthy"}"#,
            "\n",
            r#"{"Service":"db","State":"running","Health":""}"#,
            "\n",
            r#"{"Service":"worker","State":"running"}"#,
            "\n",
        );
        let states = parse_ps_json(out);
        assert_eq!(states[0].health.as_deref(), Some("healthy"));
        assert_eq!(states[1].health, None, "empty Health means no healthcheck");
        assert_eq!(states[2].health, None);
    }

    #[test]
    fn parse_ps_json_reads_legacy_array_format() {
        let out = r#"[{"Service":"web","State":"running"},{"Service":"db","State":"exited"}]"#;
        assert_eq!(
            parse_ps_json(out),
            vec![
                ServiceState {
                    service: "web".into(),
                    state: "running".into(),
                    health: None
                },
                ServiceState {
                    service: "db".into(),
                    state: "exited".into(),
                    health: None
                },
            ]
        );
    }
}

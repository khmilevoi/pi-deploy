use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;

use crate::cli::api::{ApiClient, DeployStreamEvent};
use crate::cli::config::ConnectOpts;
use crate::cli::connect::AgentConn;
use crate::cli::rpitoml::SecretsSection;
use crate::cli::ssh::SshExec;
use crate::cli::tunnel::SshTunnel;
use crate::duration::parse_duration_secs;
use crate::output;
use crate::proto::{DeployRequest, DiagnosticCheckDto};

pub async fn deploy(
    git_ref: Option<String>,
    no_gh_key: bool,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let rpitoml = resolved.rpitoml;
    let env_selection = resolved.env;
    let project = rpitoml.to_project_config();
    output::show_deploy_banner(&rpitoml.project.name);

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    output::status(format!(
        "agent {} (api {})",
        compat.agent_version(),
        compat.agent_api()
    ));

    if env_selection.is_some() {
        compat.gate(crate::compat::Feature::Environments)?;
    }

    if compat.gate(crate::compat::Feature::SourceCheck)? {
        crate::cli::sourcekey::preflight(
            &crate::cli::sourcekey::GhCli,
            &api,
            &rpitoml.project.name,
            &project.repo,
            no_gh_key,
        )
        .await?;
    }

    let req = DeployRequest {
        project: (&project).into(),
        git_ref,
        environment: env_selection
            .as_ref()
            .map(|s| crate::proto::EnvironmentDto {
                env: s.env.clone(),
                base: s.base.clone(),
                slug: s.slug.clone(),
                ttl_secs: s.ttl_secs,
                on_create: s.on_create.clone(),
            }),
    };
    let started = std::time::Instant::now();
    let accepted = api.deploy(&req).await?;
    if accepted.queued {
        output::status(format!(
            "deployment {} queued behind the active deploy (latest wins); waiting...",
            accepted.deployment_id
        ));
    } else {
        output::status(format!(
            "deployment {} started; streaming logs:",
            accepted.deployment_id
        ));
    }

    let mut pipeline = output::Pipeline::new(&rpitoml.project.name);
    let mut warnings: Vec<String> = Vec::new();
    let status = api
        .follow_logs(&accepted.deployment_id, |ev| match ev {
            DeployStreamEvent::Line(line) => {
                if let Some(w) = deploy_warning(line) {
                    warnings.push(w.to_string());
                }
                pipeline.push_line(line)
            }
            DeployStreamEvent::Stage(dto) => {
                pipeline.stage(&dto.stage, &dto.status, dto.elapsed_ms)
            }
            DeployStreamEvent::Summary { services } => pipeline.summary(services),
        })
        .await?;
    let elapsed = started.elapsed();
    let name = &rpitoml.project.name;
    let url = rpitoml.ingress.hostname.as_deref();
    let services = pipeline.services();
    match status.as_str() {
        "success" => pipeline.finish_ok(&output::deploy_stamp(
            output::StampOutcome::Success,
            name,
            url,
            services,
            elapsed,
        )),
        "superseded" => pipeline.finish_neutral(&output::deploy_stamp(
            output::StampOutcome::Superseded,
            name,
            url,
            services,
            elapsed,
        )),
        _ => {
            pipeline.finish_err(&output::deploy_stamp(
                output::StampOutcome::Failed,
                name,
                url,
                services,
                elapsed,
            ));
            for w in &warnings {
                output::warn(w);
            }
            drop(_tunnel);
            std::process::exit(1);
        }
    }
    for w in &warnings {
        output::warn(w);
    }
    Ok(())
}

pub async fn deploy_cancel(
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let rpitoml = resolved.rpitoml;
    let project_name = rpitoml.project.name.clone();

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;

    let active = api.active_deployments(&project_name).await?;
    if active.is_empty() {
        output::status(format!(
            "no active deployment for '{project_name}' - nothing to cancel"
        ));
        return Ok(());
    }
    // One failed cancel (e.g. a deploy that finished in the meantime) must not
    // leave the rest of the active list untouched.
    let mut failures = 0usize;
    for d in active {
        match api.cancel_deployment(&d.id).await {
            Ok(decision) => {
                output::status(format!("deployment {} ({}): {decision}", d.id, d.status))
            }
            Err(err) => {
                failures += 1;
                output::error(format!(
                    "deployment {} ({}): cancel failed: {err}",
                    d.id, d.status
                ));
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures} cancel request(s) failed");
    }
    Ok(())
}

pub async fn secrets_send(
    apply: bool,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let is_env = resolved.env.is_some();
    let rpitoml = resolved.rpitoml;
    let project_name = rpitoml.project.name.clone();
    let (vars, files) = collect_secrets(Path::new("."), &rpitoml.secrets)?;
    if vars.is_empty() && files.is_empty() {
        anyhow::bail!("no secrets to send: env file has no variables and [secrets].files is empty");
    }

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Secrets)?;
    if is_env {
        compat.gate(crate::compat::Feature::Environments)?;
    }
    let file_mode = match &rpitoml.secrets.file_mode {
        Some(text) => Some(
            pi_domain::secretmode::parse(text)
                .map_err(|e| anyhow::anyhow!("rpi.toml [secrets].file_mode: {e}"))?,
        ),
        None => None,
    };
    if file_mode.is_some() {
        compat.gate(crate::compat::Feature::SecretModes)?;
    }

    let (n, m) = (vars.len(), files.len());
    let resp = api
        .send_secrets(&project_name, vars, files, file_mode, apply)
        .await?;
    output::success(format!(
        "saved {n} key(s) and {m} file(s) for project '{project_name}'"
    ));
    if resp.applied {
        output::success("secrets applied to running containers");
    }
    Ok(())
}

/// Assemble the outgoing bundle per secrets spec §3: an explicitly configured
/// env file must exist, the default ".env" may be absent; all missing
/// [secrets].files are reported in one error; limits match the agent's.
///
/// Every path is resolved with `secretpath::resolve_within_root` before it is
/// opened: `rpi.toml` parsing already rejects `..`/absolute strings via
/// `validate_rel_path`, but that is a string-only check and cannot see that a
/// path component is, on disk, a symlink pointing outside the project root
/// (e.g. a git-tracked symlink committed by a malicious contributor). Without
/// this check `rpi secrets send` would follow such a symlink and upload
/// whatever it points to — anywhere on the filesystem the invoking user can
/// read — to the remote agent.
fn collect_secrets(
    root: &Path,
    section: &SecretsSection,
) -> anyhow::Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let vars = match &section.env {
        Some(name) => {
            let display = root.join(name);
            let real = pi_infrastructure::secretpath::resolve_within_root(root, name)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", display.display()))?;
            let raw = std::fs::read_to_string(&real)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", display.display()))?;
            parse_env_file(&raw)?
        }
        None => match pi_infrastructure::secretpath::resolve_within_root(root, ".env") {
            Ok(real) => {
                let raw = std::fs::read_to_string(&real)
                    .map_err(|e| anyhow::anyhow!("cannot read .env: {e}"))?;
                parse_env_file(&raw)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(anyhow::anyhow!("cannot read .env: {e}")),
        },
    };

    // The RPI_ namespace is injected by the agent at deploy time. Accepting a
    // secret with that prefix would make it ambiguous which side wins inside
    // the container, so it is refused here rather than silently overridden.
    if let Some(key) = vars.keys().find(|k| k.starts_with("RPI_")) {
        anyhow::bail!(
            "secret key '{key}' uses the reserved RPI_ prefix (rpi injects those at deploy time) - rename it"
        );
    }

    let mut files = BTreeMap::new();
    let mut missing: Vec<&str> = Vec::new();
    let mut total: usize = 0;
    for rel in &section.files {
        let display = root.join(rel);
        let real = match pi_infrastructure::secretpath::resolve_within_root(root, rel) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(rel);
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", display.display())),
        };
        let bytes = match std::fs::read(&real) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(rel);
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", display.display())),
        };
        if bytes.len() > crate::proto::MAX_SECRET_FILE_BYTES {
            anyhow::bail!("secret file '{rel}' is {} bytes; max is 1 MiB", bytes.len());
        }
        total += bytes.len();
        if total > crate::proto::MAX_SECRETS_BUNDLE_BYTES {
            anyhow::bail!("secret files exceed 8 MiB total");
        }
        files.insert(
            rel.clone(),
            base64::engine::general_purpose::STANDARD.encode(&bytes),
        );
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "secret file(s) not found: {} (paths are relative to the project root)",
            missing.join(", ")
        );
    }
    Ok((vars, files))
}

pub async fn gc(connect: ConnectOpts) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;

    let resp = api.gc().await?;
    output::success(format!(
        "gc done: disk {}% used; build cache pruned: {}",
        resp.disk_used_percent,
        if resp.builder_pruned { "yes" } else { "no" }
    ));
    Ok(())
}

pub async fn secrets_ls(
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let is_env = resolved.env.is_some();
    let rpitoml = resolved.rpitoml;
    let project_name = rpitoml.project.name.clone();

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Secrets)?;
    if is_env {
        compat.gate(crate::compat::Feature::Environments)?;
    }

    let resp = api.list_secrets(&project_name).await?;
    if resp.keys.is_empty() && resp.files.is_empty() {
        output::info(format!("no secrets stored for project '{project_name}'"));
        return Ok(());
    }
    if let Some(mode) = file_mode_to_print(&resp) {
        output::info(format!("file mode: {mode:04o}"));
    }
    if !resp.keys.is_empty() {
        output::heading("env keys:");
        for key in &resp.keys {
            println!("  {key}");
        }
    }
    if !resp.files.is_empty() {
        output::heading("files:");
        for file in &resp.files {
            println!("  {file}");
        }
    }
    Ok(())
}

/// The `file mode` reported by `rpi secrets ls` is `bundle.secret_file_mode()`
/// — the mode applied to files listed in `[secrets].files`. A bundle with no
/// files never writes anything at that mode (env vars always land in `.env`
/// at `bundle.env_mode()`, `0600` by default), so printing it there would
/// show an operator debugging permissions a number that describes nothing
/// that exists on disk. Only print when there is at least one file it
/// actually governs.
fn file_mode_to_print(resp: &crate::proto::SecretsListResponse) -> Option<u32> {
    if resp.files.is_empty() {
        return None;
    }
    resp.file_mode
}

/// Same dotenv dialect as the agent's PUT validation (§10, plan Task 3):
/// anything accepted here is accepted server-side, and vice versa.
fn parse_env_file(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let bundle = pi_infrastructure::dotenv::parse(text).map_err(|e| anyhow::anyhow!(e))?;
    Ok(bundle.vars)
}

/// `rpi ls` EXPOSE cell: `-` for private/unknown, `lan http://<ip>:<port>` for
/// expose=lan with a detected ip, `lan (ip n/a)` when the ip could not be
/// detected (§12.1).
fn expose_cell(expose: &str, lan_ip: Option<&str>, host_port: u16) -> String {
    match expose {
        "lan" => match lan_ip {
            Some(ip) => format!("lan http://{ip}:{host_port}"),
            None => "lan (ip n/a)".to_string(),
        },
        _ => "-".to_string(),
    }
}

pub async fn ls(connect: ConnectOpts) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;

    let projects = api.projects().await?;
    if projects.is_empty() {
        output::info("no projects deployed yet");
        return Ok(());
    }
    let mut table = output::table();
    table.set_header(output::header([
        "NAME", "BRANCH", "HOSTNAME", "PORT", "EXPOSE", "SERVICES",
    ]));
    for p in projects {
        let services = if p.services.is_empty() {
            "-".to_string()
        } else {
            p.services
                .iter()
                .map(|s| format!("{}:{}", s.service, s.state))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let services_sem = output::services_sem(
            &p.services
                .iter()
                .map(|s| s.state.as_str())
                .collect::<Vec<_>>(),
        );
        let expose = expose_cell(&p.expose, p.lan_ip.as_deref(), p.host_port);
        table.add_row(vec![
            output::cell(p.name),
            output::cell(p.branch),
            output::cell(p.hostname.unwrap_or_else(|| "-".into())),
            output::cell(p.host_port.to_string()),
            output::cell(expose),
            output::cell_sem(services, services_sem),
        ]);
    }
    println!("{table}");
    Ok(())
}

pub async fn logs(
    project: String,
    follow: bool,
    tail: usize,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    api.stream_sse(
        &format!("/v1/projects/{project}/logs?tail={tail}&follow={follow}"),
        |line| println!("{line}"),
    )
    .await
}

pub async fn stats(
    project: Option<String>,
    json: bool,
    watch: bool,
    interval: u64,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Stats)?;

    if watch {
        // `_tunnel` from the destructure above keeps the SSH tunnel alive
        // for the duration of the watch loop.
        return crate::cli::stats_tui::stats_watch(api, project, interval).await;
    }

    let resp = api.stats(project.as_deref()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    print!("{}", crate::cli::stats_render::render_stats_static(&resp));

    if resp.host_history.is_empty() {
        output::warn("no host history from the agent - update the agent on the Pi to see graphs");
    }
    if resp
        .projects
        .iter()
        .flat_map(|p| &p.services)
        .any(|s| s.mem_limit_bytes == 0)
    {
        output::warn(
            "per-service memory shows n/a: enable cgroup memory accounting on the Pi \
             (run `rpi doctor` for the fix)",
        );
    }
    Ok(())
}

pub async fn lifecycle(project: String, action: &str, connect: ConnectOpts) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    api.lifecycle(&project, action).await?;
    output::success(format!("{action} '{project}': done"));
    Ok(())
}

fn format_command_line(name: &str, spec: &pi_domain::entities::CommandSpec) -> String {
    let base = format!("{name}  ->  {}", spec.argv.join(" "));
    match &spec.service {
        Some(service) => format!("{base}  [service: {service}]"),
        None => base,
    }
}

pub async fn command(
    name: Option<String>,
    args: Vec<String>,
    env: Option<String>,
    vars: Vec<String>,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    let is_env = resolved.env.is_some();
    let rpitoml = resolved.rpitoml;
    let project_name = rpitoml.project.name.clone();

    let AgentConn {
        tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Commands)?;
    if is_env {
        compat.gate(crate::compat::Feature::Environments)?;
    }

    let Some(name) = name else {
        // List mode: the agent's answer is the deployed reality; the local
        // file only powers the "undeployed changes" hint.
        let resp = api.list_commands(&project_name).await?;
        if resp.commands.is_empty() {
            output::status(format!(
                "no commands deployed for '{project_name}' - declare [commands] in rpi.toml and run `rpi deploy`"
            ));
        } else {
            for (cmd, spec) in &resp.commands {
                println!("{}", format_command_line(cmd, spec));
            }
        }
        let local = rpitoml.to_project_config().commands;
        let undeployed: Vec<&str> = local
            .keys()
            .filter(|k| !resp.commands.contains_key(*k))
            .map(String::as_str)
            .collect();
        if !undeployed.is_empty() {
            output::note(format!(
                "local rpi.toml declares undeployed command(s): {} - run `rpi deploy`",
                undeployed.join(", ")
            ));
        }
        return Ok(());
    };

    // The remote output *is* the result the operator came for, so it streams
    // straight to stdout — unframed, unwindowed, untruncated — and only the
    // verdict goes to stderr. That keeps `rpi command … > file` a clean capture.
    let code = api
        .run_command(&project_name, &name, &args, output::log_line)
        .await?;
    if code != 0 {
        output::error(format!("command '{name}' exited with code {code}"));
        drop(tunnel);
        std::process::exit(code);
    }
    output::success(format!("command '{name}' finished (exit 0)"));
    Ok(())
}

pub async fn rm(
    project: String,
    volumes: bool,
    yes: bool,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    if !yes {
        output::warn(format!(
            "this removes containers{}, the ingress route, workdir, secrets, deploy key and history of '{project}'",
            if volumes { ", VOLUMES (project data!)" } else { "" }
        ));
        eprint!("type the project name to confirm: ");
        use std::io::Write;
        std::io::stderr().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim() != project {
            anyhow::bail!("confirmation failed: expected '{project}'");
        }
    }

    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    let resp = api.remove_project(&project, volumes).await?;
    output::success(format!(
        "project '{}' removed{}",
        resp.project,
        if resp.volumes_removed {
            " (volumes included)"
        } else {
            " (volumes kept)"
        }
    ));
    if let Some(hostname) = resp.hostname {
        output::note(format!(
            "if the agent has Cloudflare ingress enabled, the DNS record for {hostname} was removed; \
             otherwise delete it manually in the Cloudflare dashboard"
        ));
    }
    Ok(())
}

pub async fn status(json: bool, connect: ConnectOpts) -> anyhow::Result<()> {
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat: _compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    let resp = api.agent_status().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    print_agent_status(&resp);
    Ok(())
}

fn print_agent_status(resp: &crate::proto::AgentOverviewDto) {
    let mut table = output::table();
    table.set_header(output::header(["FIELD", "VALUE"]));
    table.add_row(vec![
        "agent".to_string(),
        format!("v{} (cli v{})", resp.version, env!("CARGO_PKG_VERSION")),
    ]);
    table.add_row(vec!["uptime".to_string(), human_duration(resp.uptime_secs)]);
    table.add_row(vec![
        "disk".to_string(),
        format!("{}% used", resp.disk_used_percent),
    ]);
    table.add_row(vec!["projects".to_string(), resp.projects.to_string()]);
    table.add_row(vec![
        "active deployments".to_string(),
        resp.active_deployments.to_string(),
    ]);
    println!("{table}");
}

pub(crate) fn render_doctor(checks: &[DiagnosticCheckDto]) -> (String, bool) {
    let mut out = String::new();
    let mut ok = true;
    for c in checks {
        let mark = if c.passed {
            output::styled_ok("PASS")
        } else {
            ok = false;
            output::styled_err("FAIL")
        };
        out.push_str(&format!("{mark}  {} - {}\n", c.name, c.detail));
        if let (false, Some(hint)) = (c.passed, &c.hint) {
            out.push_str(&format!("      hint: {hint}\n"));
        }
    }
    (out, ok)
}

fn check(name: &str, passed: bool, detail: String, hint: Option<&str>) -> DiagnosticCheckDto {
    DiagnosticCheckDto {
        name: name.to_string(),
        passed,
        detail,
        hint: if passed {
            None
        } else {
            hint.map(str::to_string)
        },
    }
}

pub async fn doctor(connect: ConnectOpts) -> anyhow::Result<()> {
    let profile = connect.resolve()?;
    let mut checks: Vec<DiagnosticCheckDto> = Vec::new();
    let ssh = SshExec { profile: &profile };
    checks.push(match ssh.check().await {
        Ok(()) => check(
            "ssh connection",
            true,
            format!("{}@{}", profile.user, profile.host),
            None,
        ),
        Err(e) => check(
            "ssh connection",
            false,
            e,
            Some("check host/user/key in ~/.config/pi/config.toml; try plain `ssh` manually"),
        ),
    });
    match SshTunnel::open(&profile).await {
        Err(e) => checks.push(check(
            "agent tunnel",
            false,
            e.to_string(),
            Some("is rpi-agent.service running on the Pi? try `rpi agent status`"),
        )),
        Ok(tunnel) => {
            let api = ApiClient::new(tunnel.base_url.clone());
            match api.version().await {
                Err(e) => checks.push(check(
                    "agent api",
                    false,
                    e.to_string(),
                    Some("agent is unreachable through the tunnel; `rpi agent logs` for details"),
                )),
                Ok(v) => {
                    checks.push(check(
                        "agent api",
                        true,
                        format!("agent v{} (api {})", v.version, v.api),
                        None,
                    ));
                    let cli_version = env!("CARGO_PKG_VERSION");
                    checks.push(check(
                        "version match",
                        v.version == cli_version,
                        format!("cli v{cli_version}, agent v{}", v.version),
                        Some(crate::compat::version_skew_hint(cli_version, &v.version)),
                    ));
                    match api.doctor().await {
                        Ok(resp) => checks.extend(resp.checks),
                        Err(e) => checks.push(check(
                            "agent doctor",
                            false,
                            e.to_string(),
                            Some("agent is older than v0.4? update it on the Pi"),
                        )),
                    }
                }
            }
        }
    }
    let (rendered, ok) = render_doctor(&checks);
    print!("{rendered}");
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn agent_status(connect: ConnectOpts) -> anyhow::Result<()> {
    let profile = connect.resolve()?;
    let api_attempt = async {
        let tunnel = SshTunnel::open(&profile).await?;
        ApiClient::new(tunnel.base_url.clone()).agent_status().await
    };
    match api_attempt.await {
        Ok(resp) => {
            print_agent_status(&resp);
            Ok(())
        }
        Err(err) => {
            output::warn(format!("agent API unreachable ({err})"));
            output::note(format!(
                "falling back to: ssh {}@{} systemctl status rpi-agent",
                profile.user, profile.host
            ));
            SshExec { profile: &profile }
                .run(&["systemctl", "status", "rpi-agent", "--no-pager"])
                .await
        }
    }
}

pub(crate) fn build_agent_logs_query(
    follow: bool,
    since: &Option<String>,
    tail: usize,
    now_unix: i64,
) -> anyhow::Result<String> {
    match since {
        Some(spec) => {
            let secs = parse_duration_secs(spec).map_err(|e| anyhow::anyhow!(e))?;
            let cutoff = now_unix - secs as i64;
            Ok(format!("/v1/agent/logs?since={cutoff}&follow={follow}"))
        }
        None => Ok(format!("/v1/agent/logs?tail={tail}&follow={follow}")),
    }
}

pub(crate) fn journalctl_args(follow: bool, since_unix: Option<i64>, tail: usize) -> Vec<String> {
    let mut args: Vec<String> = ["journalctl", "-u", "rpi-agent", "--no-pager", "-n"]
        .map(String::from)
        .to_vec();
    args.push(tail.to_string());
    if let Some(cutoff) = since_unix {
        args.push(format!("--since=@{cutoff}"));
    }
    if follow {
        args.push("-f".to_string());
    }
    args
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn agent_logs(
    follow: bool,
    since: Option<String>,
    tail: usize,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let profile = connect.resolve()?;
    let now = now_unix();
    let query = build_agent_logs_query(follow, &since, tail, now)?;
    let api_attempt = async {
        let tunnel = SshTunnel::open(&profile).await?;
        let api = ApiClient::new(tunnel.base_url.clone());
        api.stream_sse(&query, |line| println!("{line}")).await
    };
    match api_attempt.await {
        Ok(()) => Ok(()),
        Err(err) => {
            output::warn(format!("agent API unreachable ({err})"));
            let since_unix = since
                .as_deref()
                .and_then(|s| parse_duration_secs(s).ok())
                .map(|secs| now - secs as i64);
            let args = journalctl_args(follow, since_unix, tail);
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            output::note(format!(
                "falling back to: ssh {}@{} {}",
                profile.user,
                profile.host,
                args.join(" ")
            ));
            SshExec { profile: &profile }.run(&args_ref).await
        }
    }
}

pub(crate) fn human_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// A deploy log line the agent marked as a warning — re-surfaced next to the
/// final summary so it cannot scroll away with the stream.
fn deploy_warning(line: &str) -> Option<&str> {
    line.strip_prefix("warning: ")
}

/// `rpi config show`: resolve `./rpi.toml` (+ `./rpi.<env>.toml` overlay when
/// `--env` is given) and print the merged configuration as TOML. Local-only
/// — no agent connection.
pub async fn config_show(env: Option<String>, vars: Vec<String>) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    print!("{}", crate::cli::overlay::render_resolved(&resolved)?);
    print!("{}", render_runtime_preview(&resolved));
    Ok(())
}

/// The `RPI_*` variables a deploy of this configuration would export, as far
/// as they are knowable locally. `RPI_HOST_PORT` and `RPI_COMMIT_SHA` are
/// assigned by the agent (port allocation and fetch respectively), so they
/// are shown as placeholders rather than omitted — an operator debugging a
/// missing variable needs to see that they exist.
pub fn render_runtime_preview(r: &crate::cli::overlay::Resolved) -> String {
    let mut out = String::from("\n[runtime]\n");
    // Values here (e.g. RPI_BRANCH_NAME, sourced from source.branch) are not
    // character-validated and can carry `--vars`-supplied quotes, backslashes
    // or control characters. Serializing through toml::Value guarantees the
    // same valid-TOML-output guarantee render_resolved gets from serde,
    // instead of hand-formatting a string literal that could break parsing.
    let mut put = |key: &str, value: &str| {
        let escaped = toml::Value::String(value.to_string());
        out.push_str(&format!("{key} = {escaped}\n"));
    };
    put("RPI_PROJECT", &r.rpitoml.project.name);
    match &r.env {
        Some(env) => {
            put("RPI_PROJECT_BASE", &env.base);
            put("RPI_ENV", &env.env);
            if let Some(slug) = &env.slug {
                put("RPI_ENV_SLUG", slug);
            }
        }
        None => put("RPI_PROJECT_BASE", &r.rpitoml.project.name),
    }
    put("RPI_BRANCH_NAME", &r.rpitoml.source.branch);
    if let Some(hostname) = &r.rpitoml.ingress.hostname {
        put("RPI_HOSTNAME", hostname);
    }
    put("RPI_HOST_PORT", "<assigned by agent>");
    put("RPI_COMMIT_SHA", "<assigned by agent>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::rpitoml::SecretsSection;

    fn section(env: Option<&str>, files: &[&str]) -> SecretsSection {
        SecretsSection {
            env: env.map(str::to_string),
            files: files.iter().map(|s| s.to_string()).collect(),
            file_mode: None,
        }
    }

    const OVERLAY_BASE: &str = r#"
schema = 1

[project]
name = "myapp"

[source]
repo = "git@github.com:acme/myapp.git"
branch = "main"

[ingress]
hostname = "app.example.com"
service = "web"
port = 3000
"#;

    #[test]
    fn deploy_warning_extracts_only_prefixed_lines() {
        assert_eq!(deploy_warning("warning: x y"), Some("x y"));
        assert_eq!(deploy_warning("ingress: routing a -> b"), None);
        assert_eq!(deploy_warning(" warning: not at start"), None);
    }

    #[test]
    fn collect_reads_env_and_files_as_base64() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        std::fs::create_dir_all(dir.path().join("certs")).unwrap();
        std::fs::write(dir.path().join("certs/server.pem"), b"PEM").unwrap();

        let (vars, files) =
            collect_secrets(dir.path(), &section(None, &["certs/server.pem"])).unwrap();
        assert_eq!(vars["A"], "1");
        assert_eq!(files["certs/server.pem"], "UEVN"); // base64("PEM")
    }

    #[test]
    fn explicit_env_file_must_exist_but_default_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

        let err = collect_secrets(dir.path(), &section(Some(".env.prod"), &[])).unwrap_err();
        assert!(err.to_string().contains(".env.prod"), "got: {err}");

        let (vars, files) = collect_secrets(dir.path(), &section(None, &["f.txt"])).unwrap();
        assert!(vars.is_empty(), "missing default .env is fine");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn all_missing_files_are_reported_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let err = collect_secrets(dir.path(), &section(None, &["a.pem", "b.pem"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a.pem") && msg.contains("b.pem"), "got: {msg}");
    }

    #[test]
    fn oversized_file_is_rejected_locally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("big.bin"),
            vec![0u8; crate::proto::MAX_SECRET_FILE_BYTES + 1],
        )
        .unwrap();
        let err = collect_secrets(dir.path(), &section(None, &["big.bin"])).unwrap_err();
        assert!(err.to_string().contains("1 MiB"), "got: {err}");
    }

    /// Defense-in-depth: `rpi.toml`'s own `validate_rel_path` should already
    /// reject a literal `..` in `[secrets].files`, but `collect_secrets` must
    /// not blindly trust that upstream check either (a `SecretsSection` can
    /// be built directly, and this is also the last line of defense against
    /// a symlink resolving outside the root, exercised by the `cfg(unix)`
    /// tests below).
    #[test]
    fn collect_secrets_rejects_file_path_escaping_root() {
        let dir = tempfile::tempdir().unwrap();
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("escaped.txt"), b"outside-secret").unwrap();
        let outer_name = outer.path().file_name().unwrap().to_str().unwrap();
        let rel = format!("../{outer_name}/escaped.txt");

        let result = collect_secrets(dir.path(), &section(None, &[&rel]));
        assert!(
            result.is_err(),
            "must not read a file outside the project root"
        );
    }

    #[test]
    fn collect_secrets_rejects_env_path_escaping_root() {
        let dir = tempfile::tempdir().unwrap();
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("prod.env"), b"SECRET=leak\n").unwrap();
        let outer_name = outer.path().file_name().unwrap().to_str().unwrap();
        let rel = format!("../{outer_name}/prod.env");

        let result = collect_secrets(dir.path(), &section(Some(&rel), &[]));
        assert!(
            result.is_err(),
            "must not read an env file outside the project root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_secrets_rejects_symlinked_file_entry() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("id_rsa"), b"PRIVATE-KEY").unwrap();
        std::os::unix::fs::symlink(outside.path().join("id_rsa"), dir.path().join("certs.pem"))
            .unwrap();

        let result = collect_secrets(dir.path(), &section(None, &["certs.pem"]));
        assert!(
            result.is_err(),
            "must not follow a symlink out of the project root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_secrets_rejects_symlinked_default_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("real.env"), b"SECRET=leak\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("real.env"), dir.path().join(".env"))
            .unwrap();

        let result = collect_secrets(dir.path(), &section(None, &[]));
        assert!(
            result.is_err(),
            "must not follow a symlink out of the project root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_secrets_rejects_symlinked_explicit_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("real.env"), b"SECRET=leak\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("real.env"), dir.path().join("prod.env"))
            .unwrap();

        let result = collect_secrets(dir.path(), &section(Some("prod.env"), &[]));
        assert!(
            result.is_err(),
            "must not follow a symlink out of the project root"
        );
    }

    #[test]
    fn secrets_reject_the_reserved_rpi_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\nRPI_BRANCH_NAME=nope\n").unwrap();
        let err = collect_secrets(dir.path(), &section(None, &[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("RPI_BRANCH_NAME"), "got: {err}");
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn runtime_preview_lists_local_values_and_marks_agent_side_ones() {
        let resolved = crate::cli::overlay::resolve_from(
            OVERLAY_BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"${env.slug}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap();
        let text = render_runtime_preview(&resolved);
        assert!(text.contains("[runtime]"), "got:\n{text}");
        assert!(
            text.contains("RPI_PROJECT = \"myapp--branch--feature-login\""),
            "got:\n{text}"
        );
        assert!(
            text.contains("RPI_PROJECT_BASE = \"myapp\""),
            "got:\n{text}"
        );
        assert!(text.contains("RPI_ENV = \"branch\""), "got:\n{text}");
        assert!(
            text.contains("RPI_ENV_SLUG = \"feature-login\""),
            "got:\n{text}"
        );
        assert!(
            text.contains("RPI_BRANCH_NAME = \"feature/login\""),
            "got:\n{text}"
        );
        assert!(
            text.contains("RPI_HOSTNAME = \"feature-login.preview.example.com\""),
            "got:\n{text}"
        );
        assert!(
            text.contains("RPI_HOST_PORT = \"<assigned by agent>\""),
            "got:\n{text}"
        );
        assert!(
            text.contains("RPI_COMMIT_SHA = \"<assigned by agent>\""),
            "got:\n{text}"
        );
    }

    #[test]
    fn runtime_preview_omits_variables_that_do_not_exist() {
        let base = OVERLAY_BASE.replace("hostname = \"app.example.com\"\n", "");
        let resolved = crate::cli::overlay::resolve_from(&base, None, &[]).unwrap();
        let text = render_runtime_preview(&resolved);
        assert!(text.contains("RPI_PROJECT = \"myapp\""), "got:\n{text}");
        assert!(!text.contains("RPI_ENV "), "no env selected: {text}");
        assert!(!text.contains("RPI_ENV_SLUG"), "no slug: {text}");
        assert!(!text.contains("RPI_HOSTNAME"), "no hostname: {text}");
    }

    #[test]
    fn runtime_preview_escapes_quotes_and_backslashes_as_valid_toml() {
        // source.branch is never character-validated, and since Task 3 it can
        // be set from an arbitrary --vars value (parse_vars only checks the
        // key shape). A value containing a quote and a backslash must still
        // come out as a parseable [runtime] block, not broken TOML.
        let resolved = crate::cli::overlay::resolve_from(
            OVERLAY_BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"\"\n",
            )),
            &[r#"BRANCH_NAME=x"y\z"#.into()],
        )
        .unwrap();
        let text = render_runtime_preview(&resolved);

        let parsed: toml::Value = text.trim_start_matches('\n').parse().unwrap();
        assert_eq!(
            parsed["runtime"]["RPI_BRANCH_NAME"].as_str().unwrap(),
            "x\"y\\z",
            "got:\n{text}"
        );
    }

    #[test]
    fn env_file_parsing_matches_agent_rules() {
        let text = "# c\nexport TOKEN=\"abc=def\"\nNAME='single'\nDB=postgres://u:p@db/x\n";
        let vars = parse_env_file(text).unwrap();
        assert_eq!(vars["TOKEN"], "abc=def");
        assert_eq!(vars["NAME"], "single");
        assert_eq!(vars["DB"], "postgres://u:p@db/x");
        assert_eq!(vars.len(), 3);
        assert!(parse_env_file("1BAD=x").is_err());
    }

    #[test]
    fn render_doctor_marks_failures_and_hints() {
        let checks = vec![
            DiagnosticCheckDto {
                name: "docker daemon".into(),
                passed: true,
                detail: "27.0".into(),
                hint: None,
            },
            DiagnosticCheckDto {
                name: "disk space".into(),
                passed: false,
                detail: "91% used".into(),
                hint: Some("run `rpi gc`".into()),
            },
        ];
        let (out, ok) = render_doctor(&checks);
        assert!(!ok);
        assert!(out.contains("PASS  docker daemon"), "{out}");
        assert!(out.contains("FAIL  disk space"), "{out}");
        assert!(out.contains("hint: run `rpi gc`"), "{out}");
        let (_, ok) = render_doctor(&checks[..1]);
        assert!(ok);
    }

    #[test]
    fn agent_logs_query_prefers_since_over_tail() {
        let q = build_agent_logs_query(false, &None, 50, 1000).unwrap();
        assert_eq!(q, "/v1/agent/logs?tail=50&follow=false");
        let q = build_agent_logs_query(true, &Some("2h".into()), 50, 10_000).unwrap();
        assert_eq!(q, "/v1/agent/logs?since=2800&follow=true");
        assert!(build_agent_logs_query(false, &Some("soon".into()), 50, 0).is_err());
    }

    #[test]
    fn expose_cell_shows_lan_url_only_for_lan() {
        assert_eq!(expose_cell("private", None, 8000), "-".to_string());
        assert_eq!(expose_cell("", None, 8000), "-".to_string());
        assert_eq!(
            expose_cell("lan", Some("192.168.1.50"), 8000),
            "lan http://192.168.1.50:8000".to_string()
        );
        assert_eq!(expose_cell("lan", None, 8000), "lan (ip n/a)".to_string());
    }

    fn secrets_list_response(
        files: &[&str],
        file_mode: Option<u32>,
    ) -> crate::proto::SecretsListResponse {
        crate::proto::SecretsListResponse {
            keys: vec![],
            files: files.iter().map(|s| s.to_string()).collect(),
            file_mode,
        }
    }

    #[test]
    fn file_mode_to_print_is_none_for_a_bundle_with_no_files() {
        // A bundle with only env vars never writes anything at
        // `secret_file_mode()` — .env always lands at `env_mode()` — so
        // printing that value here would mislead an operator debugging
        // permissions.
        let resp = secrets_list_response(&[], Some(0o644));
        assert_eq!(file_mode_to_print(&resp), None);
    }

    #[test]
    fn file_mode_to_print_is_some_when_the_bundle_has_files() {
        let resp = secrets_list_response(&["certs/server.pem"], Some(0o640));
        assert_eq!(file_mode_to_print(&resp), Some(0o640));
    }

    #[test]
    fn file_mode_to_print_passes_through_a_legacy_agent_with_no_mode() {
        // Agents older than 0.26.0 never send `file_mode` at all; the CLI
        // must not fabricate one even when files are present.
        let resp = secrets_list_response(&["certs/server.pem"], None);
        assert_eq!(file_mode_to_print(&resp), None);
    }

    #[test]
    fn journalctl_args_shape() {
        assert_eq!(
            journalctl_args(false, None, 100),
            vec!["journalctl", "-u", "rpi-agent", "--no-pager", "-n", "100"]
        );
        assert_eq!(
            journalctl_args(true, Some(1234), 50),
            vec![
                "journalctl",
                "-u",
                "rpi-agent",
                "--no-pager",
                "-n",
                "50",
                "--since=@1234",
                "-f"
            ]
        );
    }

    #[cfg(test)]
    mod command_list_tests {
        use super::format_command_line;
        use pi_domain::entities::CommandSpec;

        #[test]
        fn service_less_command_shows_argv_only() {
            let spec = CommandSpec::new(vec!["node".into(), "seed.js".into()]);
            assert_eq!(format_command_line("seed", &spec), "seed  ->  node seed.js");
        }

        #[test]
        fn service_pinned_command_shows_service_suffix() {
            let spec = CommandSpec {
                argv: vec!["node".into(), "x.cjs".into()],
                service: Some("server".into()),
            };
            assert_eq!(
                format_command_line("create-invite", &spec),
                "create-invite  ->  node x.cjs  [service: server]"
            );
        }
    }
}

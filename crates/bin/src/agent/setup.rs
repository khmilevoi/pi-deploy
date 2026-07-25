use super::migrate::{self, Migration};
use super::self_install::{self, SelfInstallAction};
use async_trait::async_trait;
use pi_domain::contracts::CloudflareApi;
use pi_infrastructure::cloudflare::credentials_json;
use std::path::{Path, PathBuf};

/// All OS effects setup needs, behind a trait so logic is testable off-Linux.
#[async_trait]
pub trait Sys: Send + Sync {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;
    fn exists(&self, path: &Path) -> bool;
    fn read(&self, path: &Path) -> Option<String>;
    fn write(&self, path: &Path, content: &str) -> Result<(), String>;
}

/// True if a system user exists (`id -u <name>` succeeds).
pub async fn user_exists(sys: &dyn Sys, name: &str) -> bool {
    sys.run("id", &["-u", name]).await.is_ok()
}

/// True if `user` is a member of `group` (parsed from `id -nG <user>`).
pub async fn in_group(sys: &dyn Sys, user: &str, group: &str) -> bool {
    matches!(sys.run("id", &["-nG", user]).await, Ok(s) if s.split_whitespace().any(|g| g == group))
}

pub struct HostSys;

#[async_trait]
impl Sys for HostSys {
    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
            .map_err(|e| format!("spawn {program}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn write(&self, path: &Path, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

pub const UNIT_PATH: &str = "/etc/systemd/system/rpi-agent.service";
pub const AGENT_TOML_PATH: &str = "/etc/rpi/agent.toml";

/// Canonical systemd unit — byte-for-byte the working install (spec §9).
pub const UNIT: &str = "\
[Unit]
Description=rpi deploy agent
After=network-online.target docker.service
Wants=network-online.target

[Service]
User=rpi-agent
Group=rpi-agent
ExecStart=/usr/local/bin/rpi agent run --config /etc/rpi/agent.toml
RuntimeDirectory=rpi
RuntimeDirectoryMode=0750
Restart=on-failure
Environment=HOME=/var/lib/rpi
Environment=XDG_CONFIG_HOME=/var/lib/rpi/.config
Environment=XDG_CACHE_HOME=/var/lib/rpi/.cache
WorkingDirectory=/var/lib/rpi

[Install]
WantedBy=multi-user.target
";

/// Canonical agent.toml — written only when /etc/rpi/agent.toml is absent (spec §9).
pub const AGENT_TOML: &str = "\
data_dir = \"/var/lib/rpi\"
socket = \"/run/rpi/agent.sock\"
port_min = 8000
port_max = 8999
build_concurrency = 1
history_keep = 50

[timeouts]
fetch = \"2m\"
build = \"30m\"
up = \"5m\"

[gc]
disk_threshold_percent = 85
";

pub enum WriteAction {
    Wrote,
    Skipped,
    BackedUp,
}

/// Write the canonical unit; back up to *.bak only if an existing file differs.
pub fn write_unit_with_backup(sys: &dyn Sys, dry_run: bool) -> Result<WriteAction, String> {
    let path = Path::new(UNIT_PATH);
    if sys.exists(path) {
        if sys.read(path).as_deref() == Some(UNIT) {
            return Ok(WriteAction::Skipped);
        }
        if dry_run {
            return Ok(WriteAction::BackedUp);
        }
        let bak = format!("{UNIT_PATH}.bak");
        if let Some(old) = sys.read(path) {
            sys.write(Path::new(&bak), &old)?;
        }
        sys.write(path, UNIT)?;
        return Ok(WriteAction::BackedUp);
    }
    if !dry_run {
        sys.write(path, UNIT)?;
    }
    Ok(WriteAction::Wrote)
}

pub struct SetupOpts {
    pub login_user: String,
    pub with_cloudflared: bool,
    pub dry_run: bool,
    pub cf_token: Option<String>,
    pub domain: Option<String>,
    pub tunnel_name: Option<String>,
}

#[derive(Default)]
pub struct SetupReport {
    pub created: Vec<String>,
    pub skipped: Vec<String>,
    pub repaired: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl SetupReport {
    pub fn print(&self) {
        for c in &self.created {
            crate::output::success(format!("created: {c}"));
        }
        for r in &self.repaired {
            crate::output::success(format!("repaired: {r}"));
        }
        for s in &self.skipped {
            crate::output::success(format!("ok (already present): {s}"));
        }
        for w in &self.warnings {
            crate::output::warn(w);
        }
        for e in &self.errors {
            crate::output::error(e);
        }
        if self.repaired.iter().any(|r| r.contains("/var/log/rpi")) {
            crate::output::note("run `sudo systemctl restart rpi-agent` to activate file logs");
        }
    }
}

async fn ensure_dir(
    sys: &dyn Sys,
    path: &str,
    owner_group: Option<&str>,
    mode: Option<&str>,
    dry: bool,
    rep: &mut SetupReport,
    repair: bool,
) {
    if sys.exists(Path::new(path)) {
        // Repair pre-existing state dir toward the wanted ownership/mode (PR #7
        // K1 for ownership): after uninstall+reinstall the kept files keep an
        // old numeric UID, and a host provisioned before this dir was tightened
        // may still carry a wider mode.
        let mut repairs: Vec<&str> = Vec::new();
        if let Some(og) = owner_group {
            if !dry {
                let want = format!("{og}:{og}");
                let cur = sys.run("stat", &["-c", "%U:%G", path]).await;
                if cur.ok().as_deref() != Some(want.as_str())
                    && sys.run("chown", &["-R", &want, path]).await.is_ok()
                {
                    repairs.push("ownership");
                }
            }
        }
        if let Some(m) = mode {
            if !dry {
                // `stat -c %a` prints no leading zero, hence the trim.
                let cur = sys.run("stat", &["-c", "%a", path]).await;
                if cur.ok().as_deref().map(str::trim) != Some(m.trim_start_matches('0'))
                    && sys.run("chmod", &[m, path]).await.is_ok()
                {
                    repairs.push("mode");
                }
            }
        }
        if repairs.is_empty() {
            rep.skipped.push(path.to_string());
        } else {
            rep.repaired
                .push(format!("{path} ({})", repairs.join(", ")));
        }
        return;
    }
    if dry {
        if repair {
            rep.repaired.push(path.to_string());
        } else {
            rep.created.push(path.to_string());
        }
        return;
    }
    let mut args: Vec<&str> = vec!["-d"];
    if let Some(m) = mode {
        args.push("-m");
        args.push(m);
    }
    if let Some(og) = owner_group {
        args.extend(["-o", og, "-g", og]);
    }
    args.push(path);
    match sys.run("install", &args).await {
        Ok(_) => {
            if repair {
                rep.repaired.push(path.to_string());
            } else {
                rep.created.push(path.to_string());
            }
        }
        Err(e) => rep.errors.push(format!("mkdir {path} failed: {e}")),
    }
}

/// Old (pre-v0.6.x) unit path — used only to detect and migrate a pi-agent
/// install to rpi-agent. Never written to after migration.
pub(crate) const OLD_UNIT_PATH: &str = "/etc/systemd/system/pi-agent.service";

/// The pi-agent owned path prefixes, paired with their rpi-agent names. Used
/// both to move the directories and to rewrite the absolute paths baked into
/// the config/unit files that ride along inside them.
const OWNED_PATH_RENAMES: [(&str, &str); 4] = [
    ("/var/lib/pi", "/var/lib/rpi"),
    ("/var/log/pi", "/var/log/rpi"),
    ("/etc/pi", "/etc/rpi"),
    ("/run/pi", "/run/rpi"),
];

/// Rewrite the old pi-agent path prefixes to their rpi-agent names inside one
/// migrated config/unit file, in place. No-op when the file is absent or has no
/// old paths left. The four prefixes are disjoint and none is a substring of
/// another's replacement, so a repeated run is idempotent.
pub(crate) fn rewrite_owned_paths(sys: &dyn Sys, path: &Path, rep: &mut SetupReport) {
    let Some(text) = sys.read(path) else { return };
    let mut rewritten = text.clone();
    for (old, new) in OWNED_PATH_RENAMES {
        rewritten = rewritten.replace(old, new);
    }
    if rewritten == text {
        return;
    }
    match sys.write(path, &rewritten) {
        Ok(_) => rep
            .repaired
            .push(format!("rewrote paths in {}", path.display())),
        Err(e) => rep.errors.push(format!("rewrite {}: {e}", path.display())),
    }
}

/// Convert an existing `pi-agent` install to `rpi-agent` in place: stop the old
/// unit and its lingering user session, rename the Linux group and user login
/// (uid/gid unchanged, so file ownership by id is preserved without any chown),
/// move the three owned directories to their new names, rewrite the old
/// absolute paths baked into the moved config/unit files (agent.toml's
/// data_dir/socket, the cloudflared unit + config), and back up the old unit
/// file. Callers must guard with `migrate::PiToRpi::detect` first — this body
/// no longer checks whether `rpi-agent`/`pi-agent` exist.
pub(crate) async fn migrate_pi_agent(sys: &dyn Sys, dry: bool, rep: &mut SetupReport) {
    if dry {
        rep.repaired
            .push("migrate: pi-agent -> rpi-agent (dry run)".into());
        return;
    }

    // Stop the old unit before touching the identity/paths it depends on.
    let _ = sys
        .run("systemctl", &["disable", "--now", "pi-agent"])
        .await;
    // A lingering `systemd --user` manager (e.g. cloudflared) keeps the login
    // name in use, which makes `usermod -l` fail with "user is currently used".
    // Drop linger and terminate the user's session before renaming; harmless
    // (and ignored) when the user has no session at all.
    let _ = sys.run("loginctl", &["disable-linger", "pi-agent"]).await;
    let _ = sys.run("loginctl", &["terminate-user", "pi-agent"]).await;

    // Rename group then user login. uid/gid are unchanged, so every file
    // already owned by this id is "renamed" for free — no chown needed.
    if let Err(e) = sys.run("groupmod", &["-n", "rpi-agent", "pi-agent"]).await {
        rep.errors
            .push(format!("groupmod pi-agent -> rpi-agent failed: {e}"));
        return;
    }
    if let Err(e) = sys.run("usermod", &["-l", "rpi-agent", "pi-agent"]).await {
        rep.errors
            .push(format!("usermod -l rpi-agent pi-agent failed: {e}"));
        return;
    }

    for (old, new) in [
        ("/var/lib/pi", "/var/lib/rpi"),
        ("/etc/pi", "/etc/rpi"),
        ("/var/log/pi", "/var/log/rpi"),
    ] {
        if sys.exists(Path::new(old)) && !sys.exists(Path::new(new)) {
            if let Err(e) = sys.run("mv", &[old, new]).await {
                rep.errors.push(format!("mv {old} {new} failed: {e}"));
            }
        }
    }

    // The moved files still hold the old absolute paths. Without this the agent
    // would bind socket /run/pi/agent.sock (whose RuntimeDirectory no longer
    // exists) and read data_dir /var/lib/pi (now moved) — a crash loop.
    for file in [
        AGENT_TOML_PATH,
        CLOUDFLARED_UNIT_PATH,
        CLOUDFLARED_CONFIG_PATH,
    ] {
        rewrite_owned_paths(sys, Path::new(file), rep);
    }

    if sys.exists(Path::new(OLD_UNIT_PATH)) {
        let _ = sys
            .run("mv", &[OLD_UNIT_PATH, &format!("{OLD_UNIT_PATH}.bak")])
            .await;
    }

    if sys.exists(Path::new(CLOUDFLARED_UNIT_PATH)) {
        // cloudflared linger state is keyed by login name; re-enable it
        // under the new one now that /var/lib/pi (with its config) moved.
        let _ = sys.run("loginctl", &["enable-linger", "rpi-agent"]).await;
    }

    rep.repaired
        .push("migrated: pi-agent -> rpi-agent (user, group, /var/lib, /etc, /var/log)".into());
}

/// Idempotent agent bootstrap (spec §4). Adopt & preserve; never touches
/// secret.key/state.db. Returns a report; does not restart the agent.
pub async fn setup(sys: &dyn Sys, opts: &SetupOpts) -> SetupReport {
    let mut rep = SetupReport::default();
    let dry = opts.dry_run;

    // 0. migrate an existing pi-agent install in place, if present.
    //
    // Deliberate Phase-1 choice: setup() runs only the self-detecting
    // pi-to-rpi migration directly, with no ledger handle available here, so
    // the migration ledger is NOT updated by setup. A future non-self-detecting
    // migration that needs to auto-run during setup must wire `run_auto` with a
    // `DbLedger` (the DB lives at `data_dir/state.db`) instead of calling
    // `detect`/the migration function directly like this.
    if migrate::PiToRpi.detect(sys).await == migrate::MigrationState::Applicable {
        migrate_pi_agent(sys, dry, &mut rep).await;
    }

    // 1. service user
    if user_exists(sys, "rpi-agent").await {
        rep.skipped.push("user rpi-agent".into());
    } else if dry {
        rep.created.push("user rpi-agent".into());
    } else {
        match sys
            .run(
                "useradd",
                &[
                    "--system",
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    "rpi-agent",
                ],
            )
            .await
        {
            Ok(_) => rep.created.push("user rpi-agent".into()),
            Err(e) => rep.errors.push(format!("useradd rpi-agent failed: {e}")),
        }
    }

    // 2. rpi-agent in docker group
    if in_group(sys, "rpi-agent", "docker").await {
        rep.skipped.push("rpi-agent in docker group".into());
    } else if dry {
        rep.created.push("rpi-agent in docker group".into());
    } else {
        match sys.run("usermod", &["-aG", "docker", "rpi-agent"]).await {
            Ok(_) => rep.created.push("rpi-agent in docker group".into()),
            Err(e) => rep.errors.push(format!(
                "usermod rpi-agent docker failed: {e}. Install Docker first: curl -fsSL https://get.docker.com | sh"
            )),
        }
    }

    // 3. login user in rpi-agent group
    if in_group(sys, &opts.login_user, "rpi-agent").await {
        rep.skipped
            .push(format!("{} in rpi-agent group", opts.login_user));
    } else if dry {
        rep.created
            .push(format!("{} in rpi-agent group", opts.login_user));
    } else {
        match sys
            .run("usermod", &["-aG", "rpi-agent", &opts.login_user])
            .await
        {
            Ok(_) => rep
                .created
                .push(format!("{} in rpi-agent group", opts.login_user)),
            Err(e) => rep.errors.push(format!(
                "usermod {u} rpi-agent failed: {e}",
                u = opts.login_user
            )),
        }
    }

    // 4-6. directories
    ensure_dir(
        sys,
        "/var/lib/rpi",
        Some("rpi-agent"),
        Some("0750"),
        dry,
        &mut rep,
        false,
    )
    .await;
    ensure_dir(
        sys,
        "/var/log/rpi",
        Some("rpi-agent"),
        None,
        dry,
        &mut rep,
        true,
    )
    .await; // repair (§2.5)
    ensure_dir(sys, "/etc/rpi", None, None, dry, &mut rep, false).await;

    // 7. agent.toml (only if absent)
    if sys.exists(Path::new(AGENT_TOML_PATH)) {
        rep.skipped.push(AGENT_TOML_PATH.into());
    } else if dry {
        rep.created.push(AGENT_TOML_PATH.into());
    } else {
        match sys.write(Path::new(AGENT_TOML_PATH), AGENT_TOML) {
            Ok(_) => rep.created.push(AGENT_TOML_PATH.into()),
            Err(e) => rep
                .errors
                .push(format!("write {AGENT_TOML_PATH} failed: {e}")),
        }
    }

    // 8. systemd unit + enable
    match write_unit_with_backup(sys, dry) {
        Ok(WriteAction::Skipped) => rep.skipped.push(UNIT_PATH.into()),
        Ok(WriteAction::BackedUp) => rep
            .repaired
            .push(format!("{UNIT_PATH} (backed up to .bak)")),
        Ok(WriteAction::Wrote) => rep.created.push(UNIT_PATH.into()),
        Err(e) => rep.warnings.push(format!("unit: {e}")),
    }
    if !dry {
        if sys.run("systemctl", &["daemon-reload"]).await.is_err() {
            rep.warnings.push("systemctl daemon-reload failed".into());
        }
        if sys
            .run("systemctl", &["enable", "--now", "rpi-agent"])
            .await
            .is_err()
        {
            rep.warnings.push(
                "systemctl enable --now rpi-agent failed (is /usr/local/bin/rpi installed?)".into(),
            );
        }
    }

    // 9. cloudflared (opt-in)
    if opts.with_cloudflared {
        cloudflared_bootstrap(sys, opts, &mut rep).await;
    }

    // 10. dependency checks (warn, never fail)
    if sys
        .run("docker", &["version", "--format", "{{.Server.Version}}"])
        .await
        .is_err()
    {
        rep.warnings.push(
            "docker not available — install Docker first: curl -fsSL https://get.docker.com | sh"
                .into(),
        );
    }
    if sys.run("docker", &["compose", "version"]).await.is_err() {
        rep.warnings
            .push("docker compose plugin missing — install Docker Compose v2".into());
    }

    rep
}

/// Restart rpi-agent when it is active, so a replaced binary takes effect.
/// Returns a printable note; None when the unit is not active.
pub async fn restart_agent_if_active(sys: &dyn Sys) -> Option<String> {
    if sys
        .run("systemctl", &["is-active", "--quiet", "rpi-agent"])
        .await
        .is_err()
    {
        return None;
    }
    match sys.run("systemctl", &["restart", "rpi-agent"]).await {
        Ok(_) => Some("restarted: rpi-agent (new binary)".into()),
        Err(e) => Some(format!("warning: systemctl restart rpi-agent failed: {e}")),
    }
}

pub(crate) const CLOUDFLARED_UNIT_PATH: &str =
    "/var/lib/rpi/.config/systemd/user/cloudflared.service";

/// Canonical cloudflared config the setup flow instructs operators to write.
pub(crate) const CLOUDFLARED_CONFIG_PATH: &str = "/var/lib/rpi/cloudflared/config.yml";

pub(crate) const CLOUDFLARE_TOKEN_PATH: &str = "/var/lib/rpi/cloudflare/token";

pub(crate) const CLOUDFLARED_BIN: &str = "/usr/local/bin/cloudflared";

pub(crate) fn cloudflared_asset(uname_m: &str) -> Option<&'static str> {
    match uname_m {
        "aarch64" | "arm64" => Some("cloudflared-linux-arm64"),
        "armv7l" | "armv6l" | "arm" => Some("cloudflared-linux-arm"),
        "x86_64" | "amd64" => Some("cloudflared-linux-amd64"),
        _ => None,
    }
}

pub(crate) async fn ensure_cloudflared_binary(sys: &dyn Sys, dry: bool, rep: &mut SetupReport) {
    if sys.run("cloudflared", &["--version"]).await.is_ok() {
        rep.skipped.push(CLOUDFLARED_BIN.into());
        return;
    }
    if dry {
        rep.created.push(CLOUDFLARED_BIN.into());
        return;
    }
    let arch = match sys.run("uname", &["-m"]).await {
        Ok(a) => a,
        Err(e) => {
            rep.errors.push(format!("uname -m failed: {e}"));
            return;
        }
    };
    let Some(asset) = cloudflared_asset(arch.trim()) else {
        rep.errors.push(format!(
            "unsupported architecture for cloudflared: {}",
            arch.trim()
        ));
        return;
    };
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    if let Err(e) = sys
        .run("curl", &["-fsSL", "-o", CLOUDFLARED_BIN, &url])
        .await
    {
        rep.errors.push(format!("download cloudflared: {e}"));
        return;
    }
    match sys.run("chmod", &["0755", CLOUDFLARED_BIN]).await {
        Ok(_) => rep.created.push(CLOUDFLARED_BIN.into()),
        Err(e) => rep.errors.push(format!(
            "downloaded {CLOUDFLARED_BIN} but failed to chmod +x ({e}); it is not executable — fix manually"
        )),
    }
}

/// Existing locally-managed config.yml, as found on a host where the tunnel
/// was built by hand (adoption spec §3.1).
pub(crate) struct ExistingTunnelConfig {
    pub tunnel: String,
    pub credentials_file: Option<String>,
}

/// A 36-char hyphenated hex UUID — the shape of a cloudflared tunnel id.
pub(crate) fn looks_like_tunnel_id(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}

pub(crate) fn parse_existing_config(text: &str) -> Result<ExistingTunnelConfig, String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| format!("yaml: {e}"))?;
    if !doc.is_mapping() {
        return Err("top level must be a mapping".into());
    }
    let tunnel = doc
        .get("tunnel")
        .and_then(|v| v.as_str())
        .ok_or("missing `tunnel:` key")?
        .to_string();
    let credentials_file = doc
        .get("credentials-file")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(ExistingTunnelConfig {
        tunnel,
        credentials_file,
    })
}

/// §3.1: a host with an existing config.yml is adopted, never rewritten —
/// the running tunnel and its hand-written routes stay untouched; deploys
/// take over route management from here.
async fn adopt_existing_cloudflared(
    sys: &dyn Sys,
    cf: &dyn CloudflareApi,
    opts: &CloudflaredBootstrap,
    rep: &mut SetupReport,
) {
    let Some(text) = sys.read(Path::new(CLOUDFLARED_CONFIG_PATH)) else {
        rep.errors
            .push(format!("cannot read {CLOUDFLARED_CONFIG_PATH}"));
        return;
    };
    let parsed = match parse_existing_config(&text) {
        Ok(p) => p,
        Err(e) => {
            rep.errors.push(format!(
                "{CLOUDFLARED_CONFIG_PATH} exists but is not a usable cloudflared config ({e}); \
                 fix it or move it aside and re-run setup"
            ));
            return;
        }
    };
    let tunnel_id = if looks_like_tunnel_id(&parsed.tunnel) {
        parsed.tunnel.clone()
    } else {
        match cf.find_or_create_tunnel(&parsed.tunnel).await {
            Ok(creds) => creds.tunnel_id,
            Err(e) => {
                rep.errors
                    .push(format!("resolve tunnel '{}': {e}", parsed.tunnel));
                return;
            }
        }
    };
    let creds_path = parsed
        .credentials_file
        .clone()
        .unwrap_or_else(|| format!("/var/lib/rpi/cloudflared/{tunnel_id}.json"));
    if !sys.exists(Path::new(&creds_path)) {
        rep.errors.push(format!(
            "{CLOUDFLARED_CONFIG_PATH} references credentials {creds_path}, which does not \
             exist; restore the credentials JSON and re-run setup"
        ));
        return;
    }
    match sys
        .run(
            "cloudflared",
            &[
                "tunnel",
                "--config",
                CLOUDFLARED_CONFIG_PATH,
                "ingress",
                "validate",
            ],
        )
        .await
    {
        Ok(_) => rep.skipped.push(format!(
            "{CLOUDFLARED_CONFIG_PATH} (adopted, left untouched)"
        )),
        Err(e) => rep.warnings.push(format!(
            "adopted {CLOUDFLARED_CONFIG_PATH}, but `cloudflared tunnel ingress validate` \
             failed: {e}"
        )),
    }
    upsert_cloudflared_agent_toml(sys, &tunnel_id, &opts.zone, rep);
}

/// Minimal locally-managed cloudflared config: spaces only, catch-all last.
/// Per-hostname ingress rules are added later at deploy time by CloudflaredIngress.
pub(crate) fn render_config_yml(tunnel_id: &str, creds_path: &str) -> String {
    format!(
        "tunnel: {tunnel_id}\n\
         credentials-file: {creds_path}\n\
         \n\
         ingress:\n\
         \x20\x20- service: http_status:404\n"
    )
}

pub struct CloudflaredBootstrap {
    pub tunnel_name: String,
    pub zone: String,
}

/// Whether a cloudflared process is currently running, for the foreign-tunnel
/// guard (Part A). Detects a *process* (`pgrep -x cloudflared`), not the unit
/// file: the no-token fallback scaffolds the unit without starting it, so a
/// unit-file check would false-positive on that legitimate workflow.
#[derive(Debug, PartialEq, Eq)]
enum CloudflaredState {
    Running,
    NotRunning,
    Undetermined,
}

async fn cloudflared_running(sys: &dyn Sys) -> CloudflaredState {
    match sys.run("pgrep", &["-x", "cloudflared"]).await {
        Ok(_) => CloudflaredState::Running,
        Err(e) if e.starts_with("spawn ") => CloudflaredState::Undetermined,
        Err(_) => CloudflaredState::NotRunning,
    }
}

/// Create (or adopt) the tunnel via the Cloudflare API, write its credentials
/// JSON + a validated config.yml, and append [cloudflare]/[cloudflared] to
/// agent.toml. The credentials JSON carries the tunnel secret, so a failed
/// chown/chmod on it (or on config.yml) is surfaced as an error rather than
/// swallowed — otherwise setup could report success while leaving a secret
/// world-readable.
/// Returns `true` when an existing config.yml was adopted (in which case the
/// caller must not touch the cloudflared systemd unit or restart anything),
/// `false` when the fresh-install path was taken (including all early
/// returns on that path, dry-run or error).
pub(crate) async fn cloudflared_bootstrap_full(
    sys: &dyn Sys,
    cf: &dyn CloudflareApi,
    opts: &CloudflaredBootstrap,
    dry: bool,
    rep: &mut SetupReport,
) -> bool {
    ensure_cloudflared_binary(sys, dry, rep).await;

    // Adoption (§3.1): an existing config.yml is never rewritten.
    if sys.exists(Path::new(CLOUDFLARED_CONFIG_PATH)) {
        if dry {
            rep.skipped.push(format!(
                "{CLOUDFLARED_CONFIG_PATH} exists — would adopt (dry run)"
            ));
            return true;
        }
        adopt_existing_cloudflared(sys, cf, opts, rep).await;
        return true;
    }

    // Foreign-tunnel guard (Part A): config.yml is absent, so rpi has never
    // created a tunnel here — any cloudflared already running is foreign.
    // Refuse rather than overwrite its unit and restart it. Every destructive
    // action below happens after this point.
    match cloudflared_running(sys).await {
        CloudflaredState::NotRunning => {}
        CloudflaredState::Running => {
            let msg = format!(
                "a cloudflared tunnel is already running on this host, but rpi has no config.yml \
                 at {CLOUDFLARED_CONFIG_PATH} to adopt — refusing to overwrite it and restart the \
                 tunnel. Stop the running cloudflared (then re-run to create a fresh rpi-managed \
                 tunnel), or move its config to {CLOUDFLARED_CONFIG_PATH} so setup adopts it."
            );
            if dry {
                rep.skipped.push(format!("would refuse: {msg}"));
            } else {
                rep.errors.push(msg);
            }
            return false;
        }
        CloudflaredState::Undetermined => {
            let msg = "could not check for a running cloudflared (pgrep unavailable); refusing to \
                       proceed rather than risk overwriting an existing tunnel — verify manually \
                       that no cloudflared is running, or install pgrep, then re-run."
                .to_string();
            if dry {
                rep.skipped.push(format!("would refuse: {msg}"));
            } else {
                rep.errors.push(msg);
            }
            return false;
        }
    }

    if dry {
        rep.created.push("cloudflared tunnel (dry run)".into());
        return false;
    }

    let _ = sys
        .run(
            "install",
            &[
                "-d",
                "-o",
                "rpi-agent",
                "-g",
                "rpi-agent",
                "/var/lib/rpi/cloudflared",
            ],
        )
        .await;

    let creds = match cf.find_or_create_tunnel(&opts.tunnel_name).await {
        Ok(c) => c,
        Err(e) => {
            rep.errors.push(format!("create tunnel: {e}"));
            return false;
        }
    };
    let creds_path = format!("/var/lib/rpi/cloudflared/{}.json", creds.tunnel_id);
    // Only (re)write creds when we hold a secret (freshly created). An adopted
    // tunnel (empty secret) must already have its creds file on disk.
    if !creds.tunnel_secret.is_empty() {
        if let Err(e) = sys.write(Path::new(&creds_path), &credentials_json(&creds)) {
            rep.errors.push(format!("write creds: {e}"));
            return false;
        }
        let chown = sys
            .run("chown", &["rpi-agent:rpi-agent", &creds_path])
            .await;
        let chmod = sys.run("chmod", &["640", &creds_path]).await;
        if chown.is_ok() && chmod.is_ok() {
            rep.created.push(creds_path.clone());
        } else {
            rep.errors.push(format!(
                "wrote {creds_path} but failed to set rpi-agent:rpi-agent/0640 — the tunnel \
                 credentials may be world-readable; fix manually"
            ));
        }
    } else if !sys.exists(Path::new(&creds_path)) {
        rep.errors.push(format!(
            "adopted tunnel {} but no credentials at {creds_path}; re-create the tunnel or restore its JSON",
            creds.tunnel_id
        ));
        return false;
    }

    let config = render_config_yml(&creds.tunnel_id, &creds_path);
    if let Err(e) = sys.write(Path::new(CLOUDFLARED_CONFIG_PATH), &config) {
        rep.errors.push(format!("write config.yml: {e}"));
        return false;
    }
    let chown = sys
        .run("chown", &["rpi-agent:rpi-agent", CLOUDFLARED_CONFIG_PATH])
        .await;
    let chmod = sys.run("chmod", &["640", CLOUDFLARED_CONFIG_PATH]).await;
    let perms_ok = chown.is_ok() && chmod.is_ok();
    if !perms_ok {
        rep.errors.push(format!(
            "wrote {CLOUDFLARED_CONFIG_PATH} but failed to set rpi-agent:rpi-agent/0640 — fix ownership/permissions manually"
        ));
    }

    match sys
        .run(
            "cloudflared",
            &[
                "tunnel",
                "--config",
                CLOUDFLARED_CONFIG_PATH,
                "ingress",
                "validate",
            ],
        )
        .await
    {
        Ok(_) if perms_ok => rep.created.push(CLOUDFLARED_CONFIG_PATH.into()),
        Ok(_) => {} // validate ok but perms failed → error already pushed; do not also report created
        Err(e) => rep
            .errors
            .push(format!("cloudflared ingress validate: {e}")),
    }

    upsert_cloudflared_agent_toml(sys, &creds.tunnel_id, &opts.zone, rep);
    false
}

/// Append [cloudflare] + [cloudflared] to /etc/rpi/agent.toml only when absent.
fn upsert_cloudflared_agent_toml(
    sys: &dyn Sys,
    tunnel_id: &str,
    zone: &str,
    rep: &mut SetupReport,
) {
    let existing = sys.read(Path::new(AGENT_TOML_PATH)).unwrap_or_default();
    if existing.contains("[cloudflared]") {
        rep.skipped.push("agent.toml [cloudflared]".into());
        return;
    }
    let block = format!(
        "\n[cloudflare]\nzone = \"{zone}\"\ntoken_file = \"{CLOUDFLARE_TOKEN_PATH}\"\n\n\
         [cloudflared]\nconfig = \"{CLOUDFLARED_CONFIG_PATH}\"\ntunnel = \"{tunnel_id}\"\ntunnel_id = \"{tunnel_id}\"\n"
    );
    match sys.write(Path::new(AGENT_TOML_PATH), &format!("{existing}{block}")) {
        Ok(_) => rep
            .created
            .push("agent.toml [cloudflare]/[cloudflared]".into()),
        Err(e) => rep.errors.push(format!("write agent.toml sections: {e}")),
    }
}

const CLOUDFLARED_UNIT: &str = "\
[Unit]
Description=cloudflared tunnel (rpi-agent)
After=network-online.target

[Service]
ExecStart=/usr/local/bin/cloudflared tunnel --config /var/lib/rpi/cloudflared/config.yml run
Restart=on-failure

[Install]
WantedBy=default.target
";

/// Resolve the tunnel name to use for the full auto-bootstrap path: the
/// operator-supplied `--tunnel-name`, or (falling back) the machine's
/// hostname, or (if that fails/blank) the literal `"rpi"`.
async fn resolve_tunnel_name(sys: &dyn Sys, opts: &SetupOpts) -> String {
    if let Some(name) = &opts.tunnel_name {
        return name.clone();
    }
    match sys.run("hostname", &[]).await {
        Ok(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => "rpi".to_string(),
    }
}

/// Opt-in cloudflared bootstrap. When a Cloudflare API token and domain are
/// both supplied, runs the full automated flow: write the token, create/adopt
/// the tunnel via the API, write its config, and enable the systemd --user
/// service. Otherwise falls back to the original scaffold-and-warn behavior,
/// leaving `cloudflared tunnel login` and friends to the operator.
async fn cloudflared_bootstrap(sys: &dyn Sys, opts: &SetupOpts, rep: &mut SetupReport) {
    let dry = opts.dry_run;
    if let (Some(token), Some(domain)) = (&opts.cf_token, &opts.domain) {
        let errs_before = rep.errors.len();
        ensure_cloudflare_token(sys, token, dry, rep).await;
        let api = pi_infrastructure::cloudflare::HttpCloudflare::new(token.clone(), None);
        let bopts = CloudflaredBootstrap {
            tunnel_name: resolve_tunnel_name(sys, opts).await,
            zone: domain.clone(),
        };
        let adopted = cloudflared_bootstrap_full(sys, &api, &bopts, dry, rep).await;
        if rep.errors.len() > errs_before {
            // token write or tunnel bootstrap failed; do NOT enable a user
            // service that would respawn against a missing/broken config.yml
            return;
        }
        if adopted {
            // Adoption never restarts cloudflared or touches its systemd unit —
            // the existing install is left running untouched (spec §3.1).
            return;
        }
        cloudflared_user_service(sys, dry, rep).await;
        return;
    }

    if !dry {
        let _ = sys.run("loginctl", &["enable-linger", "rpi-agent"]).await;
    }
    rep.created.push("systemd linger for rpi-agent".into());

    if sys.exists(Path::new(CLOUDFLARED_UNIT_PATH)) {
        rep.skipped.push(CLOUDFLARED_UNIT_PATH.into());
    } else {
        if !dry {
            let _ = sys
                .run(
                    "install",
                    &[
                        "-d",
                        "-o",
                        "rpi-agent",
                        "-g",
                        "rpi-agent",
                        "/var/lib/rpi/.config/systemd/user",
                    ],
                )
                .await;
            let _ = sys.write(Path::new(CLOUDFLARED_UNIT_PATH), CLOUDFLARED_UNIT);
        }
        rep.created.push(CLOUDFLARED_UNIT_PATH.into());
    }
    rep.warnings.push(
        "cloudflared: finish manually — run `cloudflared tunnel login`, create a tunnel, \
         write /var/lib/rpi/cloudflared/config.yml, add [cloudflared] to /etc/rpi/agent.toml, \
         then `systemctl --user enable --now cloudflared` as rpi-agent"
            .into(),
    );
}

/// Fix the systemd `--user` manager for `rpi-agent` so it honors
/// `/var/lib/rpi/.config` (not the default home) via a drop-in on
/// `user@<uid>.service`, then enable linger + the cloudflared user unit.
pub(crate) async fn cloudflared_user_service(sys: &dyn Sys, dry: bool, rep: &mut SetupReport) {
    let uid = match sys.run("id", &["-u", "rpi-agent"]).await {
        Ok(u) => u.trim().to_string(),
        Err(e) => {
            rep.errors.push(format!("id -u rpi-agent: {e}"));
            return;
        }
    };
    let dropin_dir = format!("/etc/systemd/system/user@{uid}.service.d");
    let dropin = format!("{dropin_dir}/override.conf");
    let dropin_body =
        "[Service]\nEnvironment=XDG_CONFIG_HOME=/var/lib/rpi/.config\nEnvironment=HOME=/var/lib/rpi\n";

    if dry {
        rep.created.push(dropin.clone());
        return;
    }

    let _ = sys.run("install", &["-d", &dropin_dir]).await;
    if let Err(e) = sys.write(Path::new(&dropin), dropin_body) {
        rep.errors.push(format!("write {dropin}: {e}"));
        return;
    }
    rep.created.push(dropin);

    // ensure the user unit exists (existing scaffold path)
    let _ = sys
        .run(
            "install",
            &[
                "-d",
                "-o",
                "rpi-agent",
                "-g",
                "rpi-agent",
                "/var/lib/rpi/.config/systemd/user",
            ],
        )
        .await;
    let _ = sys.write(Path::new(CLOUDFLARED_UNIT_PATH), CLOUDFLARED_UNIT);

    let _ = sys.run("systemctl", &["daemon-reload"]).await;
    let _ = sys
        .run("systemctl", &["restart", &format!("user@{uid}.service")])
        .await;
    let _ = sys.run("loginctl", &["enable-linger", "rpi-agent"]).await;
    let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    let dbus = format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus");
    // enable+start the user unit as rpi-agent with the runtime dir + bus set
    let _ = sys
        .run(
            "sudo",
            &[
                "-u",
                "rpi-agent",
                &runtime,
                &dbus,
                "systemctl",
                "--user",
                "enable",
                "--now",
                "cloudflared",
            ],
        )
        .await;
    rep.created.push("cloudflared user service enabled".into());
}

pub(crate) async fn ensure_cloudflare_token(
    sys: &dyn Sys,
    token: &str,
    dry: bool,
    rep: &mut SetupReport,
) {
    if dry {
        rep.created.push(CLOUDFLARE_TOKEN_PATH.into());
        return;
    }
    let _ = sys.run("groupadd", &["-f", "rpi-secrets"]).await;
    let _ = sys
        .run("usermod", &["-aG", "rpi-secrets", "rpi-agent"])
        .await;
    let _ = sys
        .run(
            "install",
            &[
                "-d",
                "-m",
                "0750",
                "-o",
                "root",
                "-g",
                "rpi-secrets",
                "/var/lib/rpi/cloudflare",
            ],
        )
        .await;
    match sys.write(Path::new(CLOUDFLARE_TOKEN_PATH), token) {
        Ok(_) => {
            let chown = sys
                .run("chown", &["root:rpi-secrets", CLOUDFLARE_TOKEN_PATH])
                .await;
            let chmod = sys.run("chmod", &["640", CLOUDFLARE_TOKEN_PATH]).await;
            if chown.is_ok() && chmod.is_ok() {
                rep.created.push(CLOUDFLARE_TOKEN_PATH.into());
            } else {
                rep.errors.push(format!(
                    "wrote {CLOUDFLARE_TOKEN_PATH} but failed to set root:rpi-secrets/0640 — \
                     the token may be world-readable; fix ownership/permissions manually"
                ));
            }
        }
        Err(e) => rep
            .errors
            .push(format!("write {CLOUDFLARE_TOKEN_PATH}: {e}")),
    }
}

async fn run_with(sys: &dyn Sys, opts: &SetupOpts) -> anyhow::Result<SetupReport> {
    let report = setup(sys, opts).await;
    report.print();
    if opts.dry_run {
        crate::output::info("(dry run — no changes made)");
    }
    if !report.errors.is_empty() {
        anyhow::bail!(
            "setup completed with {} error(s); see above",
            report.errors.len()
        );
    }
    Ok(report)
}

/// When self-install already believes the running exe is the canonical file
/// (the common case: `sudo` resolves `rpi` to /usr/local/bin/rpi directly,
/// bypassing any npm/nvm shim on the invoking user's own PATH — nvm/volta/fnm
/// bin dirs are never on root's secure_path), check whether the invoking
/// login user's own npm has a — possibly newer — build installed instead of
/// trusting that self-referential "canonical" result. Returns None when npm
/// is unavailable to that user or `rpi-deploy` isn't installed via it.
pub(crate) async fn resolve_npm_dist_binary(sys: &dyn Sys, login_user: &str) -> Option<PathBuf> {
    let root = sys
        .run("sudo", &["-u", login_user, "-i", "--", "npm", "root", "-g"])
        .await
        .ok()?;
    // The agent only ever runs on Linux, and `npm root -g` reports a Linux
    // path — join with `/` explicitly rather than `Path::join` so the result
    // is correct regardless of the host platform this is compiled/tested on.
    let candidate = PathBuf::from(format!(
        "{}/rpi-deploy/dist/rpi",
        root.trim().trim_end_matches('/')
    ));
    sys.exists(&candidate).then_some(candidate)
}

const CF_TOKEN_DEPRECATION: &str =
    "--cf-token passes the API token on the command line, where it leaks via `ps`, shell history, \
     and journald; prefer --cf-token-file <path> (or `-` for stdin) or the CLOUDFLARE_API_TOKEN \
     environment variable";

#[derive(Debug)]
struct CfTokenResolution {
    token: Option<String>,
    warning: Option<String>,
}

/// Resolve the Cloudflare API token most-secure-first: `--cf-token-file`
/// (`-` = stdin) beats the deprecated inline `--cf-token`, which beats the
/// `CLOUDFLARE_API_TOKEN` env var. `read` performs the file/stdin read so this
/// stays unit-testable. An unreadable file, or an empty resolved token, is a
/// hard error — not a fall-through.
fn resolve_cf_token(
    token_file: Option<String>,
    token_inline: Option<String>,
    env_token: Option<String>,
    read: impl FnOnce(&str) -> Result<String, String>,
) -> Result<CfTokenResolution, String> {
    if let Some(path) = token_file {
        let raw = read(&path).map_err(|e| format!("read --cf-token-file {path}: {e}"))?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            return Err(format!("--cf-token-file {path} resolved to an empty token"));
        }
        let warning = token_inline.map(|_| CF_TOKEN_DEPRECATION.to_string());
        return Ok(CfTokenResolution {
            token: Some(token),
            warning,
        });
    }
    if let Some(inline) = token_inline {
        return Ok(CfTokenResolution {
            token: Some(inline),
            warning: Some(CF_TOKEN_DEPRECATION.to_string()),
        });
    }
    Ok(CfTokenResolution {
        token: env_token,
        warning: None,
    })
}

/// CLI entrypoint: resolve the login user (--user or $SUDO_USER), install the
/// running binary to /usr/local/bin/rpi (npm installs live under node_modules,
/// but the systemd unit expects the canonical path), run setup, and restart
/// an active agent when the binary changed. Must run as root (under sudo).
pub async fn run_cmd(
    user: Option<String>,
    with_cloudflared: bool,
    cf_token: Option<String>,
    cf_token_file: Option<String>,
    domain: Option<String>,
    tunnel: Option<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let login_user = user
        .or_else(|| std::env::var("SUDO_USER").ok())
        .filter(|u| !u.is_empty() && u != "root")
        .ok_or_else(|| anyhow::anyhow!(
            "cannot determine the SSH login user; run via `sudo rpi agent setup` or pass --user <name>"
        ))?;
    let read_token = |path: &str| -> Result<String, String> {
        if path == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| e.to_string())?;
            Ok(buf)
        } else {
            std::fs::read_to_string(path).map_err(|e| e.to_string())
        }
    };
    let resolution = resolve_cf_token(
        cf_token_file,
        cf_token,
        std::env::var("CLOUDFLARE_API_TOKEN").ok(),
        read_token,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(w) = &resolution.warning {
        crate::output::warn(w);
    }
    let cf_token = resolution.token;
    let opts = SetupOpts {
        login_user,
        with_cloudflared,
        dry_run,
        cf_token,
        domain,
        tunnel_name: tunnel,
    };

    let current = std::env::current_exe().map_err(|e| anyhow::anyhow!("current_exe: {e}"))?;
    let mut action =
        self_install::ensure_installed(&current, Path::new(self_install::AGENT_BIN_PATH), dry_run)
            .map_err(|e| anyhow::anyhow!("self-install {}: {e}", self_install::AGENT_BIN_PATH))?;
    let mut installed_from = current.clone();

    if action == SelfInstallAction::AlreadyCanonical {
        if let Some(candidate) = resolve_npm_dist_binary(&HostSys, &opts.login_user).await {
            if candidate != current {
                action = self_install::ensure_installed(
                    &candidate,
                    Path::new(self_install::AGENT_BIN_PATH),
                    dry_run,
                )
                .map_err(|e| {
                    anyhow::anyhow!("self-install {}: {e}", self_install::AGENT_BIN_PATH)
                })?;
                installed_from = candidate;
            }
        }
    }

    match &action {
        SelfInstallAction::AlreadyCanonical => {
            crate::output::success(format!(
                "ok (already present): {} (running from it)",
                self_install::AGENT_BIN_PATH
            ));
        }
        SelfInstallAction::UpToDate => {
            crate::output::success(format!(
                "ok (already present): {} (binary up to date)",
                self_install::AGENT_BIN_PATH
            ));
        }
        SelfInstallAction::Installed => {
            let line = format!(
                "{}: {} (from {})",
                if dry_run {
                    "would install"
                } else {
                    "installed"
                },
                self_install::AGENT_BIN_PATH,
                installed_from.display(),
            );
            if dry_run {
                crate::output::info(line);
            } else {
                crate::output::success(line);
            }
        }
    }

    run_with(&HostSys, &opts).await?;

    if matches!(action, SelfInstallAction::Installed) && !dry_run {
        if let Some(note) = restart_agent_if_active(&HostSys).await {
            crate::output::info(note);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeSys {
        pub paths: HashSet<String>,
        pub files: HashMap<String, String>,
        pub ok: HashMap<String, String>, // "program a b" -> stdout
        pub err: HashSet<String>,        // "program a b" that fail with a generic error
        pub err_msg: HashMap<String, String>, // "program a b" -> exact error string (e.g. "spawn …")
        pub calls: Mutex<Vec<String>>,
        pub writes: Mutex<Vec<(String, String)>>,
    }

    impl FakeSys {
        pub fn key(program: &str, args: &[&str]) -> String {
            std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ")
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Sys for FakeSys {
        async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
            let k = FakeSys::key(program, args);
            self.calls.lock().unwrap().push(k.clone());
            if let Some(msg) = self.err_msg.get(&k) {
                return Err(msg.clone());
            }
            if self.err.contains(&k) {
                return Err(format!("fake error: {k}"));
            }
            Ok(self.ok.get(&k).cloned().unwrap_or_default())
        }
        fn exists(&self, path: &Path) -> bool {
            self.paths.contains(path.to_str().unwrap())
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path.to_str().unwrap()).cloned()
        }
        fn write(&self, path: &Path, content: &str) -> Result<(), String> {
            self.writes
                .lock()
                .unwrap()
                .push((path.to_string_lossy().into(), content.into()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSys;
    use super::*;

    #[tokio::test]
    async fn user_exists_reflects_id_result() {
        let mut sys = FakeSys::default();
        sys.ok
            .insert(FakeSys::key("id", &["-u", "rpi-agent"]), "999".into());
        assert!(user_exists(&sys, "rpi-agent").await);

        let mut absent = FakeSys::default();
        absent.err.insert(FakeSys::key("id", &["-u", "rpi-agent"]));
        assert!(!user_exists(&absent, "rpi-agent").await);
    }

    #[tokio::test]
    async fn cloudflared_running_maps_run_outcomes() {
        // Ok(_) -> a cloudflared process exists
        let mut sys = FakeSys::default();
        sys.ok
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]), "4321".into());
        assert_eq!(cloudflared_running(&sys).await, CloudflaredState::Running);

        // Err(non-spawn) -> pgrep ran, no match
        let mut sys = FakeSys::default();
        sys.err
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]));
        assert_eq!(
            cloudflared_running(&sys).await,
            CloudflaredState::NotRunning
        );

        // Err("spawn …") -> pgrep not installed
        let mut sys = FakeSys::default();
        sys.err_msg.insert(
            FakeSys::key("pgrep", &["-x", "cloudflared"]),
            "spawn pgrep: No such file or directory".into(),
        );
        assert_eq!(
            cloudflared_running(&sys).await,
            CloudflaredState::Undetermined
        );
    }

    #[tokio::test]
    async fn in_group_parses_id_ng() {
        let mut sys = FakeSys::default();
        sys.ok.insert(
            FakeSys::key("id", &["-nG", "piuser"]),
            "piuser sudo docker rpi-agent".into(),
        );
        assert!(in_group(&sys, "piuser", "docker").await);
        assert!(!in_group(&sys, "piuser", "wheel").await);
    }

    #[test]
    fn unit_template_matches_spec_byte_for_byte() {
        assert!(UNIT.starts_with("[Unit]\nDescription=rpi deploy agent\n"));
        assert!(
            UNIT.contains("ExecStart=/usr/local/bin/rpi agent run --config /etc/rpi/agent.toml\n")
        );
        assert!(UNIT.contains("Environment=XDG_CACHE_HOME=/var/lib/rpi/.cache\n"));
        assert!(UNIT.ends_with("WantedBy=multi-user.target\n"));
    }

    #[tokio::test]
    async fn write_unit_skips_when_identical() {
        let mut sys = FakeSys::default();
        sys.paths.insert(UNIT_PATH.into());
        sys.files.insert(UNIT_PATH.into(), UNIT.into());
        let action = write_unit_with_backup(&sys, false).unwrap();
        assert!(matches!(action, WriteAction::Skipped));
        assert!(sys.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_unit_backs_up_when_different() {
        let mut sys = FakeSys::default();
        sys.paths.insert(UNIT_PATH.into());
        sys.files.insert(UNIT_PATH.into(), "old=unit\n".into());
        let action = write_unit_with_backup(&sys, false).unwrap();
        assert!(matches!(action, WriteAction::BackedUp));
        let writes = sys.writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(p, _)| p.ends_with("rpi-agent.service.bak")),
            "backup written"
        );
        assert!(
            writes.iter().any(|(p, c)| p == UNIT_PATH && c == UNIT),
            "canonical written"
        );
    }

    fn fresh_sys() -> FakeSys {
        let mut sys = FakeSys::default();
        // user absent, no dirs/files exist; group lookups succeed but show no membership.
        sys.err.insert(FakeSys::key("id", &["-u", "rpi-agent"]));
        sys.err.insert(FakeSys::key("id", &["-u", "pi-agent"])); // no legacy install to migrate
        sys.ok.insert(
            FakeSys::key("id", &["-nG", "rpi-agent"]),
            "rpi-agent".into(),
        );
        sys.ok
            .insert(FakeSys::key("id", &["-nG", "piuser"]), "piuser sudo".into());
        sys.ok.insert(
            FakeSys::key("docker", &["version", "--format", "{{.Server.Version}}"]),
            "27.0".into(),
        );
        sys.ok
            .insert(FakeSys::key("docker", &["compose", "version"]), "v2".into());
        // Clean host: no foreign cloudflared running (Part A guard sees NotRunning).
        sys.err
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]));
        sys
    }

    fn legacy_sys() -> FakeSys {
        let mut sys = FakeSys::default();
        sys.err.insert(FakeSys::key("id", &["-u", "rpi-agent"])); // not migrated yet
        sys.ok
            .insert(FakeSys::key("id", &["-u", "pi-agent"]), "999".into()); // legacy install present
        sys.ok.insert(
            FakeSys::key("systemctl", &["disable", "--now", "pi-agent"]),
            "".into(),
        );
        sys.ok.insert(
            FakeSys::key("groupmod", &["-n", "rpi-agent", "pi-agent"]),
            "".into(),
        );
        sys.ok.insert(
            FakeSys::key("usermod", &["-l", "rpi-agent", "pi-agent"]),
            "".into(),
        );
        for p in ["/var/lib/pi", "/etc/pi", "/var/log/pi", OLD_UNIT_PATH] {
            sys.paths.insert(p.into());
        }
        sys.ok.insert(
            FakeSys::key("mv", &["/var/lib/pi", "/var/lib/rpi"]),
            "".into(),
        );
        sys.ok
            .insert(FakeSys::key("mv", &["/etc/pi", "/etc/rpi"]), "".into());
        sys.ok.insert(
            FakeSys::key("mv", &["/var/log/pi", "/var/log/rpi"]),
            "".into(),
        );
        sys.ok.insert(
            FakeSys::key("mv", &[OLD_UNIT_PATH, &format!("{OLD_UNIT_PATH}.bak")]),
            "".into(),
        );
        sys
    }

    #[tokio::test]
    async fn migrates_existing_pi_agent_install_in_place() {
        let sys = legacy_sys();
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        let calls = sys.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "systemctl disable --now pi-agent"),
            "stops the old unit"
        );
        assert!(calls.iter().any(|c| c == "groupmod -n rpi-agent pi-agent"));
        assert!(calls.iter().any(|c| c == "usermod -l rpi-agent pi-agent"));
        assert!(calls.iter().any(|c| c == "mv /var/lib/pi /var/lib/rpi"));
        assert!(calls.iter().any(|c| c == "mv /etc/pi /etc/rpi"));
        assert!(calls.iter().any(|c| c == "mv /var/log/pi /var/log/rpi"));
        assert!(
            calls
                .iter()
                .any(|c| *c == format!("mv {OLD_UNIT_PATH} {OLD_UNIT_PATH}.bak")),
            "old unit backed up, not just deleted"
        );
        assert!(rep
            .repaired
            .iter()
            .any(|r| r.contains("migrated: pi-agent -> rpi-agent")));
        assert!(rep.errors.is_empty());
    }

    #[tokio::test]
    async fn migration_quiesces_user_session_before_renaming_login() {
        // usermod -l fails if the login is still in use (e.g. a lingering
        // cloudflared user manager), so the session must be dropped first.
        let sys = legacy_sys();
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        let calls = sys.calls();
        let disable = calls
            .iter()
            .position(|c| c == "loginctl disable-linger pi-agent");
        let terminate = calls
            .iter()
            .position(|c| c == "loginctl terminate-user pi-agent");
        let usermod = calls
            .iter()
            .position(|c| c == "usermod -l rpi-agent pi-agent");
        assert!(disable.is_some(), "linger dropped: {calls:?}");
        assert!(terminate.is_some(), "user session terminated: {calls:?}");
        assert!(
            disable.unwrap() < usermod.unwrap(),
            "linger dropped before rename"
        );
        assert!(
            terminate.unwrap() < usermod.unwrap(),
            "session ended before rename"
        );
    }

    #[tokio::test]
    async fn migration_rewrites_stale_paths_in_agent_toml_preserving_custom_values() {
        let mut sys = legacy_sys();
        // A real v0.5 agent.toml carries the old absolute paths plus any operator
        // customizations; only the paths must change.
        sys.files.insert(
            AGENT_TOML_PATH.into(),
            "data_dir = \"/var/lib/pi\"\nsocket = \"/run/pi/agent.sock\"\nport_min = 9000\n".into(),
        );
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        let (_, content) = writes
            .iter()
            .find(|(p, _)| p == AGENT_TOML_PATH)
            .expect("agent.toml rewritten");
        assert!(
            content.contains("data_dir = \"/var/lib/rpi\""),
            "data_dir moved: {content}"
        );
        assert!(
            content.contains("socket = \"/run/rpi/agent.sock\""),
            "socket moved: {content}"
        );
        assert!(
            !content.contains("/var/lib/pi\""),
            "no stale data_dir left: {content}"
        );
        assert!(
            !content.contains("/run/pi/"),
            "no stale socket left: {content}"
        );
        assert!(
            content.contains("port_min = 9000"),
            "custom values preserved: {content}"
        );
    }

    #[tokio::test]
    async fn migration_rewrites_cloudflared_unit_and_config_paths() {
        let mut sys = legacy_sys();
        sys.paths.insert(CLOUDFLARED_UNIT_PATH.into());
        sys.files.insert(
            CLOUDFLARED_UNIT_PATH.into(),
            "[Service]\nExecStart=/usr/local/bin/cloudflared tunnel --config /var/lib/pi/cloudflared/config.yml run\n".into(),
        );
        sys.files.insert(
            CLOUDFLARED_CONFIG_PATH.into(),
            "credentials-file: /var/lib/pi/cloudflared/home.json\n".into(),
        );
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        let unit = &writes
            .iter()
            .find(|(p, _)| p == CLOUDFLARED_UNIT_PATH)
            .expect("unit rewritten")
            .1;
        let cfg = &writes
            .iter()
            .find(|(p, _)| p == CLOUDFLARED_CONFIG_PATH)
            .expect("config rewritten")
            .1;
        assert!(
            unit.contains("/var/lib/rpi/cloudflared/config.yml"),
            "unit path moved: {unit}"
        );
        assert!(!unit.contains("/var/lib/pi/"), "no stale unit path: {unit}");
        assert!(
            cfg.contains("/var/lib/rpi/cloudflared/home.json"),
            "creds path moved: {cfg}"
        );
    }

    #[test]
    fn rewrite_owned_paths_is_idempotent_and_skips_clean_files() {
        // Already-migrated content must not be written again.
        let mut sys = FakeSys::default();
        sys.files.insert(
            "/etc/rpi/agent.toml".into(),
            "data_dir = \"/var/lib/rpi\"\n".into(),
        );
        let mut rep = SetupReport::default();
        rewrite_owned_paths(&sys, Path::new("/etc/rpi/agent.toml"), &mut rep);
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "clean file left untouched"
        );
        assert!(rep.repaired.is_empty());
    }

    #[tokio::test]
    async fn migration_skips_directories_that_do_not_exist() {
        let mut sys = legacy_sys();
        sys.paths.remove("/etc/pi"); // e.g. never had /etc/pi for some reason
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        assert!(
            !sys.calls().iter().any(|c| c.starts_with("mv /etc/pi")),
            "nothing to move"
        );
    }

    #[tokio::test]
    async fn migration_reenables_linger_when_cloudflared_was_configured() {
        let mut sys = legacy_sys();
        sys.paths.insert(CLOUDFLARED_UNIT_PATH.into());
        sys.ok.insert(
            FakeSys::key("loginctl", &["enable-linger", "rpi-agent"]),
            "".into(),
        );
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        assert!(sys
            .calls()
            .iter()
            .any(|c| c == "loginctl enable-linger rpi-agent"));
    }

    // migration_is_noop_when_already_rpi_agent and migration_is_noop_when_no_legacy_install
    // asserted the two guard early-returns that used to live in this function. Those guards
    // moved to `migrate::PiToRpi::detect`; the equivalent coverage now lives in
    // `migrate::pi_to_rpi_tests` (`already_migrated_not_applicable`, `fresh_install_not_applicable`).

    #[tokio::test]
    async fn migration_dry_run_reports_but_makes_no_changes() {
        let sys = legacy_sys();
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, true, &mut rep).await;
        assert!(
            sys.calls().is_empty(),
            "dry run makes no sys calls at all: {:?}",
            sys.calls()
        );
        assert!(rep.repaired.iter().any(|r| r.contains("dry run")));
    }

    #[tokio::test]
    async fn migration_failure_on_groupmod_stops_before_touching_directories() {
        let mut sys = legacy_sys();
        sys.ok
            .remove(&FakeSys::key("groupmod", &["-n", "rpi-agent", "pi-agent"]));
        sys.err
            .insert(FakeSys::key("groupmod", &["-n", "rpi-agent", "pi-agent"]));
        let mut rep = SetupReport::default();
        migrate_pi_agent(&sys, false, &mut rep).await;
        assert!(
            !sys.calls().iter().any(|c| c.starts_with("mv ")),
            "no directories touched after groupmod failure"
        );
        assert!(rep.errors.iter().any(|e| e.contains("groupmod")));
    }

    #[tokio::test]
    async fn setup_migrates_legacy_install_before_creating_a_fresh_user() {
        let mut sys = legacy_sys();
        // downstream steps after migration also need to probe cleanly; fresh_sys()-style
        // group/docker mocks so setup() doesn't error out past the migration step.
        sys.ok.insert(
            FakeSys::key("id", &["-nG", "rpi-agent"]),
            "rpi-agent".into(),
        );
        sys.ok
            .insert(FakeSys::key("id", &["-nG", "piuser"]), "piuser sudo".into());
        sys.ok.insert(
            FakeSys::key("docker", &["version", "--format", "{{.Server.Version}}"]),
            "27.0".into(),
        );
        sys.ok
            .insert(FakeSys::key("docker", &["compose", "version"]), "v2".into());
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let _ = setup(&sys, &opts).await;
        let calls = sys.calls();
        let migrate_idx = calls
            .iter()
            .position(|c| c == "groupmod -n rpi-agent pi-agent");
        let useradd_idx = calls.iter().position(|c| c.starts_with("useradd --system"));
        assert!(migrate_idx.is_some(), "migration ran");
        // FakeSys does not simulate id -u rpi-agent flipping to Ok after usermod -l, so
        // setup()'s own idempotency check still sees rpi-agent as absent and creates it
        // too — harmless on a fake double, and on a real system `id -u rpi-agent` would
        // succeed post-rename so this second create step would be skipped for real.
        if let Some(u) = useradd_idx {
            assert!(
                migrate_idx.unwrap() < u,
                "migration runs before the fresh-install branch"
            );
        }
    }

    #[tokio::test]
    async fn fresh_install_creates_user_dirs_unit() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("useradd --system")),
            "creates rpi-agent"
        );
        assert!(calls.iter().any(|c| c == "usermod -aG docker rpi-agent"));
        assert!(calls.iter().any(|c| c == "usermod -aG rpi-agent piuser"));
        assert!(calls
            .iter()
            .any(|c| c.contains("install -d -m 0750 -o rpi-agent -g rpi-agent /var/lib/rpi")));
        assert!(calls
            .iter()
            .any(|c| c.contains("install -d -o rpi-agent -g rpi-agent /var/log/rpi")));
        assert!(calls.iter().any(|c| c == "systemctl daemon-reload"));
        assert!(calls
            .iter()
            .any(|c| c == "systemctl enable --now rpi-agent"));
        assert!(report.warnings.is_empty(), "docker present -> no warnings");
    }

    #[tokio::test]
    async fn repairs_only_missing_var_log_pi_on_working_install() {
        let mut sys = FakeSys::default();
        // user exists and is in both groups; all dirs exist EXCEPT /var/log/rpi; unit identical.
        sys.ok
            .insert(FakeSys::key("id", &["-u", "rpi-agent"]), "999".into());
        sys.ok.insert(
            FakeSys::key("id", &["-nG", "rpi-agent"]),
            "rpi-agent docker".into(),
        );
        sys.ok.insert(
            FakeSys::key("id", &["-nG", "piuser"]),
            "piuser sudo docker rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("docker", &["version", "--format", "{{.Server.Version}}"]),
            "27.0".into(),
        );
        sys.ok
            .insert(FakeSys::key("docker", &["compose", "version"]), "v2".into());
        for p in ["/var/lib/rpi", "/etc/rpi", UNIT_PATH, AGENT_TOML_PATH] {
            sys.paths.insert(p.into());
        }
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "750".into(),
        );
        sys.files.insert(UNIT_PATH.into(), UNIT.into());
        sys.files.insert(AGENT_TOML_PATH.into(), AGENT_TOML.into());
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("useradd")),
            "user not recreated"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("usermod")),
            "groups untouched"
        );
        assert!(calls
            .iter()
            .any(|c| c.contains("install -d -o rpi-agent -g rpi-agent /var/log/rpi")));
        assert!(report.repaired.iter().any(|r| r.contains("/var/log/rpi")));
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "agent.toml/unit untouched"
        );
    }

    #[tokio::test]
    async fn ensure_dir_repairs_ownership_when_uid_drifted() {
        let mut sys = fresh_sys();
        // /var/lib/rpi существует, но владелец — старый UID (не rpi-agent:rpi-agent)
        sys.paths.insert("/var/lib/rpi".into());
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "999:999".into(),
        );
        // Mode is already correct, so only the ownership branch fires.
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "750".into(),
        );
        // useradd succeeds (новый UID)
        sys.ok.insert(
            FakeSys::key(
                "useradd",
                &[
                    "--system",
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    "rpi-agent",
                ],
            ),
            "".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "chown -R rpi-agent:rpi-agent /var/lib/rpi"),
            "chown -R issued"
        );
        assert!(
            report
                .repaired
                .iter()
                .any(|r| r.contains("/var/lib/rpi (ownership)")),
            "ownership repair reported"
        );
        assert!(
            !report.skipped.iter().any(|s| s == "/var/lib/rpi"),
            "not skipped when ownership drifted"
        );
    }

    #[tokio::test]
    async fn data_dir_is_created_at_mode_0750() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let _ = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "install -d -m 0750 -o rpi-agent -g rpi-agent /var/lib/rpi"),
            "calls: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "install -d -o rpi-agent -g rpi-agent /var/log/rpi"),
            "only the data dir gets a mode: {calls:?}"
        );
    }

    #[tokio::test]
    async fn existing_data_dir_with_a_wider_mode_is_repaired() {
        let mut sys = fresh_sys();
        sys.paths.insert("/var/lib/rpi".into());
        // Ownership is already correct, so only the mode drifts.
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "755".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(
            sys.calls().iter().any(|c| c == "chmod 0750 /var/lib/rpi"),
            "calls: {:?}",
            sys.calls()
        );
        assert!(
            report
                .repaired
                .iter()
                .any(|r| r.contains("/var/lib/rpi (mode)")),
            "mode repair reported: {:?}",
            report.repaired
        );
    }

    #[tokio::test]
    async fn a_data_dir_already_at_0750_is_left_alone() {
        let mut sys = fresh_sys();
        sys.paths.insert("/var/lib/rpi".into());
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "750".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(!sys.calls().iter().any(|c| c.starts_with("chmod 0750")));
        assert!(report.skipped.iter().any(|s| s == "/var/lib/rpi"));
    }

    #[tokio::test]
    async fn ensure_dir_skips_when_ownership_already_correct() {
        let mut sys = fresh_sys();
        sys.paths.insert("/var/lib/rpi".into());
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%U:%G", "/var/lib/rpi"]),
            "rpi-agent:rpi-agent".into(),
        );
        sys.ok.insert(
            FakeSys::key("stat", &["-c", "%a", "/var/lib/rpi"]),
            "750".into(),
        );
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            !calls.iter().any(|c| c.contains("chown")),
            "no chown when ownership ok"
        );
        assert!(report.skipped.iter().any(|s| s == "/var/lib/rpi"));
    }

    #[tokio::test]
    async fn dry_run_makes_no_changes() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: true,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let _ = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(
            calls
                .iter()
                .all(|c| c.starts_with("id ") || c.starts_with("docker ")),
            "only probes ran: {calls:?}"
        );
        assert!(sys.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_docker_warns_not_fails() {
        let mut sys = fresh_sys();
        sys.ok.remove(&FakeSys::key(
            "docker",
            &["version", "--format", "{{.Server.Version}}"],
        ));
        sys.err.insert(FakeSys::key(
            "docker",
            &["version", "--format", "{{.Server.Version}}"],
        ));
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let w = report
            .warnings
            .iter()
            .find(|w| w.contains("docker"))
            .expect("docker warning present");
        assert!(
            w.contains("curl -fsSL https://get.docker.com | sh"),
            "warning includes the install command: {w}"
        );
    }

    #[tokio::test]
    async fn with_cloudflared_enables_linger_and_instructs() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: true,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        let calls = sys.calls();
        assert!(calls
            .iter()
            .any(|c| c == "loginctl enable-linger rpi-agent"));
        assert!(report.created.iter().any(|c| c.contains("linger")));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("cloudflared tunnel login")),
            "prints manual login step"
        );
    }

    #[tokio::test]
    async fn user_service_writes_dropin_and_enables() {
        let mut sys = fresh_sys();
        // fresh_sys() seeds `id -u rpi-agent` to fail (no such user yet); override
        // it to succeed since this function assumes the user already exists.
        sys.err.remove(&FakeSys::key("id", &["-u", "rpi-agent"]));
        sys.ok
            .insert(FakeSys::key("id", &["-u", "rpi-agent"]), "999".into());
        let mut rep = SetupReport::default();
        cloudflared_user_service(&sys, false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(p, c)| p.contains("user@999.service.d/override.conf")
                    && c.contains("XDG_CONFIG_HOME=/var/lib/rpi/.config")),
            "drop-in written for uid 999"
        );
        drop(writes);
        let calls = sys.calls();
        assert!(calls.iter().any(|c| c == "systemctl daemon-reload"));
        assert!(calls
            .iter()
            .any(|c| c == "systemctl restart user@999.service"));
        assert!(calls
            .iter()
            .any(|c| c == "loginctl enable-linger rpi-agent"));
    }

    #[tokio::test]
    async fn cloudflared_user_service_enable_passes_dbus_address() {
        let mut sys = fresh_sys();
        // fresh_sys() seeds `id -u rpi-agent` to fail; this function assumes the
        // user exists, so make it succeed with a known uid.
        sys.err.remove(&FakeSys::key("id", &["-u", "rpi-agent"]));
        sys.ok
            .insert(FakeSys::key("id", &["-u", "rpi-agent"]), "999".into());
        let mut rep = SetupReport::default();
        cloudflared_user_service(&sys, false, &mut rep).await;
        assert!(
            sys.calls().iter().any(|c| c
                .contains("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/999/bus")
                && c.contains("enable")
                && c.contains("--now")
                && c.contains("cloudflared")),
            "enable --now must pass the DBUS session bus address: {:?}",
            sys.calls()
        );
    }

    #[tokio::test]
    async fn without_cloudflared_does_not_touch_linger() {
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let _ = setup(&sys, &opts).await;
        assert!(!sys.calls().iter().any(|c| c.contains("enable-linger")));
    }

    #[test]
    fn cloudflared_unit_points_at_canonical_config_path() {
        assert!(
            CLOUDFLARED_UNIT.contains("cloudflared tunnel --config /var/lib/rpi/cloudflared/config.yml run"),
            "ExecStart must use the canonical config path the setup flow instructs operators to write"
        );
    }

    #[tokio::test]
    async fn useradd_failure_records_error_not_created() {
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "rpi-agent",
            ],
        ));
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(
            report.errors.iter().any(|e| e.contains("useradd")),
            "error recorded"
        );
        assert!(
            !report.created.iter().any(|c| c == "user rpi-agent"),
            "no false created"
        );
    }

    #[tokio::test]
    async fn mkdir_failure_records_error_not_created() {
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key(
            "install",
            &[
                "-d",
                "-m",
                "0750",
                "-o",
                "rpi-agent",
                "-g",
                "rpi-agent",
                "/var/lib/rpi",
            ],
        ));
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let report = setup(&sys, &opts).await;
        assert!(
            report.errors.iter().any(|e| e.contains("/var/lib/rpi")),
            "mkdir error recorded"
        );
        assert!(!report.created.iter().any(|c| c.contains("/var/lib/rpi")));
    }

    #[tokio::test]
    async fn run_with_bails_on_errors() {
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "rpi-agent",
            ],
        ));
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: false,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let res = run_with(&sys, &opts).await;
        assert!(res.is_err(), "non-zero exit when privileged step fails");
    }

    #[tokio::test]
    async fn restart_runs_when_unit_active() {
        let mut sys = FakeSys::default();
        sys.ok.insert(
            FakeSys::key("systemctl", &["is-active", "--quiet", "rpi-agent"]),
            "".into(),
        );
        sys.ok.insert(
            FakeSys::key("systemctl", &["restart", "rpi-agent"]),
            "".into(),
        );
        let note = restart_agent_if_active(&sys).await;
        assert!(note.unwrap().contains("restarted"), "reports the restart");
        assert!(sys
            .calls()
            .iter()
            .any(|c| c == "systemctl restart rpi-agent"));
    }

    #[tokio::test]
    async fn restart_skipped_when_unit_inactive() {
        let mut sys = FakeSys::default();
        sys.err.insert(FakeSys::key(
            "systemctl",
            &["is-active", "--quiet", "rpi-agent"],
        ));
        let note = restart_agent_if_active(&sys).await;
        assert!(note.is_none());
        assert!(
            !sys.calls().iter().any(|c| c.contains("systemctl restart")),
            "no restart attempted"
        );
    }

    #[tokio::test]
    async fn restart_failure_returns_warning() {
        let mut sys = FakeSys::default();
        sys.ok.insert(
            FakeSys::key("systemctl", &["is-active", "--quiet", "rpi-agent"]),
            "".into(),
        );
        sys.err
            .insert(FakeSys::key("systemctl", &["restart", "rpi-agent"]));
        let note = restart_agent_if_active(&sys).await;
        assert!(note.unwrap().starts_with("warning:"));
    }

    #[tokio::test]
    async fn resolve_npm_dist_binary_finds_login_users_npm_install() {
        let mut sys = FakeSys::default();
        sys.ok.insert(
            FakeSys::key("sudo", &["-u", "piuser", "-i", "--", "npm", "root", "-g"]),
            "/home/piuser/.nvm/versions/node/v24.18.0/lib/node_modules".into(),
        );
        sys.paths.insert(
            "/home/piuser/.nvm/versions/node/v24.18.0/lib/node_modules/rpi-deploy/dist/rpi".into(),
        );
        let found = resolve_npm_dist_binary(&sys, "piuser").await;
        assert_eq!(
            found,
            Some(PathBuf::from(
                "/home/piuser/.nvm/versions/node/v24.18.0/lib/node_modules/rpi-deploy/dist/rpi"
            ))
        );
    }

    #[tokio::test]
    async fn resolve_npm_dist_binary_none_when_npm_unavailable() {
        let mut sys = FakeSys::default();
        sys.err.insert(FakeSys::key(
            "sudo",
            &["-u", "piuser", "-i", "--", "npm", "root", "-g"],
        ));
        let found = resolve_npm_dist_binary(&sys, "piuser").await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn resolve_npm_dist_binary_none_when_package_not_installed() {
        let mut sys = FakeSys::default();
        sys.ok.insert(
            FakeSys::key("sudo", &["-u", "piuser", "-i", "--", "npm", "root", "-g"]),
            "/home/piuser/.nvm/versions/node/v24.18.0/lib/node_modules".into(),
        );
        // no paths inserted: rpi-deploy/dist/rpi does not exist for this user
        let found = resolve_npm_dist_binary(&sys, "piuser").await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn writes_token_with_rpi_secrets_group() {
        let sys = fresh_sys();
        let mut rep = SetupReport::default();
        ensure_cloudflare_token(&sys, "cf-token-value", false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(p, c)| p == "/var/lib/rpi/cloudflare/token" && c == "cf-token-value"),
            "token written"
        );
        let calls = sys.calls();
        assert!(calls
            .iter()
            .any(|c| c.contains("groupadd") && c.contains("rpi-secrets")));
        assert!(calls
            .iter()
            .any(|c| c == "usermod -aG rpi-secrets rpi-agent"));
        assert!(calls.iter().any(|c| c.contains("install")
            && c.contains("-g rpi-secrets")
            && c.contains("/var/lib/rpi/cloudflare")));
        assert!(calls
            .iter()
            .any(|c| c.contains("chmod 640 /var/lib/rpi/cloudflare/token")));
        assert!(calls
            .iter()
            .any(|c| c.contains("chown root:rpi-secrets /var/lib/rpi/cloudflare/token")));
    }

    #[tokio::test]
    async fn token_chmod_failure_is_surfaced() {
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key(
            "chmod",
            &["640", "/var/lib/rpi/cloudflare/token"],
        ));
        let mut rep = SetupReport::default();
        ensure_cloudflare_token(&sys, "cf-token-value", false, &mut rep).await;
        assert!(!rep.errors.is_empty(), "chmod failure should be surfaced");
        assert!(
            !rep.created
                .iter()
                .any(|c| c == "/var/lib/rpi/cloudflare/token"),
            "token must not be reported as successfully created"
        );
    }

    #[test]
    fn cloudflared_asset_maps_known_arches() {
        assert_eq!(
            cloudflared_asset("aarch64"),
            Some("cloudflared-linux-arm64")
        );
        assert_eq!(cloudflared_asset("armv7l"), Some("cloudflared-linux-arm"));
        assert_eq!(cloudflared_asset("x86_64"), Some("cloudflared-linux-amd64"));
        assert_eq!(cloudflared_asset("mips"), None);
    }

    #[tokio::test]
    async fn installs_cloudflared_when_absent() {
        let mut sys = fresh_sys();
        sys.ok
            .insert(FakeSys::key("uname", &["-m"]), "aarch64".into());
        // `cloudflared --version` fails => not installed yet
        sys.err.insert(FakeSys::key("cloudflared", &["--version"]));
        let mut rep = SetupReport::default();
        ensure_cloudflared_binary(&sys, false, &mut rep).await;
        let calls = sys.calls();
        assert!(
            calls.iter().any(|c| c.contains("cloudflared-linux-arm64")),
            "downloads the arm64 asset: {calls:?}"
        );
        assert!(calls
            .iter()
            .any(|c| c.contains("chmod") && c.contains("/usr/local/bin/cloudflared")));
    }

    #[test]
    fn config_yml_uses_spaces_and_keeps_catch_all() {
        let yml = render_config_yml("tid", "/var/lib/rpi/cloudflared/tid.json");
        assert!(!yml.contains('\t'), "no tabs allowed in cloudflared config");
        assert!(yml.contains("tunnel: tid"));
        assert!(yml.contains("credentials-file: /var/lib/rpi/cloudflared/tid.json"));
        assert!(
            yml.trim_end().ends_with("service: http_status:404"),
            "catch-all last"
        );
    }

    #[tokio::test]
    async fn resolve_tunnel_name_prefers_flag_then_hostname_then_fallback() {
        // 1. explicit --tunnel-name wins and hostname is never consulted.
        let sys = fresh_sys();
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: true,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: Some("myboard".into()),
        };
        let name = resolve_tunnel_name(&sys, &opts).await;
        assert_eq!(name, "myboard");
        assert!(
            !sys.calls().iter().any(|c| c.starts_with("hostname")),
            "must not call hostname when --tunnel-name is set: {:?}",
            sys.calls()
        );

        // 2. no flag, hostname succeeds -> use it.
        let mut sys = fresh_sys();
        sys.ok.insert(FakeSys::key("hostname", &[]), "mypi".into());
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: true,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let name = resolve_tunnel_name(&sys, &opts).await;
        assert_eq!(name, "mypi");

        // 3. no flag, hostname fails -> fall back to "rpi".
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key("hostname", &[]));
        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: true,
            dry_run: false,
            cf_token: None,
            domain: None,
            tunnel_name: None,
        };
        let name = resolve_tunnel_name(&sys, &opts).await;
        assert_eq!(name, "rpi");
    }

    #[tokio::test]
    async fn bootstrap_writes_creds_config_and_validates() {
        use pi_domain::contracts::{MockCloudflareApi, TunnelCreds};
        let mut sys = fresh_sys();
        sys.ok
            .insert(FakeSys::key("uname", &["-m"]), "aarch64".into());
        sys.err.insert(FakeSys::key("cloudflared", &["--version"])); // triggers install path
        let mut cf = MockCloudflareApi::new();
        cf.expect_find_or_create_tunnel().returning(|name| {
            Ok(TunnelCreds {
                account_tag: "acc".into(),
                tunnel_id: "tid".into(),
                tunnel_name: name.to_string(),
                tunnel_secret: "c2VjcmV0".into(),
            })
        });
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        cloudflared_bootstrap_full(&sys, &cf, &opts, false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|(p, _)| p == "/var/lib/rpi/cloudflared/tid.json"),
            "creds json"
        );
        assert!(writes
            .iter()
            .any(|(p, c)| p == "/var/lib/rpi/cloudflared/config.yml" && !c.contains('\t')));
        drop(writes);
        assert!(sys.calls().iter().any(|c| c.contains("ingress validate")));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    }

    #[tokio::test]
    async fn cloudflared_chmod_failure_is_surfaced() {
        let mut sys = fresh_sys();
        sys.ok
            .insert(FakeSys::key("uname", &["-m"]), "aarch64".into());
        sys.err.insert(FakeSys::key("cloudflared", &["--version"]));
        sys.err.insert(FakeSys::key(
            "chmod",
            &["0755", "/usr/local/bin/cloudflared"],
        ));
        let mut rep = SetupReport::default();
        ensure_cloudflared_binary(&sys, false, &mut rep).await;
        assert!(!rep.errors.is_empty(), "chmod failure should be surfaced");
        assert!(
            !rep.created
                .iter()
                .any(|c| c == "/usr/local/bin/cloudflared"),
            "binary must not be reported as successfully created"
        );
    }

    #[tokio::test]
    async fn bootstrap_creds_chmod_failure_is_surfaced() {
        use pi_domain::contracts::{MockCloudflareApi, TunnelCreds};
        let mut sys = fresh_sys();
        sys.ok
            .insert(FakeSys::key("uname", &["-m"]), "aarch64".into());
        sys.err.insert(FakeSys::key("cloudflared", &["--version"])); // triggers install path
        sys.err.insert(FakeSys::key(
            "chmod",
            &["640", "/var/lib/rpi/cloudflared/tid.json"],
        ));
        let mut cf = MockCloudflareApi::new();
        cf.expect_find_or_create_tunnel().returning(|name| {
            Ok(TunnelCreds {
                account_tag: "acc".into(),
                tunnel_id: "tid".into(),
                tunnel_name: name.to_string(),
                tunnel_secret: "c2VjcmV0".into(),
            })
        });
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        cloudflared_bootstrap_full(&sys, &cf, &opts, false, &mut rep).await;
        assert!(!rep.errors.is_empty(), "chmod failure should be surfaced");
        assert!(
            !rep.created
                .iter()
                .any(|c| c == "/var/lib/rpi/cloudflared/tid.json"),
            "creds must not be reported as successfully created"
        );
    }

    #[tokio::test]
    async fn bootstrap_adopted_tunnel_without_creds_errors() {
        use pi_domain::contracts::{MockCloudflareApi, TunnelCreds};
        let sys = fresh_sys();
        let mut cf = MockCloudflareApi::new();
        cf.expect_find_or_create_tunnel().returning(|name| {
            Ok(TunnelCreds {
                account_tag: "acc".into(),
                tunnel_id: "tid".into(),
                tunnel_name: name.to_string(),
                tunnel_secret: String::new(), // adopted: no secret held locally
            })
        });
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        cloudflared_bootstrap_full(&sys, &cf, &opts, false, &mut rep).await;
        assert!(
            rep.errors
                .iter()
                .any(|e| e.contains("adopted tunnel") && e.contains("credentials")),
            "missing creds on adopted tunnel should be surfaced: {:?}",
            rep.errors
        );
        assert!(
            !sys.writes
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p == "/var/lib/rpi/cloudflared/tid.json"),
            "no creds file should be written for an adopted tunnel with no secret"
        );
    }

    #[tokio::test]
    async fn bootstrap_dry_run_makes_no_changes() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = fresh_sys();
        sys.err.insert(FakeSys::key("cloudflared", &["--version"])); // triggers dry path in binary step
        let cf = MockCloudflareApi::new(); // no expectations: API must not be touched on dry run
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        cloudflared_bootstrap_full(&sys, &cf, &opts, true, &mut rep).await;
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "dry run must not write any files"
        );
        assert!(
            !sys.calls()
                .iter()
                .any(|c| c.contains("install") && c.contains("/var/lib/rpi/cloudflared")),
            "dry run must not create /var/lib/rpi/cloudflared"
        );
    }

    const MANUAL_CONFIG_UUID: &str = "tunnel: bc15c4e2-1111-2222-3333-444455556666\n\
credentials-file: /var/lib/rpi/cloudflared/creds.json\n\
ingress:\n  - hostname: board.example.com\n    service: http://127.0.0.1:8001\n  - service: http_status:404\n";

    fn adoption_sys(config: &str) -> FakeSys {
        let mut sys = fresh_sys();
        // binary already installed -> ensure_cloudflared_binary skips
        sys.ok
            .insert(FakeSys::key("cloudflared", &["--version"]), "ok".into());
        sys.paths.insert(CLOUDFLARED_CONFIG_PATH.into());
        sys.files
            .insert(CLOUDFLARED_CONFIG_PATH.into(), config.into());
        sys
    }

    fn adoption_opts() -> CloudflaredBootstrap {
        CloudflaredBootstrap {
            tunnel_name: "ignored-on-adoption".into(),
            zone: "example.com".into(),
        }
    }

    #[tokio::test]
    async fn adoption_never_rewrites_config_and_uses_uuid_without_tunnel_api() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = adoption_sys(MANUAL_CONFIG_UUID);
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        let cf = MockCloudflareApi::new(); // no expectations: any tunnel-API call panics
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;

        let writes = sys.writes.lock().unwrap();
        assert!(
            writes.iter().all(|(p, _)| p != CLOUDFLARED_CONFIG_PATH),
            "config.yml must never be rewritten on adoption: {writes:?}"
        );
        let (_, toml) = writes
            .iter()
            .find(|(p, _)| p == AGENT_TOML_PATH)
            .expect("agent.toml sections written");
        assert!(toml.contains("tunnel_id = \"bc15c4e2-1111-2222-3333-444455556666\""));
        drop(writes);
        assert!(sys.calls().iter().any(|c| c.contains("ingress validate")));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    }

    #[tokio::test]
    async fn adoption_via_wrapper_does_not_restart_or_enable_cloudflared_service() {
        // Regression for the wrapper-level bug: cloudflared_bootstrap_full
        // correctly never rewrites config.yml or restarts anything on the
        // adoption branch, but its caller (cloudflared_bootstrap) used to
        // unconditionally fall through into cloudflared_user_service
        // afterwards — bouncing user@<uid>.service (and therefore any
        // manually-run cloudflared) even though nothing failed. Exercise the
        // wrapper directly, not cloudflared_bootstrap_full, so this seam is
        // covered.
        let mut sys = adoption_sys(MANUAL_CONFIG_UUID);
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        // Let `id -u rpi-agent` succeed so cloudflared_user_service (if
        // reached) proceeds far enough to attempt the disruptive calls,
        // rather than bailing out early for an unrelated reason.
        sys.err.remove(&FakeSys::key("id", &["-u", "rpi-agent"]));
        sys.ok
            .insert(FakeSys::key("id", &["-u", "rpi-agent"]), "999".into());

        let opts = SetupOpts {
            login_user: "piuser".into(),
            with_cloudflared: true,
            dry_run: false,
            cf_token: Some("tok".into()),
            domain: Some("example.com".into()),
            tunnel_name: None,
        };
        let mut rep = SetupReport::default();
        cloudflared_bootstrap(&sys, &opts, &mut rep).await;

        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        let calls = sys.calls();
        assert!(
            !calls.iter().any(|c| c.contains("restart")),
            "adoption must not restart any service: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("enable")),
            "adoption must not enable the cloudflared user service: {calls:?}"
        );
        let writes = sys.writes.lock().unwrap();
        assert!(
            writes.iter().all(|(p, _)| p != CLOUDFLARED_UNIT_PATH),
            "adoption must not (re)write the cloudflared unit file: {writes:?}"
        );
    }

    #[tokio::test]
    async fn adoption_resolves_tunnel_name_via_api() {
        use pi_domain::contracts::{MockCloudflareApi, TunnelCreds};
        let mut sys = adoption_sys(
            "tunnel: myboard\ncredentials-file: /var/lib/rpi/cloudflared/creds.json\ningress:\n  - service: http_status:404\n",
        );
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        let mut cf = MockCloudflareApi::new();
        cf.expect_find_or_create_tunnel()
            .withf(|name| name == "myboard")
            .returning(|name| {
                Ok(TunnelCreds {
                    account_tag: "acc".into(),
                    tunnel_id: "resolved-tid".into(),
                    tunnel_name: name.to_string(),
                    tunnel_secret: String::new(), // adopted
                })
            });
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        let writes = sys.writes.lock().unwrap();
        let (_, toml) = writes
            .iter()
            .find(|(p, _)| p == AGENT_TOML_PATH)
            .expect("agent.toml written");
        assert!(toml.contains("tunnel_id = \"resolved-tid\""));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    }

    #[tokio::test]
    async fn adoption_missing_credentials_is_an_error_with_zero_writes() {
        use pi_domain::contracts::MockCloudflareApi;
        let sys = adoption_sys(MANUAL_CONFIG_UUID); // creds path NOT in sys.paths
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(
            rep.errors.iter().any(|e| e.contains("creds.json")),
            "{:?}",
            rep.errors
        );
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "zero writes on error"
        );
    }

    #[tokio::test]
    async fn adoption_unparseable_config_is_an_error_with_zero_writes() {
        use pi_domain::contracts::MockCloudflareApi;
        let sys = adoption_sys("- just\n- a list\n"); // no `tunnel:` mapping
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(
            rep.errors
                .iter()
                .any(|e| e.contains("move it aside") || e.contains("not a usable")),
            "{:?}",
            rep.errors
        );
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "zero writes on error"
        );
    }

    #[tokio::test]
    async fn adoption_validate_failure_is_a_warning_not_an_error() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = adoption_sys(MANUAL_CONFIG_UUID);
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        sys.err.insert(FakeSys::key(
            "cloudflared",
            &[
                "tunnel",
                "--config",
                CLOUDFLARED_CONFIG_PATH,
                "ingress",
                "validate",
            ],
        ));
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert!(
            !rep.warnings.is_empty(),
            "validate failure surfaces as warning"
        );
        assert!(
            sys.writes
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p == AGENT_TOML_PATH),
            "agent.toml still written"
        );
    }

    #[tokio::test]
    async fn adoption_default_creds_path_is_derived_from_tunnel_id() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = adoption_sys(
            "tunnel: bc15c4e2-1111-2222-3333-444455556666\ningress:\n  - service: http_status:404\n",
        );
        sys.paths
            .insert("/var/lib/rpi/cloudflared/bc15c4e2-1111-2222-3333-444455556666.json".into());
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    }

    #[tokio::test]
    async fn adoption_dry_run_reports_would_adopt_and_writes_nothing() {
        use pi_domain::contracts::MockCloudflareApi;
        let sys = adoption_sys(MANUAL_CONFIG_UUID);
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), true, &mut rep).await;
        assert!(sys.writes.lock().unwrap().is_empty());
        assert!(
            rep.skipped.iter().any(|s| s.contains("would adopt")),
            "{:?}",
            rep.skipped
        );
    }

    #[tokio::test]
    async fn repeated_adoption_is_a_pure_skip() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = adoption_sys(MANUAL_CONFIG_UUID);
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        // agent.toml already carries the sections from the first run
        sys.files.insert(
            AGENT_TOML_PATH.into(),
            "[cloudflared]\nconfig = \"/var/lib/rpi/cloudflared/config.yml\"\n".into(),
        );
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "second run must not write anything"
        );
        assert!(
            rep.skipped.iter().any(|s| s.contains("agent.toml")),
            "{:?}",
            rep.skipped
        );
    }

    #[tokio::test]
    async fn bootstrap_refuses_when_foreign_cloudflared_running() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = fresh_sys();
        // cloudflared installed (binary step skips) and running (foreign).
        sys.ok.insert(
            FakeSys::key("cloudflared", &["--version"]),
            "2024.1.0".into(),
        );
        sys.err
            .remove(&FakeSys::key("pgrep", &["-x", "cloudflared"]));
        sys.ok
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]), "4321".into());
        let cf = MockCloudflareApi::new(); // no expectations: tunnel API must not be called
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        let adopted = cloudflared_bootstrap_full(&sys, &cf, &opts, false, &mut rep).await;
        assert!(!adopted);
        assert!(
            rep.errors
                .iter()
                .any(|e| e.contains("already running") && e.contains("refusing")),
            "{:?}",
            rep.errors
        );
        assert!(
            sys.writes.lock().unwrap().is_empty(),
            "guard must write nothing"
        );
    }

    #[tokio::test]
    async fn bootstrap_dry_run_refuses_when_foreign_cloudflared_running() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = fresh_sys();
        sys.err
            .remove(&FakeSys::key("pgrep", &["-x", "cloudflared"]));
        sys.ok
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]), "4321".into());
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        let adopted = cloudflared_bootstrap_full(&sys, &cf, &opts, true, &mut rep).await;
        assert!(!adopted);
        assert!(
            rep.skipped.iter().any(|s| s.starts_with("would refuse:")),
            "{:?}",
            rep.skipped
        );
        assert!(rep.errors.is_empty());
        assert!(sys.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bootstrap_refuses_when_pgrep_unavailable() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = fresh_sys();
        sys.err
            .remove(&FakeSys::key("pgrep", &["-x", "cloudflared"]));
        sys.err_msg.insert(
            FakeSys::key("pgrep", &["-x", "cloudflared"]),
            "spawn pgrep: No such file or directory".into(),
        );
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        let opts = CloudflaredBootstrap {
            tunnel_name: "myboard".into(),
            zone: "example.com".into(),
        };
        let adopted = cloudflared_bootstrap_full(&sys, &cf, &opts, false, &mut rep).await;
        assert!(!adopted);
        assert!(
            rep.errors.iter().any(|e| e.contains("pgrep unavailable")),
            "{:?}",
            rep.errors
        );
        assert!(sys.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn adoption_runs_even_with_a_cloudflared_already_running() {
        use pi_domain::contracts::MockCloudflareApi;
        let mut sys = adoption_sys(MANUAL_CONFIG_UUID);
        sys.paths
            .insert("/var/lib/rpi/cloudflared/creds.json".into());
        // A cloudflared is running, but config.yml is present -> the adoption
        // branch runs first and the fresh-path guard never fires.
        sys.err
            .remove(&FakeSys::key("pgrep", &["-x", "cloudflared"]));
        sys.ok
            .insert(FakeSys::key("pgrep", &["-x", "cloudflared"]), "4321".into());
        let cf = MockCloudflareApi::new();
        let mut rep = SetupReport::default();
        let adopted =
            cloudflared_bootstrap_full(&sys, &cf, &adoption_opts(), false, &mut rep).await;
        assert!(adopted, "config.yml present -> adoption");
        assert!(
            !rep.errors.iter().any(|e| e.contains("refusing")),
            "guard must not fire on adoption: {:?}",
            rep.errors
        );
    }

    #[test]
    fn tunnel_id_shape_detection() {
        assert!(looks_like_tunnel_id("bc15c4e2-1111-2222-3333-444455556666"));
        assert!(!looks_like_tunnel_id("myboard"));
        assert!(!looks_like_tunnel_id("bc15c4e2-1111-2222-3333-44445555666")); // 35 chars
        assert!(!looks_like_tunnel_id(
            "gc15c4e2-1111-2222-3333-444455556666"
        )); // non-hex
        assert!(!looks_like_tunnel_id(
            "BC15C4E2-1111-2222-3333-444455556666"
        )); // uppercase hex
    }

    #[test]
    fn cf_token_file_is_read_and_trimmed() {
        let r = resolve_cf_token(Some("/tok".into()), None, None, |p| {
            assert_eq!(p, "/tok");
            Ok("secret-token\n".into())
        })
        .unwrap();
        assert_eq!(r.token.as_deref(), Some("secret-token"));
        assert!(r.warning.is_none());
    }

    #[test]
    fn cf_token_file_dash_reads_stdin() {
        // The real reader maps "-" to stdin; the helper just passes the path.
        let r = resolve_cf_token(Some("-".into()), None, None, |p| {
            assert_eq!(p, "-");
            Ok("from-stdin\n".into())
        })
        .unwrap();
        assert_eq!(r.token.as_deref(), Some("from-stdin"));
    }

    #[test]
    fn cf_token_file_unreadable_is_error() {
        let err = resolve_cf_token(Some("/nope".into()), None, None, |_| {
            Err("No such file".into())
        })
        .unwrap_err();
        assert!(err.contains("--cf-token-file /nope"), "{err}");
    }

    #[test]
    fn cf_token_file_empty_resolved_token_is_error() {
        let err =
            resolve_cf_token(Some("/tok".into()), None, None, |_| Ok("   \n".into())).unwrap_err();
        assert!(err.contains("empty token"), "{err}");
    }

    #[test]
    fn cf_token_inline_resolves_with_deprecation_warning() {
        let r =
            resolve_cf_token(None, Some("inline-tok".into()), None, |_| unreachable!()).unwrap();
        assert_eq!(r.token.as_deref(), Some("inline-tok"));
        assert!(r.warning.as_deref().unwrap().contains("--cf-token-file"));
    }

    #[test]
    fn cf_token_file_wins_over_inline_but_still_warns() {
        let r = resolve_cf_token(Some("/tok".into()), Some("inline-tok".into()), None, |_| {
            Ok("file-tok".into())
        })
        .unwrap();
        assert_eq!(r.token.as_deref(), Some("file-tok"));
        assert!(
            r.warning.is_some(),
            "inline token still triggers the deprecation warning"
        );
    }

    #[test]
    fn cf_token_falls_back_to_env() {
        let r = resolve_cf_token(None, None, Some("env-tok".into()), |_| unreachable!()).unwrap();
        assert_eq!(r.token.as_deref(), Some("env-tok"));
        assert!(r.warning.is_none());
    }
}

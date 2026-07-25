mod agent;
mod cli;
mod compat;
mod duration;
mod output;
mod proto;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rpi",
    about = "deploy tool for Raspberry Pi (CLI + agent)",
    disable_version_flag = true
)]
struct Cli {
    /// Print version (with the brand banner on a terminal)
    #[arg(short = 'V', long = "version")]
    version: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Args)]
struct InitArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    compose: Option<String>,
    #[arg(long)]
    service: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    expose: Option<String>,
    #[arg(long = "env")]
    env_file: Option<String>,
    #[arg(long)]
    yes: bool,
}

#[derive(clap::Args)]
struct SetupArgs {
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    key: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    default: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Deploy current project (reads ./rpi.toml)
    Deploy {
        /// Branch or commit-sha (default — branch from rpi.toml)
        #[arg(long = "ref", conflicts_with = "cancel")]
        git_ref: Option<String>,
        /// Cancel the active deploy(s) of the current project instead
        #[arg(long)]
        cancel: bool,
        /// Skip deploy-key auto-registration via GitHub CLI; show the key
        /// for manual setup instead
        #[arg(long, conflicts_with = "cancel")]
        no_gh_key: bool,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// List projects on the agent
    #[command(alias = "ps")]
    Ls {
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Prune docker images and build cache on the agent (§8.1)
    Gc {
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Stream container logs of a project
    Logs {
        project: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        tail: usize,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Run a command declared in [commands] of rpi.toml inside the project container
    Command {
        /// Command name; omit to list commands deployed on the agent
        name: Option<String>,
        /// Extra arguments appended to the declared command (write them after --)
        #[arg(last = true)]
        args: Vec<String>,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Live CPU/memory/disk/temperature metrics (add -w for a live graph)
    Stats {
        project: Option<String>,
        #[arg(long)]
        json: bool,
        /// Full-screen live-updating view; quit with q/Esc/Ctrl-C
        #[arg(short = 'w', long)]
        watch: bool,
        /// Refresh interval in seconds for --watch
        #[arg(long, default_value_t = 2)]
        interval: u64,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Start project containers (no rebuild)
    Start {
        project: String,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Stop project containers
    Stop {
        project: String,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Restart project containers
    Restart {
        project: String,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Remove a project
    Rm {
        project: String,
        #[arg(long)]
        volumes: bool,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Agent and host overview
    Status {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Environment self-diagnosis
    Doctor {
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Generate rpi.toml in the current project (wizard; flags for CI)
    Init(InitArgs),
    /// Configure a server profile on this machine (wizard; flags for CI)
    Setup(SetupArgs),
    /// Update the agent on the board to a chosen version (SSH + sudo)
    Upgrade {
        /// Target version (default: this CLI's version; `latest` = newest release)
        #[arg(long)]
        version: Option<String>,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Manage project secrets (env vars + secret files from [secrets] in rpi.toml)
    Secrets {
        #[command(subcommand)]
        cmd: SecretsCmd,
    },
    /// Agent management
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Show the resolved configuration (base + overlay), locally.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage environments (overlays of rpi.toml)
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
}

#[derive(Subcommand)]
enum EnvCmd {
    /// List environments registered on the agent
    Ls {
        /// All environments on the agent, not only this project's
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Destroy an environment: stack, volumes, ingress, DNS, secrets, registry
    Destroy {
        env: String,
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Remove the environment's volumes and re-run on_create on next deploy
    ResetData {
        env: String,
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the resolved rpi.toml (with --env: base + overlay merged).
    Show {
        #[arg(long)]
        env: Option<String>,
        #[arg(long = "vars")]
        vars: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Send the env file and [secrets].files to the agent (encrypted at rest)
    Send {
        /// Also apply the new secrets to running containers
        #[arg(long)]
        apply: bool,
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// List stored env keys and file paths (values are never transmitted)
    Ls {
        /// Deploy/operate an environment defined by rpi.<env>.toml
        #[arg(long)]
        env: Option<String>,
        /// Overlay variables, e.g. --vars BRANCH_NAME=feature/login (repeatable)
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Start the agent (foreground; under systemd)
    Run {
        /// Path to agent.toml (default: /etc/rpi/agent.toml)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Agent overview; falls back to systemctl over ssh
    Status {
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Agent logs; falls back to journalctl over ssh
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 100)]
        tail: usize,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Bootstrap the agent on this Pi (run with sudo; idempotent)
    Setup {
        /// SSH login user to add to the rpi-agent group (default: $SUDO_USER)
        #[arg(long)]
        user: Option<String>,
        /// Also bootstrap cloudflared (linger + user unit)
        #[arg(long)]
        with_cloudflared: bool,
        /// DEPRECATED (leaks via ps/shell-history/journald): Cloudflare API token inline; prefer --cf-token-file or CLOUDFLARE_API_TOKEN
        #[arg(long)]
        cf_token: Option<String>,
        /// Read the Cloudflare API token from a file (path, or `-` for stdin); preferred over --cf-token
        #[arg(long)]
        cf_token_file: Option<String>,
        /// Base zone, e.g. example.com
        #[arg(long)]
        domain: Option<String>,
        /// Tunnel name (default: derived)
        #[arg(long)]
        tunnel: Option<String>,
        /// Print the plan without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Update this board's agent binary (run with sudo; downloads, verifies, swaps, restarts)
    Update {
        /// SSH login user for npm-channel detection (default: $SUDO_USER)
        #[arg(long)]
        user: Option<String>,
        /// Target version (default: latest published release)
        #[arg(long)]
        version: Option<String>,
        /// Resolve + report without downloading or swapping
        #[arg(long)]
        dry_run: bool,
    },
    /// Run host migrations uniformly (idempotent; detect-oriented)
    Migrate {
        /// List all migrations and their status
        #[arg(long)]
        list: bool,
        /// Show the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Apply a specific migration by id (repeatable; needed for disruptive ones)
        #[arg(long)]
        run: Vec<String>,
        /// Apply every applicable migration (with --yes for disruptive ones)
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Remove the agent (keeps data unless --purge)
    Uninstall {
        /// Also delete /var/lib/rpi, /etc/rpi, /var/log/rpi (irreversible)
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    output::init_colors();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            output::error(&err);
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.version {
        let v = env!("CARGO_PKG_VERSION");
        if output::stdout_is_tty() {
            println!("{}", output::brand_banner(v));
        } else {
            println!("rpi {v}");
        }
        return Ok(());
    }

    let cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            if output::stderr_is_tty() {
                eprintln!("{}", output::brand_banner(env!("CARGO_PKG_VERSION")));
            }
            eprintln!("run `rpi --help` to see available commands");
            return Ok(());
        }
    };

    if matches!(
        cmd,
        Cmd::Agent {
            cmd: AgentCmd::Run { .. }
        }
    ) {
        return match cmd {
            Cmd::Agent {
                cmd: AgentCmd::Run { config },
            } => agent::run::run(config).await,
            _ => unreachable!(),
        };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match cmd {
        Cmd::Deploy {
            git_ref,
            cancel,
            no_gh_key,
            env,
            vars,
            connect,
        } => {
            if cancel {
                cli::commands::deploy_cancel(env, vars, connect).await
            } else {
                cli::commands::deploy(git_ref, no_gh_key, env, vars, connect).await
            }
        }
        Cmd::Ls { connect } => cli::commands::ls(connect).await,
        Cmd::Gc { connect } => cli::commands::gc(connect).await,
        Cmd::Logs {
            project,
            follow,
            tail,
            connect,
        } => cli::commands::logs(project, follow, tail, connect).await,
        Cmd::Command {
            name,
            args,
            env,
            vars,
            connect,
        } => cli::commands::command(name, args, env, vars, connect).await,
        Cmd::Stats {
            project,
            json,
            watch,
            interval,
            connect,
        } => cli::commands::stats(project, json, watch, interval, connect).await,
        Cmd::Start { project, connect } => {
            cli::commands::lifecycle(project, "start", connect).await
        }
        Cmd::Stop { project, connect } => cli::commands::lifecycle(project, "stop", connect).await,
        Cmd::Restart { project, connect } => {
            cli::commands::lifecycle(project, "restart", connect).await
        }
        Cmd::Rm {
            project,
            volumes,
            yes,
            connect,
        } => cli::commands::rm(project, volumes, yes, connect).await,
        Cmd::Status { json, connect } => cli::commands::status(json, connect).await,
        Cmd::Doctor { connect } => cli::commands::doctor(connect).await,
        Cmd::Init(a) => {
            cli::init::run(cli::init::InitFlags {
                name: a.name,
                repo: a.repo,
                branch: a.branch,
                compose: a.compose,
                service: a.service,
                port: a.port,
                hostname: a.hostname,
                expose: a.expose,
                env_file: a.env_file,
                yes: a.yes,
            })
            .await
        }
        Cmd::Setup(a) => {
            cli::setup::run(cli::setup::SetupFlags {
                host: a.host,
                user: a.user,
                key: a.key,
                name: a.name,
                default: a.default,
                yes: a.yes,
            })
            .await
        }
        Cmd::Upgrade {
            version,
            yes,
            connect,
        } => cli::upgrade::run(version, yes, connect).await,
        Cmd::Secrets {
            cmd:
                SecretsCmd::Send {
                    apply,
                    env,
                    vars,
                    connect,
                },
        } => cli::commands::secrets_send(apply, env, vars, connect).await,
        Cmd::Secrets {
            cmd: SecretsCmd::Ls { env, vars, connect },
        } => cli::commands::secrets_ls(env, vars, connect).await,
        Cmd::Agent {
            cmd: AgentCmd::Run { .. },
        } => unreachable!(),
        Cmd::Agent {
            cmd: AgentCmd::Status { connect },
        } => cli::commands::agent_status(connect).await,
        Cmd::Agent {
            cmd:
                AgentCmd::Logs {
                    follow,
                    since,
                    tail,
                    connect,
                },
        } => cli::commands::agent_logs(follow, since, tail, connect).await,
        Cmd::Agent {
            cmd:
                AgentCmd::Setup {
                    user,
                    with_cloudflared,
                    cf_token,
                    cf_token_file,
                    domain,
                    tunnel,
                    dry_run,
                },
        } => {
            agent::setup::run_cmd(
                user,
                with_cloudflared,
                cf_token,
                cf_token_file,
                domain,
                tunnel,
                dry_run,
            )
            .await
        }
        Cmd::Agent {
            cmd:
                AgentCmd::Update {
                    user,
                    version,
                    dry_run,
                },
        } => agent::update::run_cmd(user, version, dry_run).await,
        Cmd::Agent {
            cmd:
                AgentCmd::Migrate {
                    list,
                    dry_run,
                    run,
                    all,
                    yes,
                },
        } => agent::migrate::run_cmd(list, dry_run, run, all, yes).await,
        Cmd::Agent {
            cmd: AgentCmd::Uninstall { purge, yes },
        } => agent::uninstall::run_cmd(purge, yes).await,
        Cmd::Config {
            cmd: ConfigCmd::Show { env, vars },
        } => cli::commands::config_show(env, vars).await,
        Cmd::Env {
            cmd: EnvCmd::Ls { all, connect },
        } => cli::envcmds::env_ls(all, connect).await,
        Cmd::Env {
            cmd:
                EnvCmd::Destroy {
                    env,
                    vars,
                    yes,
                    connect,
                },
        } => cli::envcmds::env_destroy(env, vars, yes, connect).await,
        Cmd::Env {
            cmd:
                EnvCmd::ResetData {
                    env,
                    vars,
                    yes,
                    connect,
                },
        } => cli::envcmds::env_reset_data(env, vars, yes, connect).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn deploy_host_requires_user() {
        assert!(Cli::try_parse_from(["pi", "deploy", "--host", "203.0.113.7"]).is_err());
    }

    #[test]
    fn deploy_ci_flags_parse() {
        let cli = Cli::try_parse_from([
            "pi",
            "deploy",
            "--host",
            "203.0.113.7",
            "--user",
            "pi",
            "--key",
            "./k",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Deploy { connect, .. } => {
                assert_eq!(connect.host.as_deref(), Some("203.0.113.7"));
                assert_eq!(connect.user.as_deref(), Some("pi"));
                assert_eq!(connect.key.as_deref(), Some("./k"));
            }
            _ => panic!("expected deploy"),
        }
    }

    #[test]
    fn deploy_no_gh_key_flag_parses() {
        let cli = Cli::try_parse_from(["pi", "deploy", "--no-gh-key"]).unwrap();
        match cli.cmd {
            Some(Cmd::Deploy { no_gh_key, .. }) => assert!(no_gh_key),
            _ => panic!("expected deploy"),
        }
        assert!(
            Cli::try_parse_from(["pi", "deploy", "--cancel", "--no-gh-key"]).is_err(),
            "--no-gh-key conflicts with --cancel"
        );
    }

    #[test]
    fn server_flag_conflicts_with_host() {
        assert!(Cli::try_parse_from([
            "pi",
            "deploy",
            "--server",
            "home",
            "--host",
            "203.0.113.7",
            "--user",
            "pi",
        ])
        .is_err());
    }

    #[test]
    fn agent_logs_flags_parse() {
        let cli = Cli::try_parse_from(["pi", "agent", "logs", "-f", "--since", "2h"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd:
                    AgentCmd::Logs {
                        follow,
                        since,
                        tail,
                        ..
                    },
            } => {
                assert!(follow);
                assert_eq!(since.as_deref(), Some("2h"));
                assert_eq!(tail, 100);
            }
            _ => panic!("expected agent logs"),
        }
    }

    #[test]
    fn init_flags_parse() {
        let cli = Cli::try_parse_from([
            "pi", "init", "--name", "rateme", "--port", "3000", "--expose", "lan", "--yes",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Init(args) => {
                assert_eq!(args.name.as_deref(), Some("rateme"));
                assert_eq!(args.port, Some(3000));
                assert_eq!(args.expose.as_deref(), Some("lan"));
                assert!(args.yes);
            }
            _ => panic!("expected init"),
        }
    }

    #[test]
    fn setup_flags_parse() {
        let cli = Cli::try_parse_from([
            "pi",
            "setup",
            "--host",
            "pihost.local",
            "--user",
            "piuser",
            "--key",
            "~/.ssh/pi",
            "--yes",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Setup(a) => {
                assert_eq!(a.host.as_deref(), Some("pihost.local"));
                assert_eq!(a.user.as_deref(), Some("piuser"));
                assert!(a.yes);
            }
            _ => panic!("expected setup"),
        }
    }

    #[test]
    fn agent_setup_flags_parse() {
        let cli =
            Cli::try_parse_from(["pi", "agent", "setup", "--user", "piuser", "--dry-run"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd:
                    AgentCmd::Setup {
                        user,
                        with_cloudflared,
                        dry_run,
                        ..
                    },
            } => {
                assert_eq!(user.as_deref(), Some("piuser"));
                assert!(!with_cloudflared);
                assert!(dry_run);
            }
            _ => panic!("expected agent setup"),
        }
    }

    #[test]
    fn parses_agent_migrate() {
        let cli =
            Cli::try_parse_from(["rpi", "agent", "migrate", "--run", "nginx-to-caddy"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd: AgentCmd::Migrate { run, .. },
            } => {
                assert_eq!(run, vec!["nginx-to-caddy".to_string()]);
            }
            _ => panic!("expected agent migrate"),
        }
    }

    #[test]
    fn parses_agent_setup_cloudflare_flags() {
        let cli = Cli::try_parse_from([
            "rpi",
            "agent",
            "setup",
            "--user",
            "piuser",
            "--with-cloudflared",
            "--cf-token",
            "t",
            "--domain",
            "example.com",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd:
                    AgentCmd::Setup {
                        with_cloudflared,
                        cf_token,
                        domain,
                        ..
                    },
            } => {
                assert!(with_cloudflared);
                assert_eq!(cf_token.as_deref(), Some("t"));
                assert_eq!(domain.as_deref(), Some("example.com"));
            }
            _ => panic!("expected agent setup"),
        }
    }

    #[test]
    fn parses_agent_setup_cf_token_file() {
        let cli = Cli::try_parse_from([
            "rpi",
            "agent",
            "setup",
            "--with-cloudflared",
            "--cf-token-file",
            "/run/secrets/cf",
            "--domain",
            "example.com",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd: AgentCmd::Setup { cf_token_file, .. },
            } => {
                assert_eq!(cf_token_file.as_deref(), Some("/run/secrets/cf"));
            }
            _ => panic!("expected agent setup"),
        }
    }

    #[test]
    fn agent_uninstall_flags_parse() {
        let cli = Cli::try_parse_from(["pi", "agent", "uninstall", "--purge", "--yes"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd: AgentCmd::Uninstall { purge, yes },
            } => {
                assert!(purge);
                assert!(yes);
            }
            _ => panic!("expected agent uninstall"),
        }
    }

    #[test]
    fn secrets_commands_parse_and_env_is_gone() {
        let cli = Cli::try_parse_from(["pi", "secrets", "send", "--apply"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Secrets {
                cmd: SecretsCmd::Send { apply, .. },
            } => assert!(apply),
            _ => panic!("expected secrets send"),
        }
        assert!(Cli::try_parse_from(["pi", "secrets", "ls"]).is_ok());
        assert!(
            Cli::try_parse_from(["pi", "env", "send"]).is_err(),
            "env is removed"
        );
    }

    #[test]
    fn command_parses_name_and_trailing_args() {
        let cli =
            Cli::try_parse_from(["pi", "command", "create-invite", "--", "--email", "x@y.com"])
                .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Command { name, args, .. } => {
                assert_eq!(name.as_deref(), Some("create-invite"));
                assert_eq!(args, vec!["--email".to_string(), "x@y.com".into()]);
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn command_rejects_the_removed_full_flag() {
        assert!(
            Cli::try_parse_from(["pi", "command", "migrate", "--full"]).is_err(),
            "--full is removed: command output always streams in full"
        );
    }

    #[test]
    fn bare_command_means_list_mode() {
        let cli = Cli::try_parse_from(["pi", "command"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Command { name, args, .. } => {
                assert_eq!(name, None);
                assert!(args.is_empty());
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn bare_rpi_parses_with_no_subcommand() {
        let cli = Cli::try_parse_from(["rpi"]).unwrap();
        assert!(cli.cmd.is_none());
        assert!(!cli.version);
    }

    #[test]
    fn version_flag_parses() {
        let cli = Cli::try_parse_from(["rpi", "--version"]).unwrap();
        assert!(cli.version);
        let short = Cli::try_parse_from(["rpi", "-V"]).unwrap();
        assert!(short.version);
    }

    #[test]
    fn stats_watch_with_interval() {
        let cli = Cli::try_parse_from(["rpi", "stats", "-w", "--interval", "5"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Stats {
                watch,
                interval,
                json,
                project,
                ..
            } => {
                assert!(watch);
                assert_eq!(interval, 5);
                assert!(!json);
                assert_eq!(project, None);
            }
            _ => panic!("expected Stats"),
        }
    }

    #[test]
    fn upgrade_flags_parse() {
        let cli = Cli::try_parse_from([
            "rpi",
            "upgrade",
            "--version",
            "0.22.0",
            "--yes",
            "--server",
            "home",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Upgrade {
                version,
                yes,
                connect,
            } => {
                assert_eq!(version.as_deref(), Some("0.22.0"));
                assert!(yes);
                assert_eq!(connect.server.as_deref(), Some("home"));
            }
            _ => panic!("expected upgrade"),
        }
    }

    #[test]
    fn upgrade_bare_parses_with_defaults() {
        let cli = Cli::try_parse_from(["rpi", "upgrade"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Upgrade { version, yes, .. } => {
                assert_eq!(version, None);
                assert!(!yes);
            }
            _ => panic!("expected upgrade"),
        }
    }

    #[test]
    fn agent_update_flags_parse() {
        let cli = Cli::try_parse_from([
            "rpi",
            "agent",
            "update",
            "--version",
            "0.22.0",
            "--user",
            "deploy",
            "--dry-run",
        ])
        .unwrap();
        match cli.cmd.unwrap() {
            Cmd::Agent {
                cmd:
                    AgentCmd::Update {
                        user,
                        version,
                        dry_run,
                    },
            } => {
                assert_eq!(user.as_deref(), Some("deploy"));
                assert_eq!(version.as_deref(), Some("0.22.0"));
                assert!(dry_run);
            }
            _ => panic!("expected agent update"),
        }
    }

    #[test]
    fn stats_json_and_positional_project() {
        let cli = Cli::try_parse_from(["rpi", "stats", "rateme", "--json"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Stats {
                json,
                project,
                watch,
                interval,
                ..
            } => {
                assert!(json);
                assert_eq!(project.as_deref(), Some("rateme"));
                assert!(!watch);
                assert_eq!(interval, 2); // default
            }
            _ => panic!("expected Stats"),
        }
    }
}

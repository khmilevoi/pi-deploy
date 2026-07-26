<div align="center">

<img src="assets/logo.svg" width="120" alt="rpi — raspberry triangle logo">

# rpi

**Deploy anything to your Pi.**

[![npm](https://img.shields.io/npm/v/rpi-deploy?color=C51A4A&label=npm)](https://www.npmjs.com/package/rpi-deploy)
[![ci](https://github.com/khmilevoi/rpi-deploy/actions/workflows/ci.yml/badge.svg)](https://github.com/khmilevoi/rpi-deploy/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[rpi.iiskelo.com](https://rpi.iiskelo.com) · [Releases](https://github.com/khmilevoi/rpi-deploy/releases) · [Quick Start](#quick-start) · [Commands](#commands) · [`rpi.toml`](#project-configuration-rpitoml)

</div>

---

`rpi` (npm package [`rpi-deploy`](https://www.npmjs.com/package/rpi-deploy)) deploys Docker Compose projects from Git to a Raspberry Pi — or any Linux host with `systemd`. An agent runs on the Pi; the CLI runs on your machine or in CI and reaches the agent through an SSH tunnel to a Unix socket, so the Pi exposes nothing but SSH. On `rpi deploy` the agent clones or fetches the repository, builds the Compose stack, starts the containers, runs a health check, and — if configured — publishes the service to the internet through a Cloudflare Tunnel.

```text
░░
▒▒▒▒    r p i
▓▓▓▓▓▓  deploy · myboard
▓▓▓▓
██

✓ fetch (2.1s)
✓ build (48.3s)
✓ start (5.6s)
✓ health (1.2s)
✓ route (0.8s)
✓ gc (0.3s)

▸ deployed ✓ myboard  →  https://myboard.example.com · 2 services (58.4s)
```

## Highlights

- **One-command setup** — `sudo rpi agent setup` bootstraps the Pi (user, dirs, systemd unit); `rpi setup` and `rpi init` wizards configure the client and the project.
- **No open ports** — the CLI tunnels over your existing SSH access; the agent listens on a Unix socket only.
- **Staged pipeline view** — `fetch → build → start → health → route → gc`, each stage collapsing into a timed `✓ build (48.3s)` summary.
- **Private repos without friction** — a deploy-key preflight verifies repo access before the pipeline and registers a read-only deploy key through your local `gh` (the token never leaves your machine, the private key never leaves the Pi). Without `gh` it prints the key and continues by itself once you add it — even picking up a `gh auth login` you run mid-wait.
- **Encrypted secrets** — `.env` plus arbitrary secret files, sent encrypted, stored age-encrypted on the agent, written into the checkout at deploy time (`.env` `0600`, secret files `0644` by default, both overridable with `[secrets].file_mode`).
- **Cloudflare Tunnel ingress** — one command installs `cloudflared`, creates or adopts the tunnel, and manages DNS entirely through the Cloudflare API. Hand-built tunnels are adopted without a rewrite or downtime.
- **Stable ports & health checks** — the agent allocates a stable host port per project, writes a Compose override, and probes HTTP (or TCP) before declaring success.
- **Latest-wins deploy queue** — a newer deploy supersedes the one in flight; `rpi deploy --cancel` aborts.
- **Admin commands** — declare `[commands]` in `rpi.toml` and run them inside the service container with `rpi command`.
- **Ops built in** — `rpi logs`, `rpi stats` (`-w` for a live dashboard TUI: CPU / memory / temperature cards with mini charts, plus a per-service table with status pills and memory bars), `rpi status`, `rpi doctor`, `rpi agent logs`, `rpi gc`.
- **Version-skew aware** — the CLI and agent handshake on `connect`, gate commands against advertised agent features, and print a banner instead of a confusing error when they're out of sync.
- **Fast install** — `npm install -g rpi-deploy` downloads a checksum-verified prebuilt binary (Windows x64, Linux x64/aarch64) in seconds, falling back to a source build elsewhere. A no-npm `install.sh` one-liner bootstraps binary-only hosts.
- **Client-triggered updates** — `rpi upgrade` brings the board's agent up to your CLI's version over the existing SSH + sudo path (download → SHA-256 verify → atomic swap → restart), no manual SSH session required.
- **Environment overlays** — `rpi deploy --env <name>` deploys a project's shared `test` environment or an ephemeral per-branch preview from an `rpi.<env>.toml` overlay next to the base `rpi.toml`, with its own hostname, an optional TTL that self-destructs it, and a one-time `on_create` hook; `rpi env ls/destroy/reset-data` manage the deployed set.
- **Configuration variables** — any string in `rpi.toml` can reference `${YOUR_VAR}` from `--vars`, or a resolver input like `${git.branch}`/`${env.slug}`; every container also gets the `RPI_*` runtime set (project, environment, branch, hostname, host port, commit sha) without configuring anything.

## Quick Start

**1. On the Raspberry Pi** (Docker and Node.js ≥ 18 are the prerequisites):

```bash
curl -fsSL https://get.docker.com | sh
sudo apt-get install -y nodejs npm

sudo npm install -g rpi-deploy   # prebuilt arm64 binary — seconds, not minutes
sudo rpi agent setup             # user, dirs, systemd unit; idempotent
rpi doctor                       # verify the installation
```

**2. On the developer machine:**

```bash
npm install -g rpi-deploy
rpi setup                        # wizard: SSH profile for your Pi
```

**3. In the project you want to deploy:**

```bash
rpi init                         # wizard: generates rpi.toml
rpi secrets send                 # only if the project needs secrets
rpi deploy
```

That's it — `rpi ls` shows the project, its host port, and its public hostname if one is configured.

## Commands

| Command | Description |
|---|---|
| `rpi deploy [--ref <git-ref>] [--no-gh-key] [--env <name> --vars K=V]` | Deploy the current project (reads `./rpi.toml`), or an environment overlay with `--env` |
| `rpi deploy --cancel` | Cancel active deploys of the current project |
| `rpi ls` (alias: `rpi ps`) | List projects on the agent |
| `rpi logs <project> [-f] [--tail N]` | Stream container logs |
| `rpi stats [project] [--json] [-w\|--watch] [--interval N]` | CPU / memory / disk / temperature metrics; `-w` opens a full-screen live view with history sparklines |
| `rpi start\|stop\|restart <project>` | Manage containers without a rebuild |
| `rpi rm <project> [--volumes]` | Remove a project |
| `rpi status [--json]` | Agent and host overview |
| `rpi doctor` | Environment self-diagnosis |
| `rpi gc` | Prune Docker images and build cache on the Pi |
| `rpi command [name] [-- <args>] [--env <name> --vars K=V]` | Run a `[commands]` entry in the service container; no name lists them |
| `rpi secrets send [--apply] [--env <name> --vars K=V]` | Send the env file and secret files (encrypted at rest) |
| `rpi secrets ls [--env <name> --vars K=V]` | List stored env keys and file paths (values are never transmitted) |
| `rpi config show [--env <name> --vars K=V]` | Print the resolved `rpi.toml`, overlay merged in, without contacting the agent |
| `rpi env ls [--all]` | List deployed environments for this project, or every environment with `--all` |
| `rpi env destroy <name>\|--full-key <key> [--vars K=V] [--yes]` | Tear down an environment: stack, volumes, ingress route, DNS, secrets, registry row |
| `rpi env reset-data <name>\|--full-key <key> [--vars K=V] [--yes]` | Wipe an environment's containers and volumes; the next deploy re-runs `on_create` |
| `rpi setup` | Wizard: server profile + SSH key + client config |
| `rpi init` | Wizard: generate `rpi.toml` in the current project |
| `rpi agent setup` | Bootstrap the agent on the Pi (run with `sudo`; idempotent) |
| `rpi agent status` / `rpi agent logs [-f] [--since 2h]` | Agent health and logs (falls back to `systemctl`/`journalctl` over SSH) |
| `rpi agent migrate [--list] [--dry-run] [--run <id>] [--all --yes]` | Host-level migrations |
| `rpi agent uninstall [--purge]` | Remove the agent (keeps data unless `--purge`) |

Every client command accepts `--server <profile>` to pick a configured server, or `--host`/`--user`/`--key` to connect without a config file.

## How It Works

```text
developer machine / CI                      Raspberry Pi
┌──────────────────┐                 ┌────────────────────────────────┐
│    rpi  (CLI)    │    ssh tunnel   │      rpi agent  (systemd)      │
│  reads rpi.toml  ├────────────────▶│  /run/rpi/agent.sock           │
└──────────────────┘                 │  SQLite state · port allocator │
                                     │  git fetch → compose build     │
                                     │  → up -d → health → ingress    │
                                     └────────────────────────────────┘
```

- `rpi agent run` is a daemon on the Pi, managed by `systemd`. It stores state in SQLite, selects a stable host port from `port_min..port_max`, writes a Compose override binding `127.0.0.1:<host-port>`, runs `docker compose build`, then `docker compose up -d --remove-orphans`, and health-checks the result.
- `rpi deploy`, `rpi ls`, `rpi secrets …`, and the other client commands run on a developer machine or CI runner and open an SSH tunnel to the agent's Unix socket.
- Deployments queue latest-wins: pushing a newer deploy supersedes the one in progress.
- Each deployable project carries an `rpi.toml` at its root. Run `rpi deploy` from the root of *that* project — not from this repository.

## Installation

Client and agent ship in the same npm package; the role comes from what you run after installing. On install the package downloads a prebuilt binary from the matching GitHub Release (Windows x64, Linux x64, Linux aarch64) and verifies its SHA-256 checksum. On other platforms, or when the download fails, it builds the bundled Rust sources (`cargo build --release --locked`; rustup is installed automatically if needed). Set `RPI_DEPLOY_BUILD_FROM_SOURCE=1` to force the source build. Installing with `--ignore-scripts` leaves the CLI unusable — as does npm's `allow-scripts` gate on recent versions, see [Troubleshooting](#troubleshooting).

Update on both roles:

```bash
npm install -g rpi-deploy@latest  # with sudo on the Pi, unless npm is nvm-managed
sudo rpi agent setup              # Pi only: swaps the binary and restarts the agent
```

> [!NOTE]
> If Node.js comes from `nvm` (or another per-user version manager), `sudo npm`/`sudo rpi` won't be found — `sudo` resets `PATH`. Install without `sudo` and forward `PATH` for the one command that needs root:
>
> ```bash
> npm install -g rpi-deploy
> sudo env "PATH=$PATH" rpi agent setup
> ```
>
> `agent setup` self-installs the binary to `/usr/local/bin/rpi`, which is on root's default `PATH`, so every later `sudo rpi …` works without the wrapper.

Upgrading from v0.5? The command was renamed `pi` → `rpi` and the project config `pi.toml` → `rpi.toml` (hard cutover). See [docs/migration-v0.5-to-v0.6.md](docs/migration-v0.5-to-v0.6.md).

### Installing without npm

```bash
curl -fsSL https://raw.githubusercontent.com/khmilevoi/rpi-deploy/master/scripts/install.sh | sh
```

Downloads and verifies the prebuilt binary and installs it to
`/usr/local/bin` (override with `RPI_INSTALL_DIR`); `RPI_VERSION` pins a
version (default: latest). It does not run setup — follow with
`sudo rpi agent setup` on a Pi, or `rpi setup` on a dev machine.

<details>
<summary><b>Build from source</b> (instead of npm)</summary>

**On the Pi** (simple, but slow on smaller boards):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

git clone https://github.com/khmilevoi/rpi-deploy.git
cd rpi-deploy
cargo build --release
sudo install -m 755 target/release/rpi /usr/local/bin/rpi
```

**Cross-build on the developer machine:**

```bash
cargo install cross
cross build --release --target aarch64-unknown-linux-gnu
scp target/aarch64-unknown-linux-gnu/release/rpi pi-user@pi-host.local:/tmp/rpi
# then on the Pi:
sudo install -m 755 /tmp/rpi /usr/local/bin/rpi
```

**CLI on the developer machine:**

```bash
cargo install --path crates/bin --locked
```

If `rpi` is not found afterwards, add `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows) to `PATH`.

</details>

<details>
<summary><b>SSH access</b> — creating and installing a key</summary>

The CLI needs passwordless SSH to the Pi. Create a key on the developer machine:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_pi        # PowerShell: $env:USERPROFILE\.ssh\id_ed25519_pi
```

Append the public key to `~/.ssh/authorized_keys` on the Pi (for the user the CLI will log in as), then test:

```bash
ssh -i ~/.ssh/id_ed25519_pi pi-user@pi-host.local true
```

Optionally add an SSH profile:

```sshconfig
Host pi-home
    HostName pi-host.local
    User pi-user
    IdentityFile ~/.ssh/id_ed25519_pi
    IdentitiesOnly yes
```

</details>

## Client Configuration

The CLI reads its config from the user config directory:

- Windows: `%APPDATA%\pi\config.toml`
- macOS/Linux: `~/.config/pi/config.toml`

`rpi setup` creates this file for you; the format is:

```toml
default = "home"

[servers.home]
host = "pi-host.local"
user = "pi-user"
key = "~/.ssh/id_ed25519_pi"
```

Select a profile per invocation with `--server home` or `PI_SERVER=home`; skip the file entirely with `--host`/`--user`/`--key` (useful in CI). Check the connection with `rpi ls` — with no projects deployed yet it prints `▸ no projects deployed yet`.

### Console theme

Output uses the raspberry brand theme: a `▸` marker on every message line, the site's green/amber for success/warn, and a pink-to-raspberry triangle banner on `rpi deploy`, bare `rpi`, and `rpi --version`. The banner only appears on an interactive terminal — piped and CI output stays plain. On truecolor terminals (`COLORTERM=truecolor`) colours render as the exact brand `#C51A4A`. Set `PI_THEME=classic` for the pre-brand look; `NO_COLOR` and non-TTY output disable styling entirely.

## Project Configuration: `rpi.toml`

Add `rpi.toml` to the root of the project you want to deploy (`rpi init` generates it):

```toml
schema = 1

[project]
name = "example-web"            # Compose project name and the agent's state key

[source]
repo = "git@github.com:you/example-web.git"
branch = "main"                 # default ref for `rpi deploy`

[build]
compose = "docker-compose.yml"  # Compose file inside the repository

[ingress]
service = "web"                 # Compose service that receives traffic
port = 3000                     # port inside the container
hostname = "app.example.com"    # optional: public ingress (Cloudflare Tunnel)
# expose = "lan"                # optional: bind 0.0.0.0 instead of 127.0.0.1

[healthcheck]
path = "/health"                # omit for a plain TCP probe
expect = "200"
timeout = "60s"

[secrets]
env = ".env"                    # optional, default ".env"
files = [                       # optional; recreated at the same paths on the Pi
  "certs/server.pem",
]
# file_mode = "0640"            # optional override; default: 0644 for files, 0600 for .env

[timeouts]                      # optional per-project overrides
fetch = "3m"
build = "45m"
up = "2m"
# command = "30m"               # for `rpi command`, default 10m
```

For a worker, bot, or internal service that needs no public HTTP ingress, simply omit `ingress.hostname`.

Field notes:

- `healthcheck.path` is probed through the allocated host port; without a path the agent uses a TCP probe.
- `secrets.files` are sent encrypted, stored age-encrypted on the agent, and written `0644` (readable beyond the agent's own account, because a container running as its image's own uid needs to read a bind-mounted secret) into the checkout on every deploy; `secrets.env`'s `.env` file stays `0600`. Paths are relative with forward slashes; `..` is rejected.
- `secrets.file_mode` overrides both of the modes above with one value — accepted forms are `"0644"`/`"644"` (owner read, optionally write; group/other read only; no execute or setuid/setgid/sticky bits). It requires an agent `>= 0.26.0` (the `secret-modes` capability) and, since the mode travels with the bundle, only takes effect on the next `rpi secrets send` — a `rpi deploy` that reuses an already-stored bundle keeps whatever mode that bundle was sent with.
- `expose = "lan"` binds the host port on `0.0.0.0`. On a host with a public IPv4 that means the public internet, and Docker bypasses host firewalls (UFW/iptables) for published ports — use it only on trusted networks or behind an external firewall.

### `[commands]` — admin commands (optional)

One-off admin commands runnable inside the service container with `rpi command`:

```toml
[commands]
create-invite = "node scripts/create-invite.js"
migrate = ["npx", "prisma", "migrate", "deploy"]
backup = "sh -c 'pg_dump mydb | gzip > /data/backup.gz'"

[commands.seed]                       # table form: pin a different service
run     = "node dist/scripts/seed.cjs"
service = "server"                    # optional; omitted => ingress service
```

- Values are a string (split with shell-word rules — quotes work, but no variables/pipes/redirects; need a shell? spell out `sh -c '…'`) or an explicit argv array. Names must match `[a-z0-9][a-z0-9_-]*`.
- Commands are registered on the agent **at deploy time** and run via `docker compose exec -T` in the `ingress.service` container by default. The agent only executes deployed commands — there is no generic remote exec.
- `rpi command` (no name) lists the deployed commands; extra args after `--` are appended to the declared argv. The remote exit code becomes the `rpi` exit code. Ctrl+C detaches and best-effort kills the run.
- Output streams to stdout line by line as the command runs — complete and untruncated, with no pane or tail window — so `rpi command migrate > migrate.log` captures exactly what ran. Only the closing verdict goes to stderr. (v0.25 removed `--full`, which existed for the old windowed pane — see [the migration note](docs/migration-command-full-flag.md).)

## Configuration Variables

Any string field of `rpi.toml` can carry a `${...}` reference. Three namespaces, told apart by syntax alone:

```toml
[source]
branch = "${BRANCH_NAME}"                        # --vars BRANCH_NAME=feature/login

[ingress]
hostname = "${env.slug}.preview.example.com"     # resolver input; env.* needs --env

[commands]
backup = "sh -c 'tar -C $${HOME} -czf /b.tgz .'" # $${ is a literal ${, left for the shell
```

- `${NAME}` — a **user variable** from `--vars NAME=VALUE` (repeatable). Names match `^[A-Z][A-Z0-9_]*$`; nothing is special-cased. `--vars` works with or without `--env`, and a key nothing references is an error, so a typo can't pass silently.
- `${ns.field}` — a **resolver input**: `${git.branch}`, `${git.sha}`, `${git.short_sha}`, and (with `--env`) `${env.name}`, `${env.slug}`. `${git.*}` is read lazily, only when referenced, so a config that uses none still works outside a repository; a detached `HEAD` is an error telling you to pass the branch via `--vars` rather than silently becoming `HEAD`.
- `RPI_*` — **runtime variables**, injected by the agent into every container and into `rpi command`'s exec: `RPI_PROJECT`, `RPI_PROJECT_BASE`, `RPI_ENV`, `RPI_ENV_SLUG`, `RPI_BRANCH_NAME`, `RPI_HOSTNAME`, `RPI_HOST_PORT`, `RPI_COMMIT_SHA`. They exist only at runtime — writing `${RPI_...}` in `rpi.toml` is an error — but `${RPI_*}` interpolates in your own `docker-compose.yml`. `rpi config show` prints them as a `[runtime]` preview. Needs an agent `>= 0.27.0`; an older one still deploys, it just injects nothing.

`${...}` is not allowed in `[project].name` (the project key must stay static). `source.branch` is resolved first, `${env.slug}` derives from the result, then every other field is resolved; substituted values are validated afterwards, which is why a raw branch name in `ingress.hostname` fails the DNS check and `${env.slug}` exists.

## Environments

An overlay file `rpi.<env>.toml` next to `rpi.toml` deploys the same project under a distinct key — a shared `test` environment, or an ephemeral per-branch preview — without duplicating the whole config:

```toml
# rpi.preview.toml
[source]
branch = "${BRANCH_NAME}"

[ingress]
hostname = "${env.slug}.preview.example.com"

[environment]
ttl = "72h"                 # optional; the reaper destroys it once expired
on_create = "migrate"       # optional; runs once, after the first successful health check
```

```bash
rpi deploy --env preview --vars BRANCH_NAME=feature/login
```

- The overlay merges onto the base field by field — scalars replace, tables merge key by key, `[commands]` and array fields replace wholesale, and an explicit `""` resets a field to unset. The deployed project key becomes `<base>--<env>`, or `<base>--<env>--<slug>` when the config references `${env.slug}`, and its hostname must differ from the base project's.
- `rpi config show --env preview --vars BRANCH_NAME=feature/login` prints the fully resolved config, plus a `[runtime]` preview of the `RPI_*` variables, without touching the agent — the way to check what a deploy would actually send.
- `rpi env destroy`/`reset-data` take the same `<env>` and `--vars` as the deploy; when the overlay is gone or no longer resolves, `--full-key <key>` (from `rpi env ls`) targets an environment without reading any config file.
- With no TTL, an environment lives until `rpi env destroy` removes it. With one, the agent's reaper tears it down on its own once it's past due (`agent.toml`'s `[environments] reap_interval`, default 1h) — as long as it isn't mid-deploy.
- `on_create` runs exactly once per environment key, after the first successful health check; `rpi env reset-data` clears the flag so the next deploy re-runs it against a clean database.

## Docker Compose Requirements

The agent publishes your service by writing an override file:

```yaml
services:
  web:
    ports:
      - "127.0.0.1:8000:3000"
```

So the production Compose file should **not** pin a fixed host port itself — that can conflict with the agent's allocator or another project on the same Pi. Use `expose`:

```yaml
services:
  web:
    build:
      context: .
    expose:
      - "3000"
```

For runtime files (logs, SQLite, uploads), mount directories instead of individual files that may not exist in a fresh clone, and git-ignore them:

```yaml
services:
  app:
    environment:
      DATABASE_URL: file:///data/app.db
    volumes:
      - ./data:/data
```

## Secrets

If `rpi.toml` has a `[secrets]` section, send the bundle from the project root before the first deploy:

```bash
rpi secrets send          # save on the agent; applied by the next deploy
rpi secrets send --apply  # save and restart the running stack with the new values
rpi secrets ls            # list stored env keys and file paths (never values)
```

The CLI reads the local env file and `[secrets].files`, sends them encrypted, and the agent stores an age-encrypted bundle in `/var/lib/rpi/secrets`. During `rpi deploy` the agent writes them into the project workdir before running Docker Compose.

By default `.env` is written `0600` and every file from `[secrets].files` is written `0644` — deliberately wider, because a container that consumes a bind-mounted secret usually runs as its own image's uid, not the agent's, and Docker has no way to `chown`/`chmod` a `file:`-sourced Compose secret on its own. Set `[secrets].file_mode` (see the sample above) to use one mode for both instead; it requires an agent `>= 0.26.0`. **The mode travels with the bundle**, so changing `file_mode` only takes effect the next time you actually send it — `rpi secrets send` (or `--apply`) — not on a plain `rpi deploy`, which just reuses whatever bundle (and mode) is already stored.

## Private Git Repositories

The Pi must be able to read `source.repo`. For private repos over SSH, `rpi deploy` runs a **deploy-key preflight** before starting the pipeline:

- With GitHub CLI (`gh`) logged in locally, the CLI registers a read-only deploy key for the repository automatically — the token never leaves your machine, the private key never leaves the Pi.
- Without `gh`, it prints the agent's public key with instructions and polls every 5 s (up to 10 min) until you add it — and if you run `gh auth login` mid-wait, it switches to automatic registration on the next poll.
- `--no-gh-key` skips the GitHub API path and always prints the key for manual setup (GitHub: *Repository → Settings → Deploy keys → Add deploy key*; write access is not needed).

## Cloudflare Tunnel

Only needed when the service must be reachable from the internet; outbound-only services can skip this section.

### Automatic setup (recommended)

One command bootstraps everything through the Cloudflare API — no `cloudflared tunnel login`, no `cert.pem`:

```bash
sudo rpi agent setup --with-cloudflared --cf-token-file <path> --domain <zone>
```

It installs the `cloudflared` binary for the host's CPU, creates (or adopts) the tunnel, writes the credentials JSON and a validated `config.yml`, fills the `[cloudflare]`/`[cloudflared]` sections of `/etc/rpi/agent.toml`, and enables the `cloudflared` systemd `--user` service. After that every deploy manages tunnel routes and proxied DNS records by itself.

- Pass the token via `--cf-token-file <path>`, `--cf-token-file -` (stdin), or the `CLOUDFLARE_API_TOKEN` environment variable. The inline `--cf-token` flag is deprecated (it leaks via `ps`/shell history).
- Required token scopes: `Zone:DNS:Edit`, `Zone:Zone:Read`, `Account:Cloudflare Tunnel:Edit`. The account id is derived from the token.
- `--tunnel <name>` overrides the tunnel name (default: derived from the hostname). `--dry-run` previews the plan.
- The token is stored at `/var/lib/rpi/cloudflare/token` (`root:rpi-secrets`, mode `0640`).
- A fresh install is refused when a foreign tunnel is already running on the host.

### Adopting an existing tunnel

If `/var/lib/rpi/cloudflared/config.yml` already exists (a hand-built tunnel), the same command adopts it instead of recreating anything: the existing `config.yml` is **never rewritten** and `cloudflared` is **not restarted** — hand-written routes and uptime are preserved. The tunnel id is taken from the `tunnel:` key, credentials are checked at the `credentials-file:` path, and the agent config is extended so future deploys manage routes and DNS. `Zone:Zone:Read` + `Zone:DNS:Edit` scopes suffice when `tunnel:` holds a UUID.

### Manual ingress

Without a `[cloudflared]` section in `/etc/rpi/agent.toml`, deploys still succeed — the agent logs the address to route manually (`hostname -> http://127.0.0.1:<host-port>`; see the port in `rpi ls`). If a project declares `[ingress] hostname` while agent ingress is disabled, the deploy summary and `rpi doctor` warn loudly so a manually-managed route is never silently mismatched.

<details>
<summary><b>Manual cloudflared configuration</b> (without the API token)</summary>

A typical locally managed `cloudflared` config:

```yaml
tunnel: <tunnel-id-or-name>
credentials-file: /var/lib/rpi/cloudflared/<tunnel-id>.json

ingress:
  - hostname: app.example.com
    service: http://127.0.0.1:8000
  - service: http_status:404   # must remain the catch-all
```

To let the agent edit this config and manage DNS itself, add both sections to `/etc/rpi/agent.toml`:

```toml
[cloudflared]
config = "/var/lib/rpi/cloudflared/config.yml"
tunnel = "home"
restart = ["systemctl", "--user", "restart", "cloudflared"]

[cloudflare]
zone = "example.com"
token_file = "/var/lib/rpi/cloudflare/token"
```

`rpi-agent` must be able to read/write `config.yml`, run the `restart` command without a password prompt, and read `token_file` (a token scoped to DNS edit + tunnel read). DNS records (proxied CNAMEs to `<tunnel-id>.cfargotunnel.com`) go through the Cloudflare API. Both sections are required — with `[cloudflared]` alone, ingress falls back to manual with a warning.

</details>

## Agent Management

`sudo rpi agent setup` is idempotent: it creates the `rpi-agent` system user, directories, the systemd unit, and `/etc/rpi/agent.toml` if missing, repairs permissions, and never touches `secret.key` or `state.db`. Re-running it is always safe; `--dry-run` previews.

### Updating the agent

Update the rpi binary on a board to a chosen version from your laptop:

```bash
rpi upgrade                 # bring the board up to this CLI's version
rpi upgrade --version 0.22.0
rpi upgrade --version latest --yes
```

`rpi upgrade` opens `ssh -t <user>@<host> sudo rpi agent update --version <X>`,
so a board whose sudo needs a password will prompt in your own terminal. It
reuses your existing SSH profile (`--server` / `PI_SERVER` / default), shows
`current → target`, and re-reads `/v1/version` afterwards to confirm. It needs
real SSH access to the board, so it doesn't apply to the local-dev override and
errors out if `PI_AGENT_URL` is set.

On the board, `rpi agent update` downloads the release archive
(`rpi-v<version>-<triple>.tar.gz`) from GitHub Releases, verifies its SHA256
against the release `SHA256SUMS`, swaps `/usr/local/bin/rpi`, re-runs the
idempotent `rpi agent setup`, and restarts `rpi-agent`. If the board was
installed via npm, it refreshes the global `rpi-deploy@<version>` instead.

For unattended updates, add a narrow sudoers rule (not blanket NOPASSWD):

```
<login-user> ALL=(root) NOPASSWD: /usr/local/bin/rpi agent update *
```

Host-level upgrades run through a uniform migration framework:

```bash
rpi agent migrate --list        # every migration and whether it's applied
rpi agent migrate --dry-run     # plan only
rpi agent migrate --run <id>    # apply a specific (disruptive) migration
rpi agent migrate --all --yes   # apply everything pending
```

Applied migrations are recorded in `state.db` and never re-run. Non-disruptive ones run automatically during `rpi agent setup`; disruptive ones are only reported there and must be applied explicitly. (Currently registered: `pi-to-rpi`, renaming a legacy `pi-agent` install.)

<details>
<summary><b>Manual agent install</b> (what <code>rpi agent setup</code> does for you)</summary>

Create the service user and directories:

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin rpi-agent || true
sudo usermod -aG docker rpi-agent
sudo usermod -aG rpi-agent "$USER"   # tunnel access to the socket; re-login after

sudo install -d -m 0750 -o rpi-agent -g rpi-agent /var/lib/rpi
sudo mkdir -p /etc/rpi
```

Create `/etc/rpi/agent.toml`:

```toml
data_dir = "/var/lib/rpi"
socket = "/run/rpi/agent.sock"
port_min = 8000
port_max = 8999
build_concurrency = 1
history_keep = 50

[timeouts]
fetch = "2m"
build = "30m"
up = "5m"

[gc]
disk_threshold_percent = 85
```

Create `/etc/systemd/system/rpi-agent.service`:

```ini
[Unit]
Description=pi deploy agent
After=network-online.target docker.service
Wants=network-online.target

[Service]
User=rpi-agent
Group=rpi-agent
ExecStart=/usr/local/bin/rpi agent run --config /etc/rpi/agent.toml
RuntimeDirectory=pi
RuntimeDirectoryMode=0750
Restart=on-failure
Environment=HOME=/var/lib/rpi
Environment=XDG_CONFIG_HOME=/var/lib/rpi/.config
Environment=XDG_CACHE_HOME=/var/lib/rpi/.cache
WorkingDirectory=/var/lib/rpi

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rpi-agent
```

`rpi-agent` is intentionally created without a home directory; the `HOME`/`XDG_*` variables keep Docker/BuildKit from writing to a missing `/home/rpi-agent`.

</details>

## Development

```bash
cargo test --workspace
```

### Full Docker end-to-end test

The production-path e2e test builds the current `rpi` once, starts an isolated
target with real SSH and `/run/rpi/agent.sock`, and deploys a local Git fixture
into a dedicated Docker-in-Docker daemon:

```bash
npm run test:e2e
```

Requirements: Node.js 18+, Docker Desktop using Linux containers (or Docker
Engine on Linux), Docker Compose 2.33.1+, and support for privileged Linux
containers. The command starts a **privileged** `docker:28-dind` service, but it
does not mount the host Docker socket or publish test ports to the host.

The scenario covers `rpi deploy` over SSH, the agent Unix socket, real Compose
build/up, HTTP health, a stable second deploy, `rpi ls`, and `rpi rm`. It does
not cover systemd installation, Cloudflare, secrets, private Git, or ARM.

On failure, inspect `target/e2e-artifacts/<run-id>`. The launcher records build,
outer Compose, agent, nested Docker, scenario, and cleanup diagnostics before it
removes the run's containers, networks, and volumes. Set `RPI_E2E_KEEP=1` to
keep a run's stack around for inspection; the launcher prints the cleanup
command.

### Manual dev stack

`npm run e2e:dev:up` starts the same topology plus a long-lived `client-dev`
container (Compose profile `dev`, fixed project name `rpi-e2e-dev`):

```bash
docker exec -it rpi-e2e-dev-client-dev-1 bash   # rpi CLI + SSH key
docker exec -it rpi-e2e-dev-target-1 bash       # agent + sshd + nested Docker
```

Inside `client-dev`, run `source /opt/e2e/lib.sh && e2e_client_init` once, then
use `rpi <cmd> --host target --user deploy --key /run/e2e-keys/id_ed25519`.
`npm run e2e:dev:down` removes the dev stack and its image.

Run a local TCP agent and point the CLI at it:

```bash
cargo run -p pi -- agent run --config dev/agent.toml
export PI_AGENT_URL="http://127.0.0.1:7700"   # PowerShell: $env:PI_AGENT_URL = "http://127.0.0.1:7700"
```

## Troubleshooting

### `npm warn allow-scripts …` / `rpi: binary not built`

npm blocked the `postinstall` script that installs the binary. Reinstall with the script allowed:

```bash
npm install -g --allow-scripts=rpi-deploy rpi-deploy       # add sudo for a system-wide npm
npm config set allow-scripts=rpi-deploy --location=user    # persist for future updates
```

`npm approve-scripts` does not work for a global install (`EGLOBAL` — no project `package.json`).

### `sudo: npm: command not found` / `sudo: rpi: command not found`

Node.js is nvm-managed and `sudo` resets `PATH`. Install without `sudo`, then `sudo env "PATH=$PATH" rpi agent setup` once — see the note in [Installation](#installation).

### `rpi ls` does not connect

Check SSH from the developer machine, then the agent on the Pi:

```bash
ssh -i ~/.ssh/id_ed25519_pi pi-user@pi-host.local true

systemctl status rpi-agent
journalctl -u rpi-agent -n 100 --no-pager
ls -l /run/rpi/agent.sock
groups "$USER"      # must include rpi-agent; open a new SSH session after adding
```

On the Pi itself the agent answers directly:

```bash
curl --unix-socket /run/rpi/agent.sock http://localhost/v1/version
```

### Clone fails with `Permission denied (publickey)`

The Pi cannot authenticate to `source.repo`. Add the deploy key printed by the preflight (or the fetch stage), or configure another SSH key with read access, then rerun `rpi deploy`.

### Docker build fails with `/home/rpi-agent` errors

The systemd unit must set `HOME=/var/lib/rpi` and the `XDG_*` variables (see the manual install section above), then:

```bash
sudo systemctl daemon-reload && sudo systemctl restart rpi-agent
```

### Docker permission denied

```bash
sudo usermod -aG docker rpi-agent
sudo systemctl restart rpi-agent
```

### Compose does not see secrets

Run `rpi secrets send` before `rpi deploy` (or `rpi secrets send --apply` for a running project).

### Health check fails

The app must listen on `0.0.0.0` inside the container; `[ingress].port` must match the container port; `[healthcheck].path` must exist and answer with `[healthcheck].expect`. On the Pi: `docker compose -p <project> ps` and `curl http://127.0.0.1:<host-port>/health`.

### Host port is in use

A fixed host `ports:` mapping in the Compose file conflicts with the agent's allocator. Remove it and use `expose` — see [Docker Compose Requirements](#docker-compose-requirements).

### CLI and agent versions differ

The CLI warns when its version differs from the agent's. Update both sides to the same release: `npm install -g rpi-deploy@latest` on both, plus `sudo rpi agent setup` on the Pi.

## Documentation

- [CI deploys with GitHub Actions](docs/ci-github-actions.md)
- [Migration: `pi` → `rpi` (v0.5 → v0.6)](docs/migration-v0.5-to-v0.6.md)
- [Migration: `[env]` → `[secrets]`](docs/migration-env-to-secrets.md)
- [Migration: `rpi command --full` removed (v0.25)](docs/migration-command-full-flag.md)
- [Release notes](https://github.com/khmilevoi/rpi-deploy/releases) — full version history

## License

[MIT](LICENSE) © khmilevoi

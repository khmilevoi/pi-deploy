---
name: rpi-cli
description: Use when operating, installing, testing, or troubleshooting the rpi deploy CLI, including rpi deploy, rpi ls, rpi secrets push, rpi secrets send (deprecated alias), rpi secrets ls, rpi secrets diff, rpi secrets group ls/rm, rpi command, rpi logs, rpi stats, rpi start/stop/restart/rm, rpi status, rpi doctor, rpi gc, rpi agent run, rpi setup, rpi init, rpi agent setup, rpi upgrade, rpi agent update, install.sh, SSH profiles, PI_SERVER, PI_AGENT_URL, local dev agents, CLI-to-agent connection failures, rpi config show and its [runtime] block, rpi env ls/destroy/reset-data (including --full-key), --env/--vars environment overlays, configuration variables, the RPI_* runtime variables injected into containers, and secret groups ([secrets].groups).
---

# Rpi CLI

## Overview

Use this skill for commands and workflows around the `rpi` binary. Treat the repository README and CLI source as the source of truth when behavior has drifted.

Primary references in this repo:

- `README.md`
- `crates/bin/src/main.rs`
- `crates/bin/src/cli/config.rs`
- `crates/bin/src/cli/commands.rs`

## Command Map

| Task | Command |
| --- | --- |
| Deploy current project | `rpi deploy` |
| Deploy a ref | `rpi deploy --ref <branch-tag-or-sha>` |
| Cancel active deploys for current `rpi.toml` project | `rpi deploy --cancel` |
| Deploy an environment overlay (`rpi.<env>.toml`) | `rpi deploy --env <env> [--vars KEY=VALUE]` |
| Deploy the base project with variables (no overlay) | `rpi deploy --vars KEY=VALUE` |
| Print the resolved config plus its `[runtime]` preview, no agent contact | `rpi config show [--env <env>] [--vars KEY=VALUE]` |
| List environments (this project's, or `--all`) | `rpi env ls [--all] [--vars KEY=VALUE]` |
| Destroy an environment (stack, volumes, ingress, DNS, secrets, registry) | `rpi env destroy <env> [--vars ...] [--yes]` |
| Destroy an environment whose config no longer resolves | `rpi env destroy --full-key <key> [--yes]` |
| Remove an environment's volumes; next deploy re-runs `on_create` | `rpi env reset-data <env> [--vars ...] [--yes]` |
| Same, by key, with no config file | `rpi env reset-data --full-key <key> [--yes]` |
| List projects | `rpi ls` or `rpi ps` |
| Push secrets to a group, or this project's own bundle | `rpi secrets push [--group <name>] [--merge] [--force] [--apply] [--env <env>] [--vars ...]` |
| Push secrets and restart the running stack | `rpi secrets push --apply` (`rpi secrets send` is a deprecated alias, no `--group`) |
| List the effective merged secrets, or one group's head | `rpi secrets ls [--group <name>] [--env <env>] [--vars ...]` |
| Compare local secret sources against the agent by digest | `rpi secrets diff [--group <name>] [--env <env>] [--vars ...]` |
| List this project's secret groups and who attaches them | `rpi secrets group ls [--env <env>] [--vars ...]` |
| Delete a secret group | `rpi secrets group rm <name> [--force] [--yes] [--env <env>] [--vars ...]` |
| Stream container logs | `rpi logs <project> [-f] [--tail N]` |
| Live CPU/memory/disk metrics | `rpi stats [project]` |
| Start / stop / restart project containers | `rpi start\|stop\|restart <project>` |
| Remove a project | `rpi rm <project> [--volumes]` |
| Agent and host overview | `rpi status` |
| Environment self-diagnosis | `rpi doctor` |
| Prune agent Docker images/build cache | `rpi gc` |
| List commands deployed on the agent | `rpi command` |
| Run a deployed `[commands]` entry | `rpi command <name>` |
| Run a deployed entry with extra args | `rpi command <name> -- <extra-args>` |
| Run a command against an environment overlay | `rpi command <name> --env <env> [--vars ...]` |
| Run foreground agent | `rpi agent run --config <agent.toml>` |
| Agent status on the Pi | `rpi agent status` |
| Agent logs on the Pi | `rpi agent logs [-f] [--since 2h]` |
| One-command developer setup | `rpi setup` |
| Scaffold a new `rpi.toml` | `rpi init` |
| Install/configure the agent on the Pi | `rpi agent setup` |
| Update a board's agent from the client (SSH + sudo) | `rpi upgrade [--version <X\|latest>] [--yes]` |
| Update the rpi binary on the board (run with sudo) | `rpi agent update [--version <X>] [--user <u>] [--dry-run]` |
| Uninstall the agent (keeps data unless `--purge`) | `rpi agent uninstall` |

Remote commands accept either a named profile or direct SSH flags:

```bash
rpi ls --server home
PI_SERVER=home rpi ls
rpi deploy --host pi-host.local --user pi-user --key ~/.ssh/id_ed25519_pi
```

Do not combine `--server` with `--host`; direct `--host` mode requires `--user`.

To install without npm (bootstrap the prebuilt binary directly), use `scripts/install.sh`:

```bash
curl -fsSL https://raw.githubusercontent.com/khmilevoi/rpi-deploy/master/scripts/install.sh | sh
```

Env overrides: `RPI_VERSION` (pin a version, default latest), `RPI_INSTALL_DIR`
(default `/usr/local/bin`). It only installs the binary — it does not run
`rpi agent setup` or `rpi setup`.

## Running Admin Commands

`rpi command` runs entries declared in `[commands]` in `rpi.toml`, inside the
`ingress.service` container by default; use the `[commands.<name>]` table form with `service = "<other-service>"` to run in a different compose service:

```bash
rpi command                                   # list mode: commands deployed on the agent
rpi command create-invite                     # run mode: execute a deployed command
rpi command create-invite -- --email x@y.com  # `--` separates extra args, appended to the declared argv
```

- The remote exit code becomes the `rpi` exit code.
- Ctrl+C detaches and best-effort kills the run on the agent; the in-container
  process may still survive, per standard `docker exec` behavior.
- A 404 from an old agent that predates `[commands]` support surfaces as
  "agent does not support [commands]; update rpi-agent on the Pi" — update the
  agent binary and redeploy.

## Configuration Variables

`--vars KEY=VALUE` (repeatable) supplies user variables to any command that
reads a configuration: `rpi deploy`, `rpi command`, `rpi secrets send`,
`rpi secrets ls`, `rpi config show`, and `rpi env destroy`/`reset-data`.

- **`--vars` does not require `--env`.** It works against the base
  `rpi.toml` alone.
- **Names are arbitrary**, matching `^[A-Z][A-Z0-9_]*$` — `BRANCH_NAME` has
  no special status. `RPI_`-prefixed names are refused (that namespace is
  runtime-only). A `--vars` key nothing references is an error naming the
  key, so a typo never passes silently.
- Besides user variables, a configuration can reference resolver inputs
  `${git.branch}`, `${git.sha}`, `${git.short_sha}` and — only when `--env`
  selected an environment — `${env.name}`, `${env.slug}`. `${git.*}` is read
  lazily, so a config that uses none still resolves outside a repository; a
  detached `HEAD` is an error telling you to pass the branch via `--vars`.
  See the `rpi-toml` skill for the full model.

`rpi config show` resolves everything locally and prints the merged TOML,
then a `[runtime]` block previewing the `RPI_*` variables a deploy would
inject into the containers. `RPI_HOST_PORT` and `RPI_COMMIT_SHA` appear as
`<assigned by agent>` — the agent allocates the port and learns the sha at
fetch time, so the CLI cannot know them:

```bash
rpi config show --vars BRANCH_NAME=feature/login
```

```toml
# ... resolved rpi.toml ...

[runtime]
RPI_PROJECT = "myapp"
RPI_PROJECT_BASE = "myapp"
RPI_BRANCH_NAME = "feature/login"
RPI_HOSTNAME = "app.example.com"
RPI_HOST_PORT = "<assigned by agent>"
RPI_COMMIT_SHA = "<assigned by agent>"
```

`RPI_*` injection needs an agent `>= 0.27.0` (`runtime-vars`). It is a
degradable capability: against an older agent `rpi deploy` prints one warning
and deploys anyway, with no `RPI_*` reaching any container.

## Environment Overlays

`--env <name>` (with an optional repeatable `--vars KEY=VALUE`) deploys or
operates a variant of the current project defined by an `rpi.<env>.toml`
overlay next to `rpi.toml` — a shared `test` environment, or a per-branch
preview. Accepted by `rpi deploy`, `rpi command`, `rpi secrets send`, and
`rpi secrets ls`; see the `rpi-toml` skill for the overlay file's schema and
merge rules.

```bash
rpi deploy --env test                              # static overlay: rpi.test.toml
rpi deploy --env branch --vars BRANCH_NAME=feature/login  # parameterized overlay
rpi config show --env branch --vars BRANCH_NAME=feature/login  # preview the merge, no agent contact
```

The deployed key is `<base>--<env>`, or `<base>--<env>--<slug>` when the
configuration references `${env.slug}`. A `source.branch` computed from a
variable (`${BRANCH_NAME}`, `${git.branch}`, `${git.short_sha}`, …) while
nothing references `${env.slug}` warns that the key stays `<base>--<env>`, so
every branch deploying that environment shares it — the usual cause of "my
two branches keep overwriting each other". A static branch never warns, no
matter how many other variables the overlay uses: a shared stand
(`.env.${STAGE}` behind a fixed hostname) is a legitimate configuration.

`rpi env` manages what a `--env` deploy already put on the agent:

```bash
rpi env ls                 # this project's environments (resolves ./rpi.toml for the base filter)
rpi env ls --vars BRANCH_NAME=feature/login   # ... when that base file needs a variable
rpi env ls --all           # every environment on the agent; reads no config file
rpi env destroy test       # tears down stack, volumes, ingress, DNS, secrets, and the registry entry
rpi env reset-data test    # drops volumes only; next `rpi deploy --env test` re-runs on_create
rpi env destroy --full-key myapp--branch--feature-login   # by key, reads no config file
```

- `rpi env destroy`/`reset-data` resolve the local overlay on the `<env>`
  path to compute the target key (same resolution and validation as
  `rpi deploy --env`, so the same `--vars` are needed), then prompt for that
  key to be typed back for confirmation unless `--yes` is passed.
- `--full-key <key>` is the escape hatch when that no longer works — a
  deleted overlay, a renamed branch, a directory that stopped resolving. It
  reads **no** configuration file at all (not the overlay, not `rpi.toml`),
  validating only the key's own `base--env` / `base--env--slug` shape, and is
  mutually exclusive with both `<env>` and `--vars`. Copy the key from
  `rpi env ls`. (The flag is `--full-key`, not `--key`: `--key` is already
  the SSH private key path on every remote command.)
- `rpi env destroy` is idempotent — a key that no longer exists reports
  "already absent" instead of erroring.
- `rpi env ls` without `--all` resolves `./rpi.toml` (no overlay) just to
  learn the base name it filters by, so it takes its own `--vars` for a base
  file that references user variables. `--all` ignores them — it reads no
  configuration file at all — and stays composable with `--vars`, because it
  is what the resolution failure tells you to fall back to.
- These commands require an agent that advertises the `environments`
  feature (agent `>= 0.24.0`); an older agent gets an upgrade message
  instead of a raw connection error.
- An environment's `[environment].ttl` (set in its overlay) is enforced
  agent-side by a background reaper, not by the CLI: the agent sweeps every
  environment on a timer (`[environments].reap_interval` in `agent.toml`,
  duration format, default one hour) and tears down any whose TTL has
  elapsed since its last successful deploy. See
  `docs/architecture/flows/environments.md` for the full flow.

## Secret Groups

A secret group is a named, reusable set of secrets owned by a project's
*base* namespace and attached declaratively via `[secrets].groups` in
`rpi.toml` (see the `rpi-toml` skill for the field). Pushing a group once and
declaring it from every branch preview's overlay means a new branch never
needs its own secrets upload:

```bash
rpi secrets push --group shared              # create or replace the group wholesale
rpi secrets push --group shared --merge      # upsert onto what is already stored
rpi secrets push --group shared --force      # overwrite unconditionally, skip the revision guard
rpi secrets group ls                         # this project's groups, revision, size, who attaches each
rpi secrets ls --group shared                # one group's head: names, digests, sizes, no merging
rpi secrets diff --group shared              # what a push would change, by digest, never a value
rpi secrets group rm shared                  # delete; prompts for the group name
rpi secrets group rm shared --force --yes    # --force: delete though a project declares it; --yes: skip the prompt
```

- `rpi secrets push` (no `--group`) targets this deploy key's own bundle and
  behaves exactly like the pre-groups `rpi secrets send`, which remains a
  deprecated alias for it with no `--group` support.
- At deploy time the agent resolves every declared group in order, then this
  deploy key's own bundle on top, and merges them per object (later layer
  wins by variable name/file path). A declared group that is missing or
  empty fails the deploy naming the group and the `rpi secrets push --group
  <name>` command that fixes it.
- Every push (group or key) is a conditional write guarded by a revision
  counter: unless `--force`, the CLI reads the target's current revision
  first and sends it as the expected revision; a write whose expectation is
  stale is rejected with an HTTP 409 telling the caller to re-run and see
  the diff, or pass `--force`. No command or endpoint ever returns a secret
  value — only names, paths, sizes, revisions and digests, whether listing,
  diffing, or reporting a push's result.
- `rpi secrets group rm` destroys encrypted secrets several environments may
  share, so unless `--yes` is passed it prints the group's revision and
  everyone who still declares it, then asks for the group name to be typed
  back — the same shape as `rpi rm`. `--force` and `--yes` are separate
  flags on purpose: `--force` only waives the "a project still declares it"
  guard, and must not double as confirmation. Deleting a name that was never
  pushed is a 404, so a typo is distinguishable from a real deletion.
- `rpi rm <project>` on a base project drops its groups along with it;
  `rpi env destroy` and the TTL reaper never do — an environment borrows its
  base's groups, it doesn't own them.
- Requires an agent `>= 0.27.0` (the `secret-groups` capability); an older
  agent gets an upgrade message instead of a raw connection error. That
  includes `rpi deploy`: a project whose `[secrets].groups` is non-empty is
  gated too, because an agent that predates groups would silently ignore the
  field and start the application with a group-less (often empty) `.env`. See
  `docs/architecture/flows/secrets.md` for the full layering, conditional-
  write, and teardown-ownership rules.

## Secrets File Mode

`rpi secrets ls` reports the effective mode secret files (and, if set,
`.env`) will be written with — `file mode: 0644` when the bundle has none
configured, or whatever `[secrets].file_mode` resolved to. Setting
`[secrets].file_mode` in `rpi.toml` requires an agent `>= 0.26.0` (the
`secret-modes` capability); `rpi secrets send`/`send --apply` refuse with an
upgrade hint against an older agent instead of silently ignoring the
setting. The mode travels with the stored bundle, so it only changes on the
next `rpi secrets send` (or `--apply`) — a `rpi deploy` that reuses an
already-stored bundle keeps whatever mode that bundle carries.

## Client Profile

The CLI reads the user config at:

- Windows: `%APPDATA%\pi\config.toml`
- macOS/Linux: `~/.config/pi/config.toml`

Minimal config:

```toml
default = "home"

[servers.home]
host = "pi-host.local"
user = "pi-user"
key = "~/.ssh/id_ed25519_pi"
```

Selection order is `--server`, then `PI_SERVER`, then `default`, then the only configured server if exactly one exists.

## Local Development

From this repository, run a TCP agent:

```bash
cargo run -p pi -- agent run --config dev/agent.toml
```

Point the CLI to it:

```bash
export PI_AGENT_URL="http://127.0.0.1:7700"
```

PowerShell:

```powershell
$env:PI_AGENT_URL = "http://127.0.0.1:7700"
```

Use local mode for CLI/API testing. Use SSH profile mode when validating real Pi connectivity.

## Deployment Checklist

Before `rpi deploy`:

1. Run from the deployable project's root, not necessarily from this repository root.
2. Confirm `./rpi.toml` exists and has the intended project name, repo, branch, service, and port.
3. Confirm the Pi can read `source.repo`; private repos may require a deploy key on the Pi.
4. If `[secrets]` is configured (env file and/or files) and secrets are required, run `rpi secrets send` before the first deploy.
5. Prefer Compose `expose` for the managed service; avoid fixed host `ports` that conflict with rpi's allocator.

## Troubleshooting

For connection failures, isolate layers in this order:

1. SSH from the developer machine: `ssh -i <key> <user>@<host> true`
2. Agent service on the Pi: `systemctl status rpi-agent`
3. Agent logs: `journalctl -u rpi-agent -n 100 --no-pager`
4. Socket permissions: `ls -l /run/rpi/agent.sock` and `groups "$USER"`
5. Direct socket API on the Pi: `curl --unix-socket /run/rpi/agent.sock http://localhost/v1/version`

For deploy failures:

- `Permission denied (publickey)`: the Pi cannot fetch `source.repo`; add the printed deploy key to the repository.
- Docker `/home/rpi-agent` errors: ensure the systemd unit sets `HOME=/var/lib/rpi`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, and `WorkingDirectory=/var/lib/rpi`.
- Compose does not see secrets: run `rpi secrets send`, or `rpi secrets send --apply` for an already running stack.
- Health check fails: verify the app listens on `0.0.0.0`, `[ingress].port` is the container port, and `[healthcheck]` matches the endpoint.
- Host port conflict: remove fixed host `ports:` from Compose and let rpi write the override.

## Editing The CLI

When changing CLI behavior, update both implementation and documentation:

- CLI shape: `crates/bin/src/main.rs`
- profile resolution: `crates/bin/src/cli/config.rs`
- command behavior: `crates/bin/src/cli/commands.rs`
- configuration resolution: `crates/bin/src/cli/overlay.rs`, `vars.rs`, `gitctx.rs`
- `rpi env` subcommands: `crates/bin/src/cli/envcmds.rs`
- capability gates and their policies: `crates/bin/src/compat.rs`
- user-facing docs and examples: `README.md`

Run focused tests first, then the workspace suite when practical:

```bash
cargo test -p pi
cargo test --workspace
```

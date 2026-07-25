# CLI philosophy

Why `rpi` commands are built the way they are. This is the "why"; the
`add-cli-command` skill is the mechanical "how" (files, order, tests). Read
this before designing a new command or a new flag on an existing one.

## 1. The CLI ↔ agent boundary

The agent (`rpi agent run`) executes **only on the board** — it owns Docker,
the filesystem, systemd, and the SQLite state. The CLI runs on the
developer's machine or in CI and is a thin, stateless client: it opens an SSH
tunnel to the agent's Unix socket and talks HTTP over it. Nothing but SSH is
exposed by the Pi.

This means:

- **No business logic in the CLI.** Deploy orchestration, health checks,
  port allocation, tunnel/DNS management — all of it lives in
  `crates/application`/`crates/infrastructure` and runs on the agent. The CLI
  renders responses; it does not compute them. If you find yourself writing
  an `if` in `cli/commands.rs` that decides *what* should happen rather than
  *how to display* what happened, that logic belongs on the agent side.
- **The agent never trusts the client's input at face value.** Every
  handler in `agent/http.rs` validates (`is_valid_name`, etc.) before calling
  a use case, exactly as if the request came from an untrusted network peer
  — because over a shared SSH tunnel, in principle, it could.
- **Two exceptions, and they're explicit.** `rpi init` and `rpi setup` run
  entirely on the developer's machine with no agent involved — they write
  local config (`rpi.toml`, the SSH profile). The `add-cli-command` skill
  calls these out as "local-only" and skips the agent-side checklist for
  them. Don't let a local-only command grow agent-shaped logic, and don't
  let an agent command grow a local fallback that duplicates it — `rpi
  agent status`/`rpi agent logs` falling back to `systemctl`/`journalctl`
  over plain SSH is the one deliberate exception, used only when the agent
  itself might be down.

## 2. Command structure and nesting

Commands read `rpi <noun> <verb>` and stop at two levels:
`rpi secrets send`, `rpi agent setup`, `rpi command <name>`. There is no
`rpi agent secrets rotate`-style third level anywhere in the tree, and new
commands shouldn't introduce one. If a feature seems to need a third level,
it usually means the noun is wrong — split it into its own top-level group
instead of nesting deeper.

Flat top-level verbs (`rpi deploy`, `rpi ls`, `rpi logs`, `rpi status`,
`rpi doctor`, `rpi gc`) are for the handful of operations a user reaches for
directly, every day, without needing a namespace. Anything that clusters —
secrets management, agent lifecycle — gets a noun group (`Secrets`,
`Agent`) instead of prefixing every verb (`rpi secrets-send`,
`rpi secrets-ls`).

## 3. Help is generated, not written

`--help` and `rpi help <cmd>` are clap-generated from the `///` doc comments
on `enum Cmd` / `enum SecretsCmd` / `enum AgentCmd` — see step 9 of the
`add-cli-command` checklist: the doc comment "IS the help text." There is
exactly one source of truth for what a command does.

`rpi help` itself is not a hand-maintained command — it's clap's built-in
alias for `--help`, working uniformly at every nesting level (`rpi help`,
`rpi help agent`, `rpi help agent setup`). Keep it that way: never add a
bespoke help string, a separate "usage" printer, or a docs page that
restates what a doc comment already says — it will drift the moment one of
them is edited and the other isn't. If a command's behavior needs more
explanation than a one-line doc comment can carry, that's a signal the
command is doing too much, not a reason to bolt on a second help system.

## 4. Output: stdout is data, stderr is status

- stdout carries the payload a script or a human parses: tables
  (`output::table()`), JSON (`--json`), a one-line result, or — for `rpi
  command` — the remote command's own output, streamed line by line and
  untruncated (`output::log_line()`), because that output *is* the payload.
- stderr carries everything about *how the operation is going*: `success`,
  `warn`, `note`, `error`, and the live `LogPane` for streamed operations.

This split exists so `rpi ls --json | jq ...` works without stripping
progress noise, and so a failed `rpi deploy` still shows its staged
`fetch → build → start → health → route → gc` timeline even when the caller
discards stdout. Every new command's rendering should keep this split rather
than mixing a status line into the data stream because it was convenient in
the moment.

## 5. Safety and trust boundaries

- **No open ports on the Pi.** The agent listens on a Unix socket; the only
  network surface is the SSH the user already has. A new command must not
  introduce a listener reachable other than through the tunnel.
- **Secrets are encrypted in transit and at rest, and the CLI doesn't linger
  over them.** `rpi secrets send` encrypts before it leaves the developer's
  machine; the agent stores them age-encrypted; `rpi secrets ls` never
  transmits values, only keys and paths. A new command touching secret
  material follows the same shape — encrypt before sending, never echo a
  secret value back for display.
- **Destructive or hard-to-reverse operations require explicit confirmation.**
  `rpi rm`, `rpi agent update`, `rpi upgrade` all gate on a prompt or
  `--yes`. If a command can delete data, replace a running binary, or take a
  service offline, it needs the same gate — don't ship a destructive command
  that runs unattended by default.

## 6. Version skew is a first-class case, not an error path

The CLI and agent handshake on `connect` and the CLI gates commands against
the agent's advertised feature set (`crates/bin/src/compat.rs`). A CLI
built for a newer agent talking to an older one prints a specific banner
("agent does not support X; update the agent on the Pi") instead of a raw
404 or a panic. Any new route added to the agent needs the same
old-agent-compat handling on the client side (see step 7 of
`add-cli-command`) — the assumption that CLI and agent are always running
matched versions does not hold in this system, since the agent upgrades on
its own schedule via `rpi upgrade`.

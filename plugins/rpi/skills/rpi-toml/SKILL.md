---
name: rpi-toml
description: Use when creating, editing, validating, reviewing, or troubleshooting rpi.toml files for rpi deployments, including schema 1 fields, project/source/build/ingress/healthcheck/env/timeouts sections, Docker Compose service and port mapping, public hostname ingress, worker services, per-project deploy settings, and rpi.<env>.toml environment overlays ([environment] ttl/on_create, merge rules, ${...} interpolation).
---

# Rpi TOML

## Overview

Use this skill for `rpi.toml`, the project-level deployment config read by `rpi deploy`, `rpi deploy --cancel`, `rpi secrets send`, and `rpi secrets ls`. Keep config advice aligned with `crates/bin/src/cli/rpitoml.rs` and `README.md`.

## Minimal Shape

Public web service:

```toml
schema = 1

[project]
name = "example-web"

[source]
repo = "git@github.com:owner/example-web.git"
branch = "main"

[build]
compose = "docker-compose.yml"

[ingress]
hostname = "app.example.com"
service = "web"
port = 3000

[healthcheck]
path = "/health"
expect = "200"
timeout = "60s"

[secrets]
env = ".env"                     # optional, default ".env"
files = [                        # optional; recreated at the same paths on the Pi
  "certs/server.pem",
]
# file_mode = "0640"             # optional; default 0644 for files, 0600 for .env
```

Worker, bot, or internal service without public HTTP ingress:

```toml
schema = 1

[project]
name = "example-worker"

[source]
repo = "git@github.com:owner/example-worker.git"
branch = "main"

[ingress]
service = "app"
port = 3000
```

## Fields

| Field | Required | Default | Notes |
| --- | --- | --- | --- |
| `schema` | yes | none | Must be `1`. |
| `project.name` | yes | none | Compose project name and agent state key. |
| `source.repo` | yes | none | Git URL fetched by the Pi. |
| `source.branch` | no | `"main"` | Default ref for `rpi deploy`. |
| `build.compose` | no | `"docker-compose.yml"` | Compose file inside the project repo. |
| `ingress.service` | yes | none | Compose service managed by rpi. |
| `ingress.port` | yes | none | Container port, not host port. |
| `ingress.hostname` | no | none | Public hostname for Cloudflare/manual ingress. |
| `healthcheck.path` | no | none | HTTP path; omitted means TCP probe. |
| `healthcheck.expect` | no | none | `"2xx"`, `"3xx"`, or exact 3-digit code. |
| `healthcheck.timeout` | no | `"60s"` | Duration string or bare seconds. |
| `secrets.env` | no | `".env"` | Local env file read by `rpi secrets send`. |
| `secrets.files` | no | none | Optional list of local secret file paths (certs, keys), forward-slash relative, `..` rejected; recreated verbatim on the Pi on every deploy. |
| `secrets.file_mode` | no | none | `^0?[0-7]{3}$` (e.g. `"0640"`/`"640"`); owner read required, owner write optional, group/other read optional, nothing else. Overrides both the `0644` default for `secrets.files` and the `0600` default for `.env` at once — a container consuming a bind-mounted secret usually isn't the agent's uid, so the file mode is what decides whether it can read it. Requires an agent `>= 0.26.0` (`secret-modes` capability); the mode travels with the bundle, so it takes effect on the next `rpi secrets send`, not a `rpi deploy` that reuses an already-stored bundle. |
| `commands.<name>` | no | none | String (shell-word split, quotes only) or argv array. Name: `[a-z0-9][a-z0-9_-]*`. Registered at deploy, run via `rpi command`. |
| `timeouts.command` | no | `"600s"` | Budget for one `rpi command` run. |

Optional per-project stage timeouts:

```toml
[timeouts]
fetch = "3m"
build = "45m"
up = "2m"
```

Valid duration examples are `"60s"`, `"2m"`, and bare seconds such as `"120"`.

Optional one-off admin commands, run inside the service container with `rpi command`:

```toml
[commands]
create-invite = "node scripts/create-invite.js"
migrate = ["npx", "prisma", "migrate", "deploy"]
backup = "sh -c 'pg_dump mydb | gzip > /data/backup.gz'"
```

Commands run in the `[ingress].service` container by default. To run a command in a different compose service, use the table form:

```toml
[commands.create-invite]
run     = "node dist/scripts/create-invite.cjs"   # string or array, same rules as the shorthand
service = "server"                                 # optional compose service to exec into; defaults to [ingress].service
```

## Environment Overlays

An overlay file `rpi.<env>.toml` next to `rpi.toml` lets `rpi deploy --env <env>`
(and `rpi command`, `rpi secrets send/ls`, `rpi config show` with the same
`--env`/`--vars` flags) deploy a variant of the project — a shared `test`
environment, or a per-branch preview — under its own derived key, isolated
runtime state, and its own secrets bundle:

```text
myapp/
├── rpi.toml
├── rpi.test.toml
└── rpi.branch.toml
```

```toml
# rpi.branch.toml — parameterized preview overlay
[source]
branch = "${BRANCH_NAME}"

[ingress]
hostname = "${RPI_ENV_SLUG}.preview.example.com"

[environment]
ttl = "7d"          # optional; overlay's [environment] is the only place this is valid
on_create = "seed"  # optional; must name a command present in the merged [commands]
```

Rules:

- `<env>` must match `^[a-z][a-z0-9-]*$` and must not be one of the reserved
  words `show`, `ls`, `destroy`, `reset-data`.
- Every overlay field is optional; unknown fields are a parse error, stricter
  than the base file. `schema` and `[project]` are forbidden in an overlay —
  schema version and the project name are properties of the base file (the
  deployed name is always CLI-derived, see below).
- `[environment]` (`ttl`, `on_create`) is valid **only** in an overlay, never
  in the base `rpi.toml`. `ttl` uses the same duration format as `[timeouts]`.
  `on_create` must name a command that exists in the *merged* `[commands]`
  table (which the overlay may itself have replaced wholesale — see merge
  rules below), checked at resolve time, before any agent contact.
- **Merge rules** (base + overlay → deployed config): scalars replace
  field-wise (an overlay field present overwrites the base value; absent
  leaves the base value untouched); nested tables (`[ingress]`,
  `[healthcheck]`, `[timeouts]`, `[secrets]`) merge field-wise the same way;
  `[commands]` and array fields (`secrets.files`) replace **wholesale** — an
  overlay `[commands]` table drops every base command not repeated in it; an
  explicit empty string (`""`) on an optional field (e.g. `ingress.hostname`,
  `secrets.env`, `secrets.file_mode`) resets it to unset rather than being
  ignored.
- **Interpolation** (`${VAR}`) is allowed only in `source.branch` and
  `ingress.hostname` — anywhere else is a parse error, including inside
  `[commands]`, an argv array, or a command table's `service`. Supported
  variables: `BRANCH_NAME` (from `--vars BRANCH_NAME=<branch>`) and
  `RPI_ENV_SLUG`, derived from `BRANCH_NAME` on demand — only computed when
  actually referenced — by lower-casing, collapsing runs of non-`[a-z0-9]`
  characters to a single `-`, truncating to 30 characters, and trimming a
  trailing `-`. An overlay with no `${...}` reference rejects `--vars`
  ("not parameterized"); a parameterized overlay without
  `--vars BRANCH_NAME=...` is an error.
- **Key derivation**: the deployed `project.name` is always CLI-derived, never
  read from the overlay — `<base>--<env>` for a static overlay, or
  `<base>--<env>--<slug>` once `${RPI_ENV_SLUG}` was actually substituted.
  `--` in a project name is reserved for this; a base `rpi.toml` whose
  `project.name` contains `--` is rejected agent-side.
- If the base `rpi.toml` sets `[ingress].hostname`, the overlay must
  override it to a different value or clear it with `hostname = ""` —
  inheriting or repeating the base hostname is rejected (CLI resolve-time
  error, and a 409 from the agent as a second line of defense), since it
  would otherwise hijack the production route on the environment's first
  successful deploy.
- `rpi config show --env <env> [--vars ...]` prints the fully resolved
  configuration (base + overlay merged, `[environment]` appended) without
  contacting the agent — the fastest way to check what a deploy would send.

See `docs/architecture/flows/environments.md` for the full resolution,
deploy-time guard, `on_create`, and `rpi env`/TTL-reaper flow.

## Authoring Workflow

1. Identify the Compose service name and container port first.
2. Set `project.name` to a stable, unique deployment name; changing it creates a different deployed project state.
3. Set `source.repo` to a URL the Raspberry Pi can fetch, not just the developer machine.
4. Use `ingress.hostname` only when the service needs public HTTP routing.
5. Add `[secrets]` when the service needs an env file and/or secret files (certs, keys) delivered from the developer machine.
6. Add `[healthcheck]` when the service has an HTTP readiness endpoint; otherwise rely on the TCP probe.
7. Add `[timeouts]` only for project-specific overrides; prefer agent defaults for normal projects.

## Compose Compatibility

The agent writes an override mapping the allocated host port to `ingress.port`, roughly:

```yaml
services:
  web:
    ports:
      - "127.0.0.1:8000:3000"
```

Recommended Compose pattern:

```yaml
services:
  web:
    build:
      context: .
    expose:
      - "3000"
```

Avoid fixed host ports for the rpi-managed service:

```yaml
services:
  web:
    ports:
      - "127.0.0.1:3000:3000"
```

That can conflict with rpi's stable host port allocator.

For mutable runtime files, mount directories instead of individual files that may not exist in a fresh clone:

```yaml
services:
  app:
    environment:
      DATABASE_URL: file:///data/app.db
    volumes:
      - ./data:/data
```

## Validation Notes

`rpi.toml` is parsed by `crates/bin/src/cli/rpitoml.rs`:

- Unknown schema versions are rejected.
- Missing `[build]`, `[healthcheck]`, `[secrets]`, `[timeouts]`, and `[commands]` sections can fall back to defaults.
- `[env]` is rejected with a parse error pointing at `[secrets]`; it was replaced by `[secrets]` (`env` + `files`), a hard cutover with no fallback in `rpi.toml`.
- `[ingress]`, `[project]`, and `[source]` are required.
- Invalid healthcheck expectation values are rejected.
- Invalid duration strings in `[healthcheck].timeout` and `[timeouts]` are rejected.
- `secrets.file_mode` is validated in `RpiToml::validate_common()` (base and
  overlay alike) and, independently, again by the agent before it writes a
  secret to disk — a bad mode that somehow reaches the agent is a hard error
  (`DomainError::Invalid`), not a silently-widened file.
- An empty `[commands]` section, an empty argv, bad command names, and unbalanced quotes in a string command are all rejected by `crates/bin/src/cli/rpitoml.rs`.

When editing the parser or adding fields, update:

- `crates/bin/src/cli/rpitoml.rs`
- `crates/bin/src/cli/overlay.rs` (overlay schema, merge, and interpolation live here, separate from the base parser)
- `README.md`
- examples in this skill if the public config surface changes
- `docs/architecture/flows/environments.md` if overlay resolution behavior changes (see the `architecture-diagrams` skill)

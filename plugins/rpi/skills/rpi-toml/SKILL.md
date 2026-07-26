---
name: rpi-toml
description: Use when creating, editing, validating, reviewing, or troubleshooting rpi.toml files for rpi deployments, including schema 1 fields, project/source/build/ingress/healthcheck/env/timeouts sections, Docker Compose service and port mapping, public hostname ingress, worker services, per-project deploy settings, configuration variables (${NAME} user vars from --vars, ${git.*}/${env.*} resolver inputs, RPI_* runtime variables), and rpi.<env>.toml environment overlays ([environment] ttl/on_create, merge rules).
---

# Rpi TOML

## Overview

Use this skill for `rpi.toml`, the project-level deployment config read by `rpi deploy`, `rpi deploy --cancel`, `rpi command`, `rpi secrets send`, `rpi secrets ls`, `rpi config show`, and `rpi env destroy`/`reset-data`. Keep config advice aligned with `crates/bin/src/cli/rpitoml.rs` and `README.md`.

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

## Variables

Any string field of `rpi.toml` (and of an `rpi.<env>.toml` overlay) can carry
`${...}` references. There are three namespaces, told apart by syntax alone —
so nothing has to be looked up to know where a value comes from.

| Syntax | What it is | Where it comes from |
| --- | --- | --- |
| `${NAME}` | user variable | `--vars NAME=VALUE` on the command line |
| `${ns.field}` | resolver input | computed by the CLI while resolving |
| `RPI_*` | runtime variable | injected by the agent into containers — **never valid in a TOML file** |

```toml
# rpi.toml — variables work in the base file, with or without an overlay
[source]
branch = "${BRANCH_NAME}"                          # or "${git.branch}"

[build]
compose = "compose.${STAGE}.yml"

[commands]
tag = "sh -c 'echo built ${git.short_sha}'"
backup = "sh -c 'tar -C $${HOME} -czf /b.tgz .'"   # $${ => literal ${
```

```bash
rpi deploy --vars BRANCH_NAME=feature/login --vars STAGE=qa   # --env is not required
```

(`${env.name}`/`${env.slug}` are the exception: they describe a selected
environment, so they only resolve with `--env` — see the overlay example
below.)

**User variables (`${NAME}`).** Names must match `^[A-Z][A-Z0-9_]*$`; the name
is otherwise arbitrary (`BRANCH_NAME` is not special). `--vars` is repeatable,
everything after the first `=` is the value, an empty value is fine, and a
duplicate key is an error. Names starting with `RPI_` are refused. A `--vars`
key that no field references is an error naming that key — a typo cannot pass
silently. A reference with no matching value is likewise an error, listing
what *is* available.

**Resolver inputs (`${ns.field}`).** Exactly five, in exactly two namespaces:

| Input | Value |
| --- | --- |
| `${git.branch}` | current branch of the repository rpi is run from |
| `${git.sha}` | full 40-character sha of `HEAD` |
| `${git.short_sha}` | git's own abbreviation of `HEAD` |
| `${env.name}` | the `<env>` passed to `--env` (only with `--env`) |
| `${env.slug}` | `source.branch`, sanitized for use in DNS/keys (only with `--env`) |

`${git.*}` runs `git` lazily — only for the inputs a configuration actually
references — so a config that uses none still resolves outside a repository.
A detached `HEAD` makes `${git.branch}` a hard error naming `--vars` as the
workaround (it does *not* silently become `HEAD`); `${git.sha}` and
`${git.short_sha}` still work while detached. `${env.slug}` is the branch
lower-cased, every run of non-`[a-z0-9]` characters collapsed to one `-`,
truncated to 30 characters, trailing `-` trimmed; a branch that normalizes to
nothing is an error.

**Runtime variables (`RPI_*`).** These exist only inside containers and
exec'd processes; the agent injects them at deploy time. Writing `${RPI_...}`
in a TOML file is a dedicated error, and `${RPI_ENV_SLUG}` specifically
suggests `${env.slug}` — that rename is the one hard break. `RPI_` is also
rejected as a `--vars` name and as a secret key. `rpi config show` prints a
`[runtime]` block previewing the set (`RPI_HOST_PORT` and `RPI_COMMIT_SHA`
show as `<assigned by agent>`).

Rules:

- **Where it works**: every string field of `rpi.toml` and of an overlay,
  including `[commands]` in string form, in argv-array form, and a command
  table's `service`. Two exceptions: `[project].name` refuses any reference (the
  project key must stay static — it drives key derivation), and `schema`
  cannot carry one (it is a number in the base file and forbidden outright in
  an overlay).
- **Escaping**: `$${` renders a literal `${` and is inert as a reference —
  that is how a command keeps a shell variable of its own. `$$` is special
  only immediately before `{`; a lone `$`, `$$`, or `$5` is ordinary text.
- **Order**: `source.branch` is resolved first, `${env.slug}` is derived from
  the result, then everything else is resolved. `${env.slug}` inside
  `source.branch` is therefore a circular-reference error.
- **Validation runs after substitution**, so a substituted value still has to
  be valid — a raw branch name interpolated into `ingress.hostname` fails the
  DNS check (`feature/login` has a `/`), which is exactly why `${env.slug}`
  exists.

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
branch = "${BRANCH_NAME}"                          # or "${git.branch}"

[ingress]
hostname = "${env.slug}.preview.example.com"       # ${env.slug} => per-branch key

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
- **Variables** work the same in an overlay as in the base file (see the
  Variables section above); `${env.name}` and `${env.slug}` are available
  only when `--env` selected an environment. `--vars` is *not* tied to
  `--env` — it works against the base `rpi.toml` alone.
- **Key derivation**: the deployed `project.name` is always CLI-derived, never
  read from the overlay — `<base>--<env>`, or `<base>--<env>--<slug>` when
  either file references `${env.slug}`. The suffix hangs on that one
  reference and nothing else: using some other variable (`.env.${STAGE}`,
  say) leaves the key at `<base>--<env>`, so a shared stand does not turn
  into a per-branch environment by accident. A **computed `source.branch`**
  (any `${...}` in it) with no `${env.slug}` anywhere does print a warning
  naming the key you actually get — that combination means every branch
  deploying the environment lands on one shared key. A static branch never
  warns, however many other variables the files use.
  `--` in a project name is reserved for this; a base `rpi.toml` whose
  `project.name` contains `--` is rejected agent-side.
- If the base `rpi.toml` sets `[ingress].hostname`, the overlay must
  override it to a different value or clear it with `hostname = ""` —
  inheriting or repeating the base hostname is rejected (CLI resolve-time
  error, and a 409 from the agent as a second line of defense), since it
  would otherwise hijack the production route on the environment's first
  successful deploy.
- `rpi config show [--env <env>] [--vars ...]` prints the fully resolved
  configuration (base + overlay merged, `[environment]` appended, then a
  `[runtime]` preview of the `RPI_*` variables) without contacting the agent
  — the fastest way to check what a deploy would send.

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

The agent writes an override mapping the allocated host port to `ingress.port`, and gives every service of the stack the `RPI_*` runtime variables, roughly:

```yaml
services:
  web:                                   # the [ingress].service
    restart: unless-stopped
    ports:
      - "127.0.0.1:8000:3000"
    environment:                         # a mapping, so compose merges key-wise
      RPI_PROJECT: myapp--branch--feature-login
      RPI_BRANCH_NAME: feature/login
      RPI_HOST_PORT: "8000"
  worker:                                # every other service: environment only
    environment:
      RPI_PROJECT: myapp--branch--feature-login
```

Those names are readable from inside the container and interpolate in the project's own compose file (`${RPI_HOST_PORT}`), but they are never valid in `rpi.toml` itself. Injecting them needs an agent `>= 0.27.0` (the `runtime-vars` capability); an older agent still deploys, it just injects nothing.

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
- `crates/bin/src/cli/overlay.rs` (overlay schema, the merge, and the two-phase resolver live here, separate from the base parser)
- `crates/bin/src/cli/vars.rs` (namespaces, `--vars` parsing, `$${` escaping) and `crates/bin/src/cli/gitctx.rs` (the `${git.*}` inputs) if the variable surface changes
- `crates/domain/src/runtimevars.rs` if the `RPI_*` set changes, plus `render_runtime_preview` in `crates/bin/src/cli/commands.rs`, which mirrors it for `rpi config show`
- `README.md`
- examples in this skill if the public config surface changes
- `docs/architecture/flows/environments.md` if overlay resolution behavior changes, and `docs/architecture/flows/deploy.md` if `RPI_*` delivery changes (see the `architecture-diagrams` skill)

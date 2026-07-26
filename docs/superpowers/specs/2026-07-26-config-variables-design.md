# Configuration Variables — Design

Date: 2026-07-26
Status: approved, not implemented
Supersedes the variable model of `2026-07-24-environment-overlays-design.md`
(§ "Variables and interpolation" and § "Slug derivation"); everything else in
that spec stands.

## Goal

Replace the single-purpose variable system built for environment overlays with
a general one:

1. `--vars` accepts arbitrary variables usable anywhere in the configuration,
   not just `BRANCH_NAME` in two whitelisted fields.
2. A namespace of `RPI_*` variables describes the deployment to the code it
   deploys — available inside processes and containers.
3. `RPI_BRANCH_NAME` exists, carrying the branch the configuration resolved to.

## Why the current model cannot be extended in place

Today (`crates/bin/src/cli/overlay.rs`):

- `parse_vars` hard-rejects any name other than `BRANCH_NAME`.
- `${...}` is legal only in `source.branch` and `ingress.hostname`; anywhere
  else it is a parse error naming the field.
- `--vars` requires `--env`; the base `rpi.toml` is never parameterized.
- The project key gains a `--slug` suffix when the overlay used *any*
  variable — a proxy that only works while there is exactly one variable.
- Nothing reaches the agent: substitution happens entirely in the CLI before
  `ProjectConfig` is sent. Containers receive environment only from the
  secrets bundle written to `<workdir>/.env`.

Each of these is load-bearing for the others, so the change is a redesign of
the model rather than a widening of a whitelist.

## Variable model

Three disjoint namespaces, distinguishable by syntax alone.

| Class | Syntax | Charset | Source | Available in |
|---|---|---|---|---|
| User | `${NAME}` | `^[A-Z][A-Z0-9_]*$` | `--vars NAME=VALUE` | TOML only |
| Resolver input | `${ns.field}` | `^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$` | local machine / resolution | TOML only |
| Runtime | `RPI_*` | — | computed by the agent | process and container environment only |

The split is a category distinction, not a stylistic one. Resolver inputs are
*inputs* — facts about the machine running `rpi`. `RPI_*` are *outputs* —
facts about the deployment that resulted. Inside a container "the local
machine's current git branch" is meaningless, so inputs are never exported;
`RPI_BRANCH_NAME` is, and it is exactly the branch that got deployed.

Making the syntax differ prevents the category error by construction: nobody
seeing `${git.branch}` in a TOML file will look for `git.branch` in `printenv`.

### Resolver inputs

| Variable | Value |
|---|---|
| `git.branch` | current branch of the local repository |
| `git.sha` | full sha of local `HEAD` |
| `git.short_sha` | abbreviated sha of local `HEAD` |
| `env.name` | the `--env` value |
| `env.slug` | normalized effective `source.branch` (see below) |

All are computed lazily — `git` is only invoked when a reference to a `git.*`
variable is actually present. Without this, `rpi config show` would break
outside a git repository even for configurations that use no variables.

`${git.branch}` with a detached `HEAD` is a hard error naming the fix
(`pass the branch explicitly via --vars`). The alternatives are worse:
returning the literal `HEAD` silently produces the project key
`myapp--branch--head` and lets unrelated CI runs collide on one environment,
and falling back to `GITHUB_REF_NAME` hard-codes one CI vendor into the
resolver.

`env.slug` normalization is unchanged from the current `derive_slug`:
lower-case, collapse every run of characters outside `[a-z0-9]` to a single
`-`, truncate to 30 characters, trim a trailing `-`; a branch that normalizes
to nothing is an error.

### Substitution scope

Every string field of `rpi.toml` and `rpi.<env>.toml` **except** `schema` and
`[project].name`, where a `${` is an error. The project name determines the
registry key, the `base--env[--slug]` derivation and the production-key-hijack
check; a computed name would make all three unverifiable.

This includes `[commands]` in all four shapes (`Line`, `Argv`, `Table.run`,
`Table.service`). Substitution runs **before** `shlex` splitting, so a value
containing spaces or quotes affects argv splitting. This is documented
behavior, not an accident.

### Escaping

`$${...}` yields a literal `${...}`. `$$` is special only immediately before a
`{`; anywhere else it is two ordinary characters, so a shell command
containing `$$` (the PID) is unaffected.

This is required, not a nicety. The base `rpi.toml` never restricted `${...}`
in `[commands]`, so a command like `backup = "sh -c 'tar -C ${HOME} .'"`,
intended for the shell inside the container, works today. Without escaping it
would start failing with `unknown variable HOME`.

## Resolution pipeline

Two phases, forced by exactly one dependency: `env.slug` derives from
`source.branch`.

```
1. Parse rpi.toml and rpi.<env>.toml (raw; values not yet validated)
2. Phase 1 — substitute into source.branch of both files
   available: ${NAME}, ${git.*}, ${env.name}
3. effective branch = overlay.source.branch ?? base.source.branch
4. if ${env.slug} appears anywhere -> slug = normalize(effective branch)
5. Phase 2 — substitute into every remaining field of both files
   available: ${NAME}, ${git.*}, ${env.name}, ${env.slug}
6. apply_overlay -> key base--env[--slug] -> validate_common -> hostname-hijack check
```

`${env.slug}` inside `source.branch` is a hard error
(`circular reference - env.slug is derived from source.branch`).

Both phases run over the base file and the overlay alike, so a base `rpi.toml`
deployed without `--env` is parameterizable too. In that case `env.name` and
`env.slug` are undefined, and referencing either is the ordinary
`unknown variable` error — there is no environment for them to describe.

Deriving the slug from the resolved `source.branch` rather than from a
user-named variable means the rule reads "the slug is the normalized branch
being deployed" and stops depending on what the user happened to call their
variable.

### Rules removed

- `--vars` requires `--env`.
- "overlay is not parameterized — remove `--vars`".
- "parameterized overlay requires `--vars BRANCH_NAME=...`".
- `BRANCH_NAME`'s privileged status; it becomes an ordinary user variable.
- The `source.branch` / `ingress.hostname` field whitelist.

### Rule added

Every `--vars KEY` must be referenced at least once across the base file and
the overlay, or resolution fails naming the key. This preserves the typo
detection that the "not parameterized" check existed for.

### Key derivation

The project key gains its `--slug` suffix if and only if `${env.slug}` is
actually referenced. The old rule — any variable used — breaks the moment
variables are general: a shared stand declaring

```toml
# rpi.test.toml
[secrets]
env = ".env.${STAGE}"
```

uses a variable and would be turned into a per-branch ephemeral environment
demanding a branch name.

## Runtime variables

| Variable | Value | Absent when |
|---|---|---|
| `RPI_PROJECT` | resolved key (`myapp` / `myapp--branch--feature-login`) | never |
| `RPI_PROJECT_BASE` | base project name (`myapp`) | never |
| `RPI_ENV` | environment name (`branch`) | plain deploy |
| `RPI_ENV_SLUG` | slug (`feature-login`) | key has no slug |
| `RPI_BRANCH_NAME` | `source.branch` of the resolved configuration | never |
| `RPI_HOSTNAME` | merged `ingress.hostname` | no hostname configured |
| `RPI_HOST_PORT` | host port allocated by the agent | never |
| `RPI_COMMIT_SHA` | commit sha actually deployed | before the first successful deploy |

`RPI_SERVICE` and `RPI_PORT` are deliberately excluded: a container already
knows its own service name and its own listen port — that is not information
from outside.

An absent variable is **not exported at all** rather than exported empty, so
`${RPI_ENV:-prod}` works inside a container.

`RPI_BRANCH_NAME` is `ProjectConfig.branch`, always. A positional
`rpi deploy <git_ref>` does not change it: the variable means "the branch named
in the configuration". "What actually got deployed" is `RPI_COMMIT_SHA`.

### No protocol change

The whole catalog derives from state the agent already persists:

```
RPI_PROJECT       <- project.config.name
RPI_PROJECT_BASE  <- project.config.environment.base ?? config.name
RPI_ENV/_SLUG     <- project.config.environment.{env, slug}
RPI_BRANCH_NAME   <- project.config.branch
RPI_HOSTNAME      <- project.config.hostname
RPI_HOST_PORT     <- project.host_port
RPI_COMMIT_SHA    <- current fetch (during deploy) / registry (everywhere else)
```

So the deploy request payload is unchanged — no new field, no map on the wire,
no JSON column. (The `/v1/version` handshake does gain one capability string;
see "Old agent, new CLI".) One function on the agent —
`rpi_vars(&Project, Option<&str>) -> BTreeMap<String, String>` — is the single
source of truth, which is why the CLI and the agent cannot disagree about a
value: only the agent computes them.

The "run what was deployed, not what the local file currently says" property
that `commands` has comes for free, since the source is the same registry row.

### Registry

One new column, `last_commit_sha`, stamped by `mark_deploy_success` (today it
takes only `finished_at`). It exists so `rpi command` and `rpi restart` can see
`RPI_COMMIT_SHA` outside a deploy. The alternative — querying the last
successful row of the deployment history — needs no column but costs a join
per invocation; the column is O(1) and the value is single-writer.

## Delivery

### Process environment

`DockerComposeRuntime::compose()` sets the whole catalog alongside the existing
`BUILDKIT_PROGRESS=plain` (`crates/infrastructure/src/docker.rs:206`), covering
`build`, `up`, `down`, `exec` and `lifecycle`. Effect: `${RPI_*}` interpolates
inside the project's own `docker-compose.yml`.

Compose gives the shell environment higher precedence than `.env`, so this
never has to touch the secrets file.

### Override file

`overrides/<project>.yml` stops being single-service:

```yaml
# generated by pi - do not edit
services:
  web:                     # the public service
    restart: unless-stopped
    ports: ["127.0.0.1:8000:3000"]
    environment: {RPI_PROJECT: "...", ...}
  worker:                  # every other service: environment only
    environment: {RPI_PROJECT: "...", ...}
```

Service names come from `docker compose config --services` over
`compose_file` plus the repository's own `docker-compose.override.yml` — not
our override, which does not exist yet on a first deploy — run with the same
process environment. A failure fails the deploy at the `start` stage with the
command's own error; degrading silently would mean a missing environment
variable with no trace, and a compose file that cannot be parsed would fail
`build` moments later anyway.

Two details that are easy to get wrong:

- `environment` is emitted as a **mapping**, not a list, so Compose merges it
  key-wise and our override — last in the file chain — wins over a same-named
  key in the project's compose file.
- The file is produced by a YAML emitter (`serde_yaml`, already a workspace
  dependency), not by `format!`. Values now come from branch names and
  hostnames, and a `"` in one must not be able to corrupt the file. The
  current `override_yaml` is string concatenation.

### Exec

`rpi command` and `on_create` pass `docker compose exec -e RPI_X=...`. A
container started by an earlier deploy does not see refreshed values;
the exec'd process does.

### Pipeline order

`upsert` (yields the host port) → `fetch` (yields the sha) → secrets →
enumerate services → write override → `build` → `up`. Both late-bound values
are available before the override is written.

## Rejected: writing `RPI_*` into `<workdir>/.env`

That file is owned by the secrets writer, which replaces it wholesale, sets it
`0600`, masks its values in logs and lists its keys in `rpi secrets ls`.
Mixing non-secret variables in would make `secrets ls` lie and break
whole-bundle replace.

## CLI surface

- `--vars KEY=VALUE`, repeatable, arbitrary `KEY` matching
  `^[A-Z][A-Z0-9_]*$`, `RPI_` prefix rejected. No longer requires `--env`.
  Same commands as today: `deploy`, `deploy --cancel`, `command`,
  `secrets send`, `secrets ls`, `config show`, `env destroy`,
  `env reset-data`.
- `rpi env destroy --key <full-key>` and `rpi env reset-data --key <full-key>`,
  mutually exclusive with the `<env>` + `--vars` path. `rpi env ls` already
  prints the key.

  This exists because the new slug derivation reads `source.branch`, which
  lives in the overlay — so `destroy` now needs the overlay file, losing the
  documented property that an environment stays destroyable after its overlay
  is deleted. The explicit key restores it by another route.

  `--key` reads no configuration file at all: not the overlay, and not
  `rpi.toml` either. It is the escape hatch for a project directory that no
  longer resolves, so making it depend on any local file would defeat it. The
  key is validated for shape (`base--env[--slug]`) and, as today, typed back
  at the confirmation prompt unless `--yes` is passed.
- `rpi config show` additionally prints a synthetic `[runtime]` block with the
  locally derivable `RPI_*` and `<assigned by agent>` for `RPI_HOST_PORT` and
  `RPI_COMMIT_SHA`, following the precedent of the existing synthetic
  `[environment]` block.
- `rpi secrets send` rejects any key using the `RPI_` prefix, so it is never
  ambiguous which side wins.

## Error handling

Every error below is raised in the CLI before the agent is contacted, except
the last.

```
--vars: variable name 'x' must match ^[A-Z][A-Z0-9_]*$
--vars: the RPI_ prefix is reserved for runtime variables ('RPI_X')
--vars: duplicate variable 'X'
--vars: variable 'X' is never referenced in rpi.toml or rpi.branch.toml
source.branch: unknown variable 'X' (available: BRANCH_NAME, git.branch, env.name)
ingress.hostname: RPI_* variables exist only at runtime; did you mean ${env.slug}?
ingress.hostname: unknown namespace 'foo' (available: git, env)
ingress.hostname: unclosed ${...} in '...'
[project].name: ${...} is not allowed (the project key must be static)
source.branch: circular reference - env.slug is derived from source.branch
${git.branch}: HEAD is detached; pass the branch explicitly via --vars
${git.branch}: not a git repository
rpi secrets send: key 'RPI_FOO' uses the reserved RPI_ prefix
<agent> start: docker compose config --services failed: ...
```

## Breaking changes and migration

1. **`${RPI_ENV_SLUG}` in TOML becomes an error.** Loud, with the fix in the
   message. One edit per overlay: `${env.slug}`. No compatibility alias — an
   alias would permanently entrench the exact confusion this redesign removes,
   and environment overlays shipped in 0.24.0 two days before this spec.

2. **`${...}` in the base file's `[commands]` now interpolates.** Covered by
   `$${...}` escaping, but it needs a release-note line.

3. **The project key can change silently.** The old rule granted the `--slug`
   suffix for using any variable; the new one grants it only for `${env.slug}`.
   An overlay with `source.branch = "${BRANCH_NAME}"` and a hard-coded hostname
   used to produce `myapp--branch--feature-login` and will now produce
   `myapp--branch`, orphaning the old deployment.

   In practice this is close to unreachable: the production-hostname-hijack
   check forces a parameterized overlay to set its own hostname, and one
   hard-coded hostname shared by every branch is self-evidently broken. But
   "close to" is not "never", so resolution emits a **warning** — not an
   error — when `--env` is set, `--vars` were passed, and `${env.slug}` appears
   nowhere, stating that the key will carry no slug suffix.

### Old agent, new CLI

The protocol does not change, so the deploy succeeds — but the container gets
no `RPI_*`, and rpi cannot detect the problem, because the configuration can no
longer express a dependency on them. A hard gate would break working deploys
over a dependency that may not exist.

So: a new `Feature::RuntimeVars` (capability `runtime-vars`, since `0.27.0`)
with `Policy::Degradable` — the one-shot warning banner. This is the first
feature to use `Degradable`, which `crates/bin/src/compat.rs:11` has carried as
a forward contract since 2026-07-12 without a consumer.

## Testing

Resolver unit tests: substitution in every field shape; `$$` escaping; phase
ordering; slug from the merged branch; circular reference; unreferenced
variable; `RPI_*` in TOML; the `[project].name` ban; namespace charset;
unknown namespace; detached `HEAD`; no git repository.

`rpi_vars` unit tests: full map for an environment deploy and for a plain one;
absent variables absent rather than empty.

Override unit tests: multi-service shape; the public service keeps `ports` and
`restart`; `environment` emitted as a mapping; a value containing `"` survives
a YAML round-trip.

Docker adapter unit tests: `RPI_*` present in the process environment; the
argument shape of `config --services`; `-e` flags on `exec`.

E2e (per this repository's default of covering features with e2e scenarios):
deploy an overlay using `${git.branch}` and `${env.slug}`;
`rpi command -- printenv RPI_BRANCH_NAME` returns the expected value; a
non-public worker service also sees the variables.

## Documentation to update

- `docs/architecture/flows/environments.md` — walkthrough steps 2 and 4, and
  the source anchors.
- `docs/architecture/flows/deploy.md` — service enumeration and the override
  write.
- The `rpi-toml` and `rpi-cli` skills.

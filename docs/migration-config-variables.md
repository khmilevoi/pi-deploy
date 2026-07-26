# Migration: Configuration Variables Reach Every Field (v0.27)

Before this release, `${...}` interpolation existed only inside an
environment overlay (`rpi.<env>.toml`), and only in two fields:
`source.branch` and `ingress.hostname`. The only variable was
`${BRANCH_NAME}`, supplied via `--vars`, and `--vars` required `--env`.
Everything else — the base `rpi.toml`, `[commands]`, `[secrets]`,
`[healthcheck]`, `${RPI_ENV_SLUG}` — was either passed through untouched or a
whitelist error.

v0.27 replaces that with one interpolation pass over every string leaf of
`rpi.toml` and `rpi.<env>.toml` (except `schema` and `[project].name`, which
stay static), three disjoint variable namespaces — `${NAME}` user variables,
`${ns.field}` resolver inputs, `RPI_*` runtime-only variables — and `$${` as
the escape for a literal `${`.

## What To Do

### 1. Escape any shell `${VAR}` written inside `[commands]`

`[commands]` used to be passed through untouched, so a shell variable
expansion meant for the container's own shell worked by accident:

```toml
[commands]
echo-lines = ["sh", "-c", "long=; long=${long}x; echo $long"]
```

It now resolves as a configuration variable and fails, because `long` is
neither `${NAME}` (uppercase) nor `${ns.field}` (dotted lowercase):

```
▸ commands.echo-lines.2: invalid variable name 'long' (use ${NAME} for a --vars variable or ${ns.field} for a resolver input)
```

Escape it with `$${`, which resolves to a literal `${` and reaches the
container's shell exactly as before:

```toml
[commands]
echo-lines = ["sh", "-c", "long=; long=$${long}x; echo $long"]
```

`$` on its own, or followed by anything other than `{`, is untouched — `$i`,
`$((i+1))`, `$$` (a shell PID) need no change; only `${...}` is special. A
shell variable that happens to be all-uppercase (`${HOME}`, `${PATH}`) hits a
different message — `unknown variable 'HOME' (available: ...)`, since it
looks like a valid `${NAME}` reference — but the fix is the same: `$${HOME}`.

### 2. Replace `${RPI_ENV_SLUG}` and any other `RPI_*` reference

`${RPI_ENV_SLUG}` (and any other `RPI_*` name) inside a TOML file is now a
hard error — `RPI_*` exists only in the environment of containers and
exec'd processes, injected by the agent, never inside a configuration file:

```
▸ ingress.hostname: RPI_* variables exist only at runtime; did you mean ${env.slug}?
```

Replace it with the resolver input `${env.slug}`, which is the same value
(the normalized, effective `source.branch`):

```diff
 [ingress]
-hostname = "${RPI_ENV_SLUG}.preview.example.com"
+hostname = "${env.slug}.preview.example.com"
```

### 3. Check whether an environment's per-branch key suffix survives

Previously, an overlay got the `--slug` suffix on its project key
(`myapp--branch--feature-login`) whenever it used *any* variable in
`source.branch` or `ingress.hostname`. Now the suffix is granted if and only
if `${env.slug}` is referenced anywhere in the merged configuration:

```toml
# rpi.branch.toml
[source]
branch = "${BRANCH_NAME}"

[ingress]
hostname = "preview.example.com"   # fixed, no ${env.slug}
```

Before: keyed `myapp--branch--feature-login` per branch. After: keyed
`myapp--branch` — every branch now shares one deployment key, and the
previously deployed per-branch environments become orphaned (a plain
`rpi deploy --env branch` always resolves the *current* key, so it no longer
targets them). Resolution warns instead of failing:

```
▸ rpi.branch.toml: source.branch is computed but nothing references ${env.slug}, so the key stays 'myapp--branch' - every branch deploying this environment shares it
```

If you want the old per-branch behavior back, add a `${env.slug}` reference
to the overlay (commonly in `ingress.hostname` or `secrets.env`). If the
shared key is what you want, clean up the now-orphaned environments — list
them with `rpi env ls` and remove each stale one with `rpi env destroy
--full-key <base--env--slug>`.

### 4. `--vars` no longer requires `--env` or `BRANCH_NAME`

`--vars KEY=VALUE` now works against the base `rpi.toml` alone, and accepts
any name matching `^[A-Z][A-Z0-9_]*$` (the `RPI_` prefix stays reserved for
runtime variables). `BRANCH_NAME` is an ordinary user variable now, not a
privileged one.

### 5. An unreferenced `--vars` key is now an error

A `--vars KEY=...` that never appears as `${KEY}` anywhere in the base file
or the overlay now fails resolution naming the key, instead of silently
being ignored:

```
▸ --vars: variable 'TYPO' is never referenced in rpi.toml or rpi.branch.toml
```

### 6. `rpi env destroy` / `rpi env reset-data` resolve the overlay

Because the slug now derives from `source.branch`, which lives in the
overlay, targeting `<env>` reads `rpi.toml` and `rpi.<env>.toml` the same way
`rpi deploy --env <env>` does — pass `--vars` if the overlay needs it. If the
overlay no longer resolves (deleted, or its variables gone), target the
environment directly with the exact key `rpi env ls` prints:

```bash
rpi env destroy --full-key myapp--branch--feature-login
```

`--full-key` reads no configuration file at all: not the overlay, and not
`rpi.toml` either.

## Rollback

Pin the previous release if you are not ready for the change:

```bash
npm install -g rpi-deploy@0.26.1
```

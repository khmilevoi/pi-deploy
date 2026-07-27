# Per-project storage layout (idea)

Status: potential feature; not an implementation specification.

## Goal

Group everything the agent stores about one project under a single directory,
so a project and all of its environments can be inspected, backed up or removed
as one unit:

```text
/var/lib/rpi/projects/myboard/
  envs/
    default/
    dev/
    branch/
  secrets/
```

Today the layout is one directory *per concern*, with the project key repeated
in each of them.

## Current layout

Documented in `docs/architecture/storage.md`; the paths themselves are built in
three infrastructure modules:

```text
/var/lib/rpi/
  state.db                          # projects, deployments, applied_migrations
  secret.key
  known_hosts
  workdirs/<key>/                   # crates/infrastructure/src/git.rs (roots, workdir())
  keys/<key>/id_ed25519             # crates/infrastructure/src/git.rs
  overrides/<key>.yml               # crates/infrastructure/src/overrides.rs
  secrets/<key>.secrets.age         # crates/infrastructure/src/secrets.rs (bundle_path)
  secrets/<key>.env.age             # legacy, read as fallback, dropped on next save
  secrets/groups/<base>/<name>.age  # crates/infrastructure/src/secrets.rs (group_path)
```

`<key>` is the derived project key: `myboard`, `myboard--dev`, or
`myboard--branch--<slug>`. Secret groups are the one place that already nests by
base project, which is a useful precedent for the shape proposed here.

## What is cheap

Path construction is confined to those three modules — roughly six functions in
total. The application layer (`deploy.rs`, `command.rs`, `lifecycle.rs`,
`remove.rs`, `environments.rs`, `secrets.rs`) reaches the filesystem only
through ports and never sees a path, so it does not change as long as the port
signatures stay the same.

The registry already stores the decomposition: `projects.env_base`,
`projects.env_name` and `projects.env_slug` are columns
(`crates/infrastructure/src/sqlite.rs`). The agent therefore does not have to
recover structure by string surgery on the key — the authoritative
`(base, env, slug)` triple is available where the paths are built.

## What is expensive

**Where the decomposition enters the store.** Below the CLI the key is an opaque
string. Either split it on `--` inside infrastructure — cheap, but it re-derives
structure from a string, and that fragility is precisely why `--` is rejected in
a project name — or thread a typed `ProjectKey { base, env, slug }` through the
ports, which touches every `workdir(&str)` signature and its test doubles
(~15 sites). The second is the honest version and should be costed as such.

**Migrating existing boards.** `workdirs/*`, `overrides/*.yml`, `secrets/*.age`,
`secrets/groups/*` and `keys/*` all have to move into the new tree,
idempotently and resumably. This cannot be skipped: an agent built for the new
layout finds no projects under the old one. The mechanism already exists —
`rpi agent migrate` with its ledger and `disruptive` flag (precedent:
`nginx-to-caddy`) — so the work is the migration itself, not the machinery. This
is the main risk in the whole change.

**Trailing references.** ~150 mentions of `workdirs` / `/var/lib/rpi` across the
crates (mostly tests), `docs/architecture/storage.md` plus the `agent-setup`,
`secrets` and `observability` flows, e2e scenarios that assert on paths,
`rpi agent uninstall --purge`, and `rpi doctor`'s checks. Directory permissions
(`/var/lib/rpi` `0750`) and the secret file modes must carry over to the new
directories rather than being re-created with defaults.

## Explicit non-goal: renaming the Compose project

`docker compose -p <key>` (`crates/infrastructure/src/docker.rs`) names
containers, volumes and networks from the project key, not from any filesystem
path. Changing the on-disk layout leaves all of them untouched, and it must stay
that way: renaming a Compose project orphans its named volumes, which is data
loss, not a refactor. "Everything about myboard in one place" therefore stops at
the agent's own storage. Bringing volumes under a per-project naming scheme is a
separate, genuinely destructive migration that would have to move volume
contents explicitly.

## Open questions (decide before writing code)

1. **Is `secrets/` at the project level a change of layout or of semantics?**
   Today every key has its own bundle — `myboard--dev` can hold different values
   from `myboard` — and only *groups* are shared across a project. A single
   `secrets/` per project reads as "one set of secrets for all environments",
   which removes per-environment values. If the intent is instead "the bundles
   of all environments live under the project", the shape is
   `projects/<base>/secrets/<env>.age` and nothing about the merge semantics
   moves. These are very different amounts of work, and the second one is
   compatible with the layered model shipped in 0.27.0.
2. **Where does the slug go?** Keys like `myboard--branch--featureone` exist, but
   the proposed tree has no level for the slug: `envs/branch--featureone/` or
   `envs/branch/featureone/`.
3. **Is the base project `envs/default/`?** `default` is not a reserved
   environment name today (only `show`, `ls`, `destroy`, `reset-data` are), so
   `rpi.default.toml` is a legal overlay and would collide with a directory that
   means "the base project". Either reserve the name or keep the base alongside
   `envs/` rather than inside it.
4. **What does `rpi rm <base>` remove?** Nesting makes "delete the project
   directory" trivially easy to write and far more destructive than today's
   behaviour, where `rpi env destroy` leaves groups alone and `rpi rm` of a base
   project drops them.

## Rough size

The layout alone, with the key split on `--` and a migration, is a few days.
With a typed key threaded through the ports, the migration, the architecture and
flow docs, and the e2e scenarios that hard-code paths, it is closer to one to
two weeks — and question 1 above can move most of that work from paths to secret
layering, so it should be answered first.

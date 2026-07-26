# Secret Groups — Design

Date: 2026-07-26
Status: approved design; extends the per-project secrets model introduced in
`docs/superpowers/specs/2026-07-25-secret-file-modes-design.md` and the
environment model in `docs/superpowers/specs/2026-07-24-environment-overlays-design.md`.

## Goal

Store a project's secrets as named, reusable **groups** on the agent, and let
a deploy attach groups declaratively. A group's contents survive the
environments that consume them, so deploying a new branch preview stops
requiring a fresh secrets upload.

## The problem

Secrets are stored under the *deploy key*: `<data_dir>/secrets/<key>.secrets.age`
(`crates/infrastructure/src/secrets.rs`), and the deploy stage loads them by
`config.name` (`crates/application/src/deploy.rs`). For a base project the key
is stable, so secrets persist between deploys. For an environment overlay the
key is derived — `myapp--test`, or `myapp--branch--<slug>` for a parameterized
overlay (`crates/bin/src/cli/overlay.rs`, `derive_key`). Therefore:

- every new branch produces a new slug, a new key, and an empty bundle, so
  `rpi secrets send` must run again before the first deploy of that branch;
- the TTL reaper deletes the bundle together with the environment, so
  recreating the same branch later requires another upload;
- the only source of truth is the operator's local `.env` file — there is
  nothing on the agent a new environment could attach to.

## Design summary

A **secret group** is a named, addressable set of secret objects (environment
variables and files) owned by a base project. Groups live on the agent, which
is the source of truth. A project declares which groups it attaches in
`[secrets].groups`; at deploy time the declared groups are merged in the
declared order and the deploy key's own implicit group is layered on top.

Three properties carry the design:

1. **Scope is the base project.** A group is addressed as `<base>/<name>`.
   Cross-project sharing is deliberately not offered (see Non-goals): a
   global namespace would let any project's `rpi.toml` name another project's
   group and have the agent hand those values to its own container.
2. **The agent is authoritative, and overwrites are conditional.** A group
   carries a monotonic revision. `rpi secrets push` sends the revision it
   expects to overwrite; a group that moved on rejects the write. Values never
   leave the agent — only names, sizes and digests do.
3. **Layering makes the specific win.** Declared groups apply in order, then
   the deploy key's implicit group. This is how Render (service vars over
   group vars) and Vercel (branch vars over Preview vars) resolve the same
   question, and it doubles as the per-branch override mechanism.

### Why not the alternatives

- **Key secrets by `(base, env-name)` instead of by deploy key.** Much smaller
  change and it fixes the reported pain, but one set cannot be reused across
  two environments, environments cannot compose two sets, and the set's
  identity is implicitly tied to the overlay's filename — renaming the overlay
  would silently detach the secrets.
- **A separate local secrets file plus an external vault (the Kamal model).**
  Kamal's `.kamal/secrets` exists because Kamal has no server-side store and
  must keep values out of git. Our agent already has an encrypted vault and
  `rpi.toml` already holds paths rather than values, so a second local file
  would solve a problem we do not have — and it would reintroduce the very
  pain being fixed, since values would again travel from a workstation on
  every deploy.

## Data model

Group contents reuse the existing `SecretsBundle` (vars, files, `file_mode`)
so masking, limits and the workdir writer keep one code path:

```rust
/// Contents of one group plus the revision they were stored at.
pub struct SecretGroup {
    pub objects: SecretsBundle,
    /// Monotonic; 0 means the group does not exist.
    pub revision: u64,
}

/// What a caller addresses.
pub enum GroupRef {
    /// Declared group owned by a base project.
    Named { base: String, name: String },
    /// The implicit group of a single deploy key.
    Key(String),
}

/// Metadata only — never values.
pub struct GroupHead {
    pub revision: u64,
    /// var name -> digest
    pub vars: BTreeMap<String, String>,
    /// file path -> (size, digest)
    pub files: BTreeMap<String, (u64, String)>,
    pub file_mode: Option<u32>,
}

pub struct GroupSummary {
    pub name: String,
    pub revision: u64,
    pub keys: usize,
    pub files: usize,
    pub bytes: u64,
    pub updated_at: i64,
}
```

`GroupSummary` deliberately carries no "attached by" field: the store sees
files, not the registry. The `group ls` use-case joins the store's summaries
with `ProjectRepository`, which is also the only place that can answer
"which registered projects declare this group" for the `group rm` guard.

`SecretStore` becomes addressable:

```rust
async fn load(&self, r: &GroupRef) -> Result<SecretGroup, DomainError>;
async fn head(&self, r: &GroupRef) -> Result<GroupHead, DomainError>;
async fn save(
    &self,
    r: &GroupRef,
    objects: &SecretsBundle,
    expected: Option<u64>,
) -> Result<u64, DomainError>;
async fn remove(&self, r: &GroupRef) -> Result<(), DomainError>;
async fn list(&self, base: &str) -> Result<Vec<GroupSummary>, DomainError>;
```

`save` semantics: `expected: Some(n)` writes only when the current revision is
exactly `n` — a first write sends `Some(0)`, because an absent group reads as
revision 0. A mismatch is `DomainError::Conflict`. `expected: None` is the
unconditional write behind `--force`. On success the stored revision is the
current revision plus one — including a forced write, which must not reset the
counter — and the new value is returned.

`load` on an absent group returns an empty bundle at revision 0, matching
today's behavior for a project with no secrets.

### Digests

A digest is the first 16 hex characters of SHA-256 over the raw value bytes
(64 bits — enough against accidental collision). `sha2` is already in the
dependency tree via `age`; it becomes a direct dependency.

A digest is a fingerprint for comparison, not a hiding mechanism: for a
low-entropy value (`true`, `production`, a short PIN) it is trivially
recovered by brute force. Therefore `ls`, `diff` and the group endpoints
require the same authorization as a deploy — which is not a new concession,
since anyone who can deploy can already deploy code that prints secrets.

## Storage layout

```
<data_dir>/secrets/
  secret.key                    # agent identity, unchanged
  <deploy-key>.secrets.age      # implicit group of one deploy key — path unchanged
  groups/<base>/<group>.age     # declared groups
```

No migration runs. `StoredBundle` gains `#[serde(default)] revision: Option<u64>`,
so bundles written before this change load as revision 0 and the first
conditional push against them succeeds. The legacy `<key>.env.age` fallback in
`secrets.rs` is untouched.

Because declared groups live under their own directory, a group name can never
collide with a deploy key on disk, so no group names need reserving. Group
names are validated as `^[a-z][a-z0-9-]*$` (the rule already used for
environment names) with a 40-character limit; `base` is validated by the
existing `validated_project`.

## Attachment and layering

`[secrets].groups` is a new array in `rpi.toml` and in overlays. It merges by
the existing array rule — replaced wholesale, never concatenated — so an
overlay states the complete list it wants:

```toml
# rpi.toml
[secrets]
env = ".env"
groups = ["common"]

# rpi.branch.toml
[secrets]
env = ".env.preview"
groups = ["common", "preview"]
```

`ProjectConfig` gains `secret_groups: Vec<String>`, persisted in a new
nullable registry column; empty means today's behavior exactly.

The deploy secrets stage:

1. for each declared name, in declared order, `load(Named { base, name })`,
   where `base` is `config.environment.base` for an environment and
   `config.name` for a base project;
2. then `load(Key(config.name))` — the implicit group, always the top layer;
3. merge per object: a later layer replaces an earlier one entry by entry (by
   variable name, and by file path for files);
4. resolve `file_mode` from the last layer that sets one, else the defaults
   (0644 for secret files, 0600 for the injected `.env`);
5. arm masking on the **merged** bundle, so values contributed by any layer
   are masked in `up` output;
6. log provenance:
   `secrets injected (5 keys, 2 files; groups: common@r3, preview@r7, key@r2)`.

A declared group that is missing or empty fails the deploy with
`DomainError::NotFound`. An application started without its secrets breaks
later and less legibly than a deploy that refuses to start.

Limits keep today's constants: 1 MiB per file and 8 MiB per group
(`MAX_SECRET_FILE_BYTES`, `MAX_SECRETS_BUNDLE_BYTES`). The merged set is
checked against the 8 MiB ceiling too, and the error names the contributing
layers.

## CLI surface

| Command | Behavior |
| --- | --- |
| `rpi secrets push [--group <g>] [--merge] [--force] [--apply]` | Without `--group`, targets the resolved deploy key's implicit group (today's `send`). Reads `[secrets].env` + `[secrets].files` from the resolved config, prints the diff by name, writes with the expected revision. |
| `rpi secrets ls [--group <g>]` | Without `--group`, the **effective** view for the resolved key: every object with the layer it came from, shadowed entries marked. With `--group`, that group alone: revision, names, digests, sizes. |
| `rpi secrets diff [--group <g>]` | Compares local files against the agent by digest. |
| `rpi secrets group ls` | Groups of the base project: name, revision, counts, size, `attached_by`. |
| `rpi secrets group rm <name>` | Deletes a group. Refuses when a registered project declares it, unless `--force`. |
| `rpi secrets send` | Deprecated alias for `push` without `--group`; prints a deprecation notice. |

`--env` / `--vars` behave as they do today on every command that resolves the
project from `./rpi.toml`. The `group ls` / `group rm` sub-noun shape follows
`rpi env ls` / `rpi env rm` rather than a `--group` flag, so listing and
deletion read like the rest of the CLI.

Every group-addressing command needs the **base** name, not the resolved
project key. With `--env` the resolved `project.name` is the derived key
(`myapp--branch--login`), so `base` comes from the resolution's
`EnvSelection::base`; without `--env` it is `rpitoml.project.name`. Using the
derived key here would address a group directory that no project owns, so this
is stated as an explicit rule rather than left to the call site.

`push` writes the full contents of the group and deletes objects that the
local sources no longer contain; `--merge` upserts without deleting. Both
modes are conditional on the expected revision — `--merge` weakens what is
written, not the guard — and both print the diff before writing, so a full
replace is never blind.

`diff` without `--group` compares the local sources against the resolved
deploy key's implicit group, mirroring `push`'s default target.

`--apply` applies to a single project: the one resolved from the current
config. Other projects that declare the group are listed as unaffected. A
fan-out that restarts every attached environment from one command is too
abrupt a default; redeploy or apply per project instead.

## Wire protocol and agent changes

New endpoints:

- `PUT /v1/projects/{base}/secret-groups/{group}` — body carries `vars`,
  `files`, `file_mode`, `expected_revision`, `merge`. `409 Conflict` when the
  expected revision does not match, with the current revision and the
  differing object names in the error body.
- `GET /v1/projects/{base}/secret-groups/{group}` — revision, digests, sizes.
- `GET /v1/projects/{base}/secret-groups` — list of `GroupSummary`.
- `DELETE /v1/projects/{base}/secret-groups/{group}`.

Existing endpoints keep their shape: `PUT /v1/projects/{key}/secrets` gains an
optional `expected_revision`, and `GET /v1/projects/{key}/secrets` gains the
effective view's layer information. No response, on any endpoint, contains a
secret value.

Compatibility: a new `Feature::SecretGroups` with minimum agent version
`0.27.0`. Group commands and a non-empty `[secrets].groups` are gated hard.
`push` without `--group` must keep working against a 0.26 agent, where the
overwrite guard is unavailable — the CLI warns rather than staying quiet,
because a silently disabled guard is worse than an absent one.

## Lifecycle

- `rpi env destroy` and the TTL reaper remove only `<key>.secrets.age`.
  Declared groups survive; this is the point of the feature — recreate the
  branch, deploy, and the group is still there.
- `rpi rm <base>` removes `groups/<base>/` along with the project. The
  confirmation text gains the group count, and any still-registered
  environments of that base are listed, because their next deploy will fail
  with the missing-group error. Loud beats silent.
- `rpi secrets group rm <name>` refuses while a registered project declares
  the group, unless `--force`.

## Error handling

| Situation | Behavior |
| --- | --- |
| Declared group missing or empty at deploy | `NotFound`, naming the group and the base; deploy fails at the secrets stage |
| Stale revision on `push` | `409`; CLI prints which object names differ and exits without writing; `--force` overrides |
| Invalid group name | Rejected at `rpi.toml` parse time and again server-side, same message on both sides |
| Merged set over 8 MiB | `Invalid`, naming the contributing layers |
| `group rm` of an attached group | `Conflict`, listing the projects that declare it |
| Group commands against a pre-0.27.0 agent | Gated with the standard feature-unavailable error |
| `push` without `--group` against a pre-0.27.0 agent | Proceeds, warning that the overwrite guard is unavailable |

## Safety invariants

1. No endpoint and no CLI output ever emits a secret value; masking is armed
   on the merged bundle before any container output is streamed.
2. A group is reachable only under its owning base project's path. There is no
   syntax that names another project's group.
3. A full-replace `push` cannot silently discard another writer's change: the
   write is conditional on the revision, and bypassing that requires
   `--force`.
4. A deploy either has every declared group or does not start.
5. Existing behavior is unchanged when `[secrets].groups` is absent, including
   on-disk paths, so a downgrade after an upgrade still reads its bundles.

## Testing

Unit:

- layer precedence, per object, including a file path shadowed by a later
  layer, and `file_mode` resolved from the last layer that sets one;
- revision conflict on `save`, and `expected: None` bypassing it;
- digest stability and the absence of values in `GroupHead`;
- group-name validation, matched between the `rpi.toml` parser and the agent;
- effective-view provenance, including shadowed entries;
- merged-set size ceiling with the layer names in the error.

Store:

- save/load round-trip carrying the revision;
- absent group loads as revision 0;
- a bundle written before this change loads as revision 0 and accepts a first
  conditional push;
- group files are 0600 and are not plaintext on disk.

HTTP:

- `409` on a stale revision, with current revision in the body;
- no `GET` response contains a value;
- `GET /secret-groups` for one base never lists another base's groups;
- `DELETE` of an attached group is a conflict.

E2E (this repo includes e2e scenarios for features by default):

- push a group, deploy branch environment A, deploy branch environment B —
  both receive the secrets with no second upload;
- rotate a value with `push`, redeploy, the new value is live;
- `rpi env destroy` the environment, redeploy it, the group is still attached;
- a deploy declaring an absent group fails at the secrets stage with the
  group named.

## Implementation phases (one spec, three phases)

1. **Addressable store.** `GroupRef`, `SecretGroup`, revisions in
   `StoredBundle`, the new `SecretStore` trait and its `EncryptedFileStore`
   implementation, digests. No CLI or protocol change yet; the existing
   per-key path routes through `GroupRef::Key`.
2. **Groups end to end.** `[secrets].groups` in the parser and overlay merge,
   `secret_groups` in `ProjectConfig` and the registry, layered injection at
   deploy, the four group endpoints, `push` / `ls` / `diff` /
   `group ls` / `group rm`, the compat feature, `send` as a deprecated alias.
3. **Conditional writes and provenance.** `expected_revision` on both write
   paths, the `409` path with differing names, the effective view with layers,
   and the pre-0.27.0 warning.

Documentation lands with the behavior in each phase:
`docs/architecture/flows/secrets.md` (layers, conditional writes),
`docs/architecture/storage.md` (the new directory),
`docs/architecture/flows/environments.md` (its claim that a fresh environment
has no secrets until they are sent explicitly stops being true — the property
that a production bundle is never copied still holds and should be restated),
plus the `rpi-toml` and `rpi-cli` skills and the compatibility table.

## Non-goals

- Cross-project or agent-global groups, and any `shared/` namespace.
- Group inheritance (a group extending another), as Doppler branch configs do.
  Layering at attachment time covers the same need without a second mechanism.
- Pinning a group revision to a deploy for replay. Fly, Vercel, Render and
  Railway all resolve at deploy time; only ECS pins versions, and the
  reproducibility it buys does not pay for the complexity here.
- Per-file `file_mode`. The mode stays a per-set scalar.
- Reading values back from the agent, in any form, including a `pull`.
- Fan-out `--apply` across every project attached to a group.
- External secret managers (1Password, Doppler, Vault) as group sources.
- A UI or any interactive editor for group contents.

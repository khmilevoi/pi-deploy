# Secret File Modes — Design

Date: 2026-07-25
Status: approved design. Supersedes the "Права/exec-биты файлов. Всё пишется
0600, каталоги 0700" YAGNI item in
`docs/superpowers/specs/2026-07-07-secret-files-design.md` §1.

## Goal

Make secret files materialized by `rpi` readable by the container that
consumes them, without weakening the protection of the plaintext secrets
sitting in a checkout on the Pi.

## The defect

A widget deployed to `board-dev.iiskelo.com` failed with
`EACCES: permission denied, open '/run/secrets/passport-checker_series'`
even though the secret was delivered correctly and its contents were valid.

The chain:

- `FsSecretsWriter` writes every secret file through
  `fsutil::write_private_atomic` (`crates/infrastructure/src/secretsfile.rs:134`),
  which hardcodes mode `0600` (`crates/infrastructure/src/fsutil.rs:16`).
  Files land owned by the agent's uid.
- The consuming container runs as its image's own user (uid 1000 for
  `node`), which is neither the owner nor a group member. At `0600` there
  are no bits left for anyone else.
- Compose cannot fix this on its side. Outside Swarm, a `file:`-sourced
  secret is bind-mounted verbatim; per Docker's Compose reference, `uid`,
  `gid` and `mode` "are only implemented in Docker Compose when the secret
  source is `environment`; they are silently ignored for `file` sources due
  to bind-mount limitations." The host's ownership and mode are therefore
  the only thing that decides readability.
- `group_add` does not help either: at `0600` the group has no read bit to
  grant.

This is not a bug in the writer. The secret-files design (2026-07-07)
explicitly deferred file modes and assumed "защита — права файлов и владение
`rpi-agent`" — a threat model in which the consumer of a secret file runs as
the agent. For a bind-mounted compose secret the consumer is a container
process with an unrelated uid, and nothing in `rpi.toml` can express that.
So the defect is a missing capability, and it hits every project that uses
`secrets: file:` with a non-root image.

### Correction to a common assumption

"The workdir tree is closed to outsiders anyway, so `0600` on individual
files adds nothing" is false in the current code:

- `/var/lib/rpi` is created with `install -d -o rpi-agent -g rpi-agent`
  (`crates/bin/src/agent/setup.rs:200,410`) — no `-m`, so `0755`.
- `workdirs/` comes from `create_dir_all` (`crates/infrastructure/src/git.rs:106`)
  and the per-project workdir from `git clone` — both `0755`.
- Only directories that `FsSecretsWriter` itself creates are `0700`
  (`crates/infrastructure/src/secretsfile.rs:64`), and only when they do not
  already exist.

Today the `0600` file mode is the only thing keeping a plaintext secret in a
checkout away from other unprivileged local users. Widening it therefore has
to be paid for by tightening the tree (§4), not waved away.

## Non-goals

- Per-file modes. One project-wide setting; a project needing two different
  modes for two secrets is not a case anyone has.
- Choosing the owner uid/gid. The agent runs as `User=rpi-agent`
  (`crates/bin/src/agent/setup.rs:65`), so it cannot `chown` to an arbitrary
  uid at all, and gaining that ability means giving the agent privileges it
  deliberately does not have.
- ACLs (`setfacl`). Precise, but needs the container's uid in the config, a
  libacl dependency and filesystem support — cost far above the benefit over
  §1.
- Auto-injecting `group_add` into generated compose overrides.
  `FsOverrideStore` (`crates/infrastructure/src/overrides.rs:43`) writes an
  override for the public service only and does not know the service list.
- Changing how consumers declare secrets in their own compose files.

## 1. What is written, and with which mode

| Written into the checkout | No `file_mode` | `file_mode = "0640"` |
| --- | --- | --- |
| files from `[secrets].files` | **0644** | 0640 |
| `.env` | 0600 | 0640 |
| directories created by the writer | **0755** | 0755 |
| `.rpi-secrets-manifest.json` | 0600 | 0600 |
| stored bundle, agent age key | 0600 | 0600 |

`.env` keeps `0600` by default because compose reads it as the agent and
nothing is broken today; `file_mode` still covers it, so a project that
bind-mounts `.env` into a container has a supported way out instead of a
workaround.

The manifest is the writer's own bookkeeping, not a project secret, and
nobody mounts it. It stays private unconditionally so that `file_mode` never
needs the caveat "except that one file, which isn't really yours".

Directory modes play no part in confidentiality — file contents are
protected by the file mode, and the names are in the repository anyway.
Directories are therefore always `0755`, independent of `file_mode`: one
rule instead of two coupled ones.

## 2. Getting the mode to disk

`fsutil::write_private_atomic` takes a mode parameter. The order keeps every
intermediate state safe:

```
create temp (0600) -> write -> sync_all -> set_permissions(mode) -> rename
```

The widening happens on a file not yet visible under its final name, so
there is no window in which a partially written file is already readable.
`set_permissions` rather than `OpenOptions::mode` is deliberate: the latter
is masked by the unit's umask, and the result must not depend on what
systemd happens to set.

The mode travels with the bundle:

- `SecretsBundle.file_mode: Option<u32>` (`crates/domain/src/entities.rs:11`)
- `StoredBundle.file_mode` with `#[serde(default)]`
  (`crates/infrastructure/src/secrets.rs:23`) so previously stored blobs
  deserialize unchanged
- `SecretsSendRequest.file_mode` (`crates/bin/src/proto.rs:151`)

Deploy reads the mode from the stored bundle, so `rpi deploy` without a
resend applies it exactly as `rpi secrets send --apply` does.

No migration is needed. Writes go through temp + rename, so the first deploy
after the agent is updated replaces every existing `0600` file with the new
mode.

## 3. Directories: creation and self-healing

While walking the path to each file in the bundle — the same loop that
already rejects symlinked components — for every intermediate directory:

- missing → create `0755`;
- exists, owned by the current euid, mode exactly `0700` → widen to `0755`
  (that combination is the fingerprint of this writer's own earlier run);
- anything else → leave alone. A directory from git (`0755`), one owned by
  someone else, or one with any other mode is not ours to touch.

The decision is made right after `stat_dir_component`, at the same step
where `DirStep::Symlink` is rejected today, so the symlink guard is not
weakened.

Without self-healing, every already-deployed project would keep a `0700`
directory forever and directory-style mounts (`- ./secrets:/app/secrets`)
would keep failing after the fix ships — the same debugging session a second
time.

## 4. Tightening `/var/lib/rpi`

Protection moves from the individual file to the root of the tree:
`install -d -m 0750 -o rpi-agent -g rpi-agent`. For existing installs,
`ensure_dir` (`crates/bin/src/agent/setup.rs:168`) currently repairs only
ownership in its "already exists" branch; it also repairs the mode, and the
result appears in the `rpi setup` report as
`repaired: /var/lib/rpi (mode)`.

Who can read plaintext secrets in a checkout:

- **before**: `/var/lib/rpi` `0755`, file `0600` → `rpi-agent` and root;
- **after**: `/var/lib/rpi` `0750`, file `0644` → `rpi-agent`, root, and
  members of the `rpi-agent` group.

`rpi setup` adds the Pi's login user to the `rpi-agent` group
(`crates/bin/src/agent/setup.rs:396`), which is precisely the membership
that grants access to the agent's control socket (mode `0660`,
`crates/bin/src/agent/run.rs:111`). Anyone who can reach that socket can
already deploy arbitrary code and run commands inside a project's
containers — that is, read any secret the project holds. The group therefore
gains nothing it did not have, while an unprivileged user outside it now
loses access to the whole tree instead of only to those files that happened
to carry the right mode. (`rpi setup` separately adds `rpi-agent` itself —
not the login user — to the `docker` group, `:379`.)

Only `/var/lib/rpi` changes. `/var/log/rpi` and `/etc/rpi` hold no secrets
and are left alone rather than widening the blast radius for symmetry's
sake.

## 5. `[secrets].file_mode`

```toml
[secrets]
files = ["packages/widgets/passport-checker/secrets/series"]
file_mode = "0640"   # optional; default: 0644 for files, 0600 for .env
```

A string, not a number: `mode = 644` in TOML is decimal 644, a trap for no
reason. Accepted form is `^0?[0-7]{3}$`, so `"0644"` and `"644"` are the
same value.

Permitted bits are described by a rule rather than a list: the owner gets
read and optionally write; group and others get read only. Rejected are
execute bits (a secret is not a program), setuid/setgid/sticky (a fourth
digit is not accepted at all), and write for anyone but the owner (a
container has no business rewriting what `rpi` overwrites on the next
deploy). In practice this admits `0400 0440 0444 0600 0640 0644`, while
still allowing sensible exotica such as `0604` without a code change.

Validation runs in two places, mirroring what path validation already does
(`docs/architecture/flows/secrets.md` §8):

- `RpiToml::validate_common()` (`crates/bin/src/cli/rpitoml.rs:256`), so a
  merged overlay is checked exactly like a base file;
- again on the agent before writing, because the other end of the wire is
  not necessarily our CLI. An invalid mode there is a `DomainError::Invalid`
  that stops the deploy instead of producing a world-writable file.

In an overlay, `file_mode = ""` resets to the default — the same convention
already used for `ingress.hostname`, `secrets.env` and `healthcheck.path`
(`crates/bin/src/cli/overlay.rs:624`). `[secrets]` already participates in
overlay merging (`overlay.rs:611,649`), so this is an additional field, not
a new mechanism.

## 6. Compatibility

`Feature::SecretModes`, capability `"secret-modes"`, `since = "0.26.0"`,
`Policy::Required` (`crates/bin/src/compat.rs:27`).

The gate fires **only when `file_mode` is actually set**: a project without
it keeps working against an older agent exactly as before, simply without
the new default.

`Required` rather than `Degradable` because silence is the failure mode this
whole design exists to remove. An old agent's serde would drop the unknown
field, store a bundle without a mode, write `0600`, and send the operator
back to reading container logs while being sure the configuration was
right.

Note for release notes: updating only the CLI changes nothing. The mode is
written by the agent, so the fix lands when the Pi is updated.

## 7. Diagnostics

Three additions, one per stretch of the path where the failure was
invisible:

- **Deploy and `secrets send --apply` log**: one line after the write, e.g.
  `secrets: 3 vars, 2 files (mode 0644)`. Counts and mode only; values are
  never logged, and masking is armed by then in any case.
- **`rpi secrets ls`**: the effective file mode in the output header —
  `file_mode` when the bundle carries one, otherwise the `0644` default.
  The on-disk state is deliberately not probed — the listing comes from the
  encrypted store, the checkout may not exist, and any divergence between
  "what is configured" and "what will be applied" is what §6 covers.
- **`rpi doctor`**: a `data dir permissions` check in
  `crates/infrastructure/src/probe.rs` — `/var/lib/rpi` is owned by
  `rpi-agent` and no wider than `0750`; hint `sudo rpi agent setup`, which
  is idempotent and repairs exactly this. It sits next to the existing
  `rpi-agent group` check (`probe.rs:193`).

## 8. Testing

Unit tests, on the invariants that change:

- files default to `0644`; `.env` defaults to `0600`; `file_mode` applies to
  both; directories are `0755`; the manifest stays `0600`
- self-healing widens a `0700` agent-owned directory, and leaves alone a
  directory from the repository, one with another owner, and one with any
  other mode
- existing assertions at `secretsfile.rs:603` and `:608` are updated to the
  new values; `env_file_is_0600_even_when_replacing_a_wider_one` stays valid
  as written; the symlink-escape tests must keep passing untouched
- `file_mode` parsing and validation, overlay override and empty-string
  reset, the new variant in `compat::Feature::ALL`, `install -d -m 0750` and
  the mode repair in `setup.rs`

E2E: a `secret-file-perms` scenario — the one that was missing. A fixture
with `user: "1000:1000"`, a secret declared as `secrets: file:`, a container
that reads `/run/secrets/...`, and `rpi command` asserting the contents. On
today's code this scenario fails with EACCES, i.e. it reproduces the defect
rather than describing the area around it.

## 9. Documentation

- `docs/architecture/flows/secrets.md` — "mode 0600" appears twice in the
  diagram plus in walkthrough item 4; the `secretsfile.rs` source anchor
  also states the modes
- `README.md`, `plugins/rpi/skills/rpi-toml/SKILL.md`,
  `plugins/rpi/skills/rpi-cli/SKILL.md`
- `docs/superpowers/specs/2026-07-07-secret-files-design.md` — one line
  recording that its YAGNI item on file modes is superseded here, so the
  next reader does not mistake a retracted decision for a live one

## 10. Version

Minor bump to `0.26.0`, matching the `since` of the new capability.

## Risks

- **A project relying on `0600`.** Anything reading a secret as the agent
  keeps working; the change only adds readers inside the `rpi-agent` group
  and the docker socket already grants that. `file_mode = "0600"` restores
  the old behaviour explicitly.
- **`/var/lib/rpi` at `0750` breaking an operator's own scripts.** Only for
  a user outside the `rpi-agent` group; the repair is reported by
  `rpi setup` rather than applied silently.
- **Directory self-healing overwriting a deliberate `0700`.** Narrow by
  construction (agent-owned and exactly `0700`), and harmless once the tree
  root is `0750` — a directory mode does not protect file contents.

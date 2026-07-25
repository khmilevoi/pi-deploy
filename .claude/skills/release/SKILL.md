---
name: release
description: Use when cutting, publishing, planning, or troubleshooting a release of rpi-deploy — choosing the version bump (patch vs minor vs major), git tag, GitHub Release binaries, npm publish, and the post-release landing page audit.
---

# Releasing rpi-deploy

Pushing a tag `vX.Y.Z` triggers `.github/workflows/release.yml`, which does everything downstream automatically: version checks → 3-target binary build → GitHub Release with SHA256SUMS and generated notes → npm publish (OIDC, no token). Your job is one correct commit plus one correct tag. Both real release mistakes in this repo's history were made *before* the tag: a version-sync miss (fix commit `492012d`) and a stale README status line — steps 2 and 3 exist because of them.

The version lives in three files that must agree, plus the tag: `Cargo.toml` (`[workspace.package] version` — the only Cargo.toml to touch; crates inherit via `version.workspace = true`), `package.json`, and `Cargo.lock` (regenerated, never hand-edited). CI compares the tag byte-for-byte against `"v" + package.json version`.

## Choosing the bump (patch / minor / major)

Decide from the actual unreleased commits, never from memory of what "feels" shipped:

```
rtk git log --oneline "$(git describe --tags --abbrev=0)..HEAD"
```

The project is pre-1.0; the rules as practiced here:

- **Minor** (`0.X.Y → 0.(X+1).0`) — at least one commit gives users something new to do or see: any `feat:` commit, a new command or flag, a new config field, new output rendering, or a deprecation (the old way still works but warns — that's new behavior, not a fix). Precedents: v0.14.0 (output theming), v0.17.0 (token-via-file setup, new doctor ingress checks).
- **Patch** (`0.X.Y → 0.X.(Y+1)`) — only `fix:` / `docs:` / `chore:` / `ci:` commits: corrections to behavior that already existed, nothing users can newly do. Precedents: v0.9.1 (self-install fix), v0.17.1 (doctor false-failure fix).
- **Major** — not used before 1.0. A breaking change (rpi.toml schema, removed flag or command, agent-protocol incompatibility) still ships as a **minor** bump, but requires a `docs/migration-*.md` (precedent: `migration-v0.5-to-v0.6.md`) and a README callout. Moving to 1.0.0 is a deliberate API-stability decision — only on the user's explicit call, never yours.

Tiebreakers: mixed `feat` + `fix` → minor (feat wins). "Is this rendering change a feature?" — if a user looking at the terminal can tell the difference, yes → minor. If genuinely torn, name the two candidate versions and your reasoning to the user before bumping.

## Quick mode (patch releases)

`/release quick` asks for the short path. `node scripts/release-preflight.mjs`
decides whether it is allowed — its verdict overrides the request, so a
`quick: refused` line means the full checklist below, no exceptions.

The mode rests on one fact: in a patch release the fix is already merged
and already green on master, and the release commit touches only
`Cargo.toml`, `package.json` and `Cargo.lock` — files that cannot break
clippy or a test. So quick mode does not skip checks, it declines to re-run
checks that already passed on the same tree. That reasoning does not
survive a `feat:` commit, which is why preflight refuses one.

1. `node scripts/release-preflight.mjs` → `quick: allowed (X.Y.Z)`.
2. `node scripts/bump-version.mjs X.Y.Z` — writes the three files, runs
   `cargo update --workspace`, and refuses if the lockfile picked up an
   unrelated dependency bump or if anything else is in the tree.
3. `rtk git commit -m "chore: release X.Y.Z"` with those three files, push.
4. `rtk git tag -a vX.Y.Z -m "vX.Y.Z" && rtk git push origin vX.Y.Z` —
   immediately, without waiting for `ci` on the release commit. The release
   workflow's own `check` job re-runs versions and tests before `build` and
   `publish`, so a failure leaves a dead tag, never a partial publish, and
   the recovery in "If the release workflow fails after the tag" applies.
5. `node scripts/release-verify.mjs X.Y.Z`.
6. Release notes — required, same as any release, but short: one bullet per
   `fix:` commit stating the old and the new behaviour. See "Release notes"
   below.
7. Landing: `cd` to the site repo, `rtk git pull`,
   `npm run sync-version -- X.Y.Z`, `npm run og`, commit, push,
   `rpi deploy`, then confirm the live page and re-fetch `og:image`.

What quick mode does **not** do, and why it is safe:

- **The local gate** (`fmt`, `clippy`, `test`, `node --test`, `npm pack`) —
  replaced by preflight's "ci green on HEAD". Note this is a move between
  two check surfaces, not a narrowing: the local gate runs on Windows and
  catches things a Linux CI never will, and vice versa. It is acceptable
  only because the release commit is version-only.
- **The README `Status: vX.Y` line** — invariant under a patch bump
  (`0.25.1 → 0.25.2` leaves `v0.25`). Other documentation is still updated
  when the fix changes behaviour the docs describe.
- **The four-auditor landing audit** — replaced by `sync-version`, which
  guarantees versions by construction. Feature text, CLI transcripts and
  `llms.txt` are still audited on every minor and major release.

## Release checklist (one commit, then one tag)

0. **Preflight**: `node scripts/release-preflight.mjs`. It reports a clean
   tree, `HEAD == origin/master`, the classified commit range, the current
   and next-patch version, and whether `ci` is green on HEAD. Its `bump:`
   line is input to the decision above, not a replacement for it — minor
   versus major is still your call.
1. **Clean start**: `rtk git status` clean, `rtk git pull` — you release exactly `origin/master` HEAD.
2. **Bump versions**: `node scripts/bump-version.mjs X.Y.Z`. It writes
   `Cargo.toml` `[workspace.package] version` and `package.json` `version`,
   runs `cargo update --workspace`, and fails if `Cargo.lock` carries any
   change beyond the workspace version lines or if the tree holds anything
   but those three files. A stale lockfile is a guaranteed CI failure
   (`--locked` everywhere), which is what the assertion is for.
3. **Update docs — this is part of the release commit, not optional polish**:
   - `README.md` "Status: vX.Y (...)" line near the top: new version + one-phrase feature summary, and fold the shipped features into the surrounding status paragraph / Supported features list (see how v0.7 prebuilt binaries is described there).
   - If the release changes behavior users must migrate through, add `docs/migration-*.md` (precedent: `migration-v0.5-to-v0.6.md`).
   - The landing page lives in a separate repo and is a **post-release follow-up** — run the "Landing page audit" section below after the tag; never fold it into the release commit.
4. **Local gate** (mirrors CI's `check` job and `ci.yml`; catch it here, not in CI):
   ```
   node scripts/check-version.js        # must print: check-version: ok (X.Y.Z)
   rtk cargo fmt --all -- --check
   rtk cargo clippy --all-targets --locked -- -D warnings
   rtk cargo test --locked
   node --test "scripts/**/*.test.*"   # postinstall + release tooling tests; CI runs these too
   npm pack --dry-run                   # tarball must include bin/, scripts/, crates/, Cargo.toml, Cargo.lock
   ```
5. **Commit and push**: `chore: release X.Y.Z` with `Cargo.toml package.json Cargo.lock README.md` (+ any docs). Wait for the `ci` workflow to go green: `rtk gh run list --workflow ci --limit 1`.
6. **Optional dry run** (recommended after toolchain/dependency changes): `gh workflow run release.yml --ref master` builds all 3 targets (Windows MSVC, x86_64/aarch64 musl) but skips release + publish.
7. **Tag and push**: `rtk git tag -a vX.Y.Z -m "vX.Y.Z" && rtk git push origin vX.Y.Z`. Lowercase `v`, full three-part version — the check job rejects anything else.

## After the tag (automatic — do not do these by hand)

check (versions+tests) → build (3 archives named `rpi-vX.Y.Z-<triple>.*`) → GitHub Release (`--generate-notes`, SHA256SUMS) → npm publish. The generated notes are only a raw commit list — turning them into a real description of what changed is a required post-release step (see "Release notes" below), not optional polish.

## Post-release verification

```
node scripts/release-verify.mjs X.Y.Z
```

It checks the release workflow's jobs, the release assets, the published npm version, and an `npx` smoke test, and exits non-zero if any of them is wrong — the workflow's jobs must all be green; let the script's own output say which.

The npx check runs inside a throwaway Docker container, never directly on the dev machine — a local machine can have a global `rpi-deploy` install or npx cache that shadows the version resolution and silently passes/fails against stale state instead of the real published package. The script also times the install and flags anything slower than ~90s, since that means npx fell back to a source build instead of the prebuilt binary.

## Release notes: describe what changed (required)

`--generate-notes` produces commit subjects, which describe the work, not the change — a user reading "chore(cli): tidy stats_render imports" learns nothing. After the workflow publishes the release, rewrite the notes so they open with a **What changed** section: one bullet per user-facing change, each stating what exactly changed and what the user can now do or will now see (new command/flag, changed output, fixed behavior — with the old vs new behavior for fixes). Derive the bullets from the same `git log` range you used to choose the bump, never from memory; internal-only commits (refactors, CI, test scaffolding) are folded into a single "Internal" line or omitted. Keep the auto-generated commit list below it as "Full changelog". Then update the release:

```
gh release edit vX.Y.Z --notes-file notes.md
```

The release is not done until the notes describe the changes — a green npm publish with a bare commit list does not close the checklist.

## Landing page audit (after every release, in subagents)

The landing (`rpi-deploy-site`, live at https://rpi.iiskelo.com) once sat five releases stale — quick-start step 1 still printed `rpi 0.12.0` when v0.17.1 was current — because this step used to say "check whether this release changed anything the landing shows", and that check got answered from memory ("probably not") instead of by reading the page. Drift accumulates across releases, so the audit is unconditional: run it even when this release "obviously" changed nothing user-visible — the drift you find is usually from earlier releases.

The audit brief lives in the site repo, not here — **read `docs/landing-audit.md` in `rpi-deploy-site` before doing any of this** (step 1 below gets you a local copy). It defines the four auditor lenses, the shared context each needs, and the report format; this section only covers the release-side mechanics.

1. **Sync the site repo.** Local checkout is a sibling directory: `C:\Users\Khmil\RustProjects\rpi-deploy-site` — `cd` there and `rtk git pull` first; only `git clone git@github.com:khmilevoi/rpi-deploy-site.git` as a fallback if the directory doesn't exist. Then read `docs/landing-audit.md`.
2. **Sync the version first**: `npm run sync-version -- X.Y.Z` in the site
   repo. It rewrites every semver under `src/` and fails loudly if they are
   not all the same string, so the auditors never spend attention on
   numbers. If it reports a mixed set, resolve that by hand before
   continuing — a foreign semver on the page is exactly the kind of thing
   the audit exists to catch.
3. **Spawn four read-only auditor subagents in parallel** (one message; Explore-type agents fit — they must not edit anything), one per section of `docs/landing-audit.md` (facts and numbers; CLI output fidelity; features and quick start; discovery files — `llms.txt`/`sitemap.xml`/`robots.txt`). Each prompt must be self-contained: the site repo path, the pi repo path, the absolute path to `docs/landing-audit.md` in the site repo with which section to follow, and the report format defined there. Version strings are already handled by `sync-version`; auditors report on feature text, CLI transcripts and discovery files.
4. **Apply the confirmed fixes yourself** in the site repo (auditors only report, except Auditor 4's discovery-file fixes, which are low-risk enough to apply directly per the brief). When you rewrite any terminal block, transcribe it from the rendering code — the canonical deploy transcript and colour map in the Auditor 2 section of `docs/landing-audit.md` — never from memory or invention.
5. **Regenerate the OG image before publishing — always, not only "if it changed".** `src/assets/og.png` is a rendered screenshot of the hero (`npm run og`, `scripts/generate-og.mjs`), so any edit to the hero, its terminal, the logo, copy, or styling silently stales it, and a stale OG is what everyone sees when the link is shared. Run `npm run og` and confirm `git status` shows `src/assets/og.png` restaged (or unchanged only if the hero genuinely did not move). Never commit hero changes without this.
6. **Deploy and verify**: commit (include the regenerated `og.png` and any discovery-file fixes), push, then `rpi deploy` from the site repo root; check the live page reflects the fixes and re-fetch the OG (`og:image`) to confirm it is the new hero. The npm version *badge* is the only element that updates itself — every other number and claim on the page, and in `llms.txt`/`sitemap.xml`, is hand-written.
7. **Purge the CDN cache if the deploy touched `styles.css`, `copy.js`, or `src/assets/**`.** These have no content hash in their filename, so Cloudflare's edge cache doesn't know they changed and keeps serving pre-deploy bytes until its TTL expires — `index.html` updates instantly (never cached) while CSS/JS/fonts silently lag behind for up to hours. See the "Post-deploy: purge the CDN cache" section of `docs/landing-audit.md` in the site repo for the purge command and how to verify it took (`Cf-Cache-Status` + a byte-diff against local, not just eyeballing the page).

## If the release workflow fails after the tag

Fix on master, then delete and re-create the tag: `git tag -d vX.Y.Z && git push origin :refs/tags/vX.Y.Z`, delete the partial GitHub Release if one exists, re-tag. **Only until npm publish has succeeded** — published npm versions are immutable; after that, ship `X.Y.Z+1` instead.

---
name: release
description: Use when cutting, publishing, planning, or troubleshooting a release of rpi-deploy — choosing the version bump (patch vs minor vs major), git tag, GitHub Release binaries, npm publish, and the post-release landing page audit.
---

# Releasing rpi-deploy

Pushing a tag `vX.Y.Z` triggers `.github/workflows/release.yml`, which does everything downstream automatically: version checks → 3-target binary build → GitHub Release with SHA256SUMS and generated notes → npm publish (OIDC, no token). Your job is one correct commit plus one correct tag. Both real release mistakes in this repo's history were made *before* the tag: a version-sync miss (fix commit `492012d`) and a stale README `## Highlights` section — steps 2 and 3 exist because of them.

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
`quick: refused` line means the full checklist below, no exceptions,
whichever of its four independent grounds fired: a dirty working tree,
`HEAD` not equal to `origin/master`, a commit in range that is minor-level
or unclassifiable (`feat:`/`perf:`, an `!` or `BREAKING CHANGE` marker, or
an unrecognised prefix), or no green `ci` run for HEAD.

The mode rests on one fact: in a patch release the fix is already merged
and already green on master, and the release commit touches only
`Cargo.toml`, `package.json` and `Cargo.lock` — files that cannot break
clippy or a test. So quick mode does not skip checks, it declines to re-run
checks that already passed on the same tree. That reasoning does not
survive a `feat:` commit, which is why preflight refuses one.

1. `node scripts/release-preflight.mjs` → `quick: allowed (X.Y.Z)`.
2. `node scripts/bump-version.mjs X.Y.Z` — writes the three files, runs
   `cargo update --workspace`, and refuses if the lockfile picked up an
   unrelated dependency bump or if anything else is in the tree. If it fails
   *after* writing, it prints the `git checkout -- Cargo.toml package.json
   Cargo.lock` line that puts the tree back — run that before re-running it,
   or the dirty-tree guard blocks the retry. Exit 1 is a refusal you must
   act on; exit 2 means the script could not run (no `cargo`, a registry
   outage) and says nothing about the release.
3. `npm pack --dry-run` — **kept, not skipped**. It takes about two seconds,
   and it is the one local-gate item with no CI equivalent: `ci.yml` runs
   `fmt`, `clippy`, `cargo test --locked`, `npm run test:node` and e2e, and
   none of them builds the tarball or reads `package.json`'s `files`
   whitelist. So a fix like "move the binary resolver into
   `scripts/lib/resolve.js`" is green on CI (tests run against the repo
   tree) and green in the release workflow's `check` job, and still publishes
   a package whose `postinstall` dies with MODULE_NOT_FOUND for every
   installer — because `files` lists only `scripts/postinstall.js` and
   `scripts/check-version.js`. It is also the only gate item whose failure
   cannot be undone: a published npm version is immutable, and `latest` has
   already moved by the time the first install fails. The tarball must
   include `bin/`, the whitelisted `scripts/`, `crates/`, `Cargo.toml`,
   `Cargo.lock`.
4. `rtk git commit -m "chore: release X.Y.Z"` with those three files, then
   `rtk git push`. **If the push is rejected as non-fast-forward, go back to
   step 1 and re-run preflight — do not `git pull --rebase` and push
   again.** The rejection means a PR merged to `master` while you were
   releasing, and rebasing puts the release commit on top of commits
   preflight never classified, one of which may be a `feat:`; tagging then
   ships unverified, possibly minor-level code as a patch with the local
   gate skipped. Nothing later catches this — the step-5 check cannot (after
   a rebase and a successful push the release commit genuinely *is* on
   `origin/master`), and the workflow's `check` job only compares
   `package.json` against the tag name. The rejected push is the signal;
   act on it there. Going back means dropping the release commit first —
   `rtk git fetch origin master && rtk git reset --hard origin/master` —
   otherwise preflight refuses on `HEAD != origin/master`; nothing is lost,
   the commit holds only the three files step 2 regenerates.
5. **Confirm the commit you are about to tag is on `origin/master`**:
   ```
   rtk git fetch origin master
   rtk git merge-base --is-ancestor HEAD origin/master
   ```
   The fetch is only there to make the answer current: `origin/master` is a
   local cache, and git advances it on a successful push but not on a
   rejected one. `--is-ancestor` exits 0 when HEAD is contained in
   `origin/master` — which is the property actually wanted, "the commit I am
   tagging is on master". Non-zero means it is not: the push never landed,
   and `rtk git push origin vX.Y.Z` would carry the missing objects to
   GitHub as the tag's own payload, producing a released commit that is on
   no branch. Ancestry rather than equality on purpose — someone else's PR
   merging after your push moves `origin/master` ahead of you and leaves
   your tag perfectly correct, so an equality check would refuse a good
   release. Non-zero means stop: push the commit, or if the push was
   rejected go back to step 1. Never `--force`. (Preflight's own
   `HEAD == origin/master` answers a different question at a different
   moment — before the bump you must be sitting exactly on `master` HEAD, or
   the commit range it classifies is not the range you would release.)
6. `rtk git tag -a vX.Y.Z -m "vX.Y.Z" && rtk git push origin vX.Y.Z` —
   immediately, without waiting for `ci` on the release commit. The release
   workflow's own `check` job re-runs versions and tests before `build` and
   `publish`, so a failure leaves a dead tag, never a partial publish, and
   the recovery in "If the release workflow fails after the tag" applies.
7. **Wait for the release workflow, then verify.** The GitHub Release does
   not exist until the `release` job creates it, roughly 5-10 minutes after
   the tag — verifying before that reports the release as missing, which is
   noise, not a finding:
   ```
   gh run watch "$(gh run list --workflow release --branch vX.Y.Z --limit 1 --json databaseId --jq '.[0].databaseId')"
   node scripts/release-verify.mjs X.Y.Z
   ```
   `--branch vX.Y.Z` is not optional: a tag-triggered run reports the tag as
   its `headBranch`, and without the filter the newest `release` run is
   whichever one ran last — the previous release, or a
   `gh workflow run release.yml` dry run — so
   `gh run watch` returns "completed successfully" instantly and you walk
   into a verify against a release that has not been built yet.
   `release-verify.mjs` selects its run the same way, by `headBranch == tag`.
   If the command prints nothing at all, GitHub has not registered the run
   yet: wait a few seconds and repeat it.
8. Release notes — required, same as any release, but short: one bullet per
   `fix:` commit stating the old and the new behaviour. See "Release notes"
   below.
9. Landing: `cd` to the site repo, `rtk git pull`,
   `npm run sync-version -- X.Y.Z`, `npm run og`, commit, push,
   `rpi deploy`, then confirm the live page and re-fetch `og:image`.
   No CDN purge is needed **as long as `sync-version` reports every
   replacement in `src/index.html`** — that file is never edge-cached, and
   `og.png` is reached only through the versioned `?v=` query the sync just
   bumped. The script scans every non-binary file under `src/` by design, so
   read its per-file output rather than assuming: a replacement in
   `copy.js`, `styles.css` or `assets/` does require a purge, and the script
   prints an explicit warning naming it when that happens. See the
   "Post-deploy: purge the CDN cache" section of `docs/landing-audit.md` in
   the site repo.

What quick mode does **not** do, and why it is safe:

- **The local gate** (`fmt`, `clippy`, `test`, `node --test`) — replaced by
  preflight's "ci green on HEAD". Note this is a move between two check
  surfaces, not a narrowing: the local gate runs on Windows and catches
  things a Linux CI never will, and vice versa. It is acceptable only
  because the release commit is version-only. `npm pack --dry-run` is
  deliberately absent from this list — it is not replaced by anything, which
  is why it stays as step 3.
- **A README edit** — a patch release usually needs none at all. `README.md`
  `## Highlights` is updated only when the fix changes behaviour described
  there; other documentation is still updated when the fix changes
  documented behaviour it describes. When it *is* needed, the order is
  forced: run `bump-version.mjs` first (it refuses a dirty tree), then edit
  the README, then commit all four files — the script's own refusal message
  does not say this.
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
   - `README.md` `## Highlights` section: update it when the release changes or adds
     something it describes. The npm version badge near the top updates itself and needs
     no edit.
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
5. **Commit and push**: `chore: release X.Y.Z` with `Cargo.toml package.json Cargo.lock README.md` (+ any docs), then wait for the `ci` workflow to go green: `rtk gh run list --workflow ci --limit 1`.
   **If the push is rejected as non-fast-forward, go back to step 0 — do not
   `git pull --rebase` and push again.** The rejection means a PR merged to
   `master` while you were preparing the release, so the commit range you
   chose the bump from and ran the local gate against is no longer the range
   you would be tagging: the new commits can turn your patch into a minor,
   and nothing downstream re-reads them. Re-run preflight, re-check the
   bump, and redo steps 2-4 on the new base. Step 7's ancestry check does
   not cover this — after a rebase and a successful push the release commit
   really is on `origin/master`. Drop the release commit before going back
   (preflight and `bump-version.mjs` both refuse otherwise): `rtk git reset
   --hard origin/master` when it holds only the three generated files, or
   `rtk git reset --soft HEAD~1 && rtk git stash` first when it also carries
   README or docs edits you want to keep — step 2 needs a clean tree, so
   restore them after it.
6. **Optional dry run** (recommended after toolchain/dependency changes): `gh workflow run release.yml --ref master` builds all 3 targets (Windows MSVC, x86_64/aarch64 musl) but skips release + publish.
7. **Tag and push**: first confirm the commit you are about to tag is on `origin/master`:
   ```
   rtk git fetch origin master
   rtk git merge-base --is-ancestor HEAD origin/master
   ```
   The fetch is only there to make the answer current — `origin/master` is a
   local cache, advanced by a successful push and not by a rejected one.
   `--is-ancestor` exits 0 when HEAD is contained in `origin/master`, which
   is the property actually wanted. Non-zero means the release commit never
   landed on the remote, and `rtk git push origin vX.Y.Z` would carry its
   objects to GitHub as the tag's own payload — a released commit on no
   branch, which the workflow's `check` job cannot catch because it only
   compares `package.json` against the tag name. Ancestry rather than
   equality on purpose: another PR merging after your push moves
   `origin/master` ahead and leaves your tag perfectly correct, so an
   equality check would refuse a good release. The case this check does
   *not* cover — a rebase after a rejected push — is handled at step 5,
   where the rejection happens. Then
   `rtk git tag -a vX.Y.Z -m "vX.Y.Z" && rtk git push origin vX.Y.Z`. Lowercase `v`,
   full three-part version — the check job rejects anything else.

## After the tag (automatic — do not do these by hand)

check (versions+tests) → build (3 archives named `rpi-vX.Y.Z-<triple>.*`) → GitHub Release (`--generate-notes`, SHA256SUMS) → npm publish. The generated notes are only a raw commit list — turning them into a real description of what changed is a required post-release step (see "Release notes" below), not optional polish.

## Post-release verification

Wait for the workflow first. The GitHub Release, the assets and the npm
version do not exist until the `release` and `npm-publish` jobs run, roughly
5-10 minutes after the tag — verifying before that reports the release as
not yet published, which is noise rather than a finding:

```
gh run watch "$(gh run list --workflow release --branch vX.Y.Z --limit 1 --json databaseId --jq '.[0].databaseId')"
node scripts/release-verify.mjs X.Y.Z
```

`--branch vX.Y.Z` selects the run this tag triggered — a tag-triggered run
reports the tag as its `headBranch`, and `release-verify.mjs` finds its run
the same way. Without the filter the newest `release` run is whichever ran
last (the previous release, or a step-6 dry run), so `gh run watch` reports
success instantly and the verify runs against a release that does not exist
yet. An empty result means GitHub has not registered the run yet — wait a
few seconds and repeat.

It checks the release workflow's jobs, the release assets, the published npm version, and an `npx` smoke test, and exits non-zero if any of them is wrong — the workflow's jobs must all be green; let the script's own output say which. The job list is matrix-expanded: a real run reports six rows, three of them `build (…)` instances, for four required job names.

Run too early it reports rather than crashes: a release the `release` job has
not created yet becomes a failing `assets` check, the npx smoke test is
reported as *not run* (a failing check, never a pass) instead of costing a
docker pull to rediscover the same fact, and every check that could answer
still prints. Only a broken environment — no `gh` or `docker` on PATH, an
expired token, a stopped Docker daemon, no network — exits 2, and it prints
the results it already had on the way out.

The npx check runs inside a throwaway Docker container, never directly on the dev machine — a local machine can have a global `rpi-deploy` install or npx cache that shadows the version resolution and silently passes/fails against stale state instead of the real published package. The script also times the install and flags anything slower than ~90s, since that means npx fell back to a source build instead of the prebuilt binary.

## Release notes: describe what changed (required)

`--generate-notes` produces commit subjects, which describe the work, not the change — a user reading "chore(cli): tidy stats_render imports" learns nothing. After the workflow publishes the release, rewrite the notes so they open with a **What changed** section: one bullet per user-facing change, each stating what exactly changed and what the user can now do or will now see (new command/flag, changed output, fixed behavior — with the old vs new behavior for fixes). Derive the bullets from the same `git log` range you used to choose the bump, never from memory; internal-only commits (refactors, CI, test scaffolding) are folded into a single "Internal" line or omitted. Keep the auto-generated commit list below it as "Full changelog". Then update the release:

```
gh release edit vX.Y.Z --notes-file notes.md
```

The release is not done until the notes describe the changes — a green npm publish with a bare commit list does not close the checklist.

## Landing page audit (minor and major releases, in subagents)

The landing (`rpi-deploy-site`, live at https://rpi.iiskelo.com) once sat five releases stale — quick-start step 1 still printed `rpi 0.12.0` when v0.17.1 was current — because this step used to say "check whether this release changed anything the landing shows", and that check got answered from memory ("probably not") instead of by reading the page. Drift accumulates across releases, so the audit is unconditional **on every minor and major release**: run it even when this release "obviously" changed nothing user-visible — "obviously nothing" is the exact reasoning that let the landing sit stale before, and the drift you find is usually from earlier releases. Patch releases do not run this audit — quick mode's landing step above covers them with `sync-version` alone, which guarantees version strings by construction but not feature text; that gap is deliberate and accumulates until the next minor or major runs the full audit.

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

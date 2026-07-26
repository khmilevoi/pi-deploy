# Release Quick Mode — Design

Date: 2026-07-25
Status: design pending review. Changes `.claude/skills/release/SKILL.md` and
adds release tooling to `scripts/` here and in `rpi-deploy-site`.

## Goal

Make a patch release — a bug fix already merged and green on master, with
nothing new for a user to do — cost a few minutes instead of the full
release ceremony, without giving up the two guarantees that ceremony buys:
versions never drift apart, and the landing page never shows a stale
version.

## The observation this rests on

In a patch release the fix is already on master and already passed CI. The
release commit itself touches only `Cargo.toml`, `package.json` and
`Cargo.lock`. Those three files cannot break `clippy` or a test.

So most of the local gate (`fmt`, `clippy`, `test`, `node --test`) is not a
check being risked — it is a re-run of checks that already passed on the
same code tree. The quick mode's rule is therefore **do not re-run what is
already green**, not "skip checks".

`npm pack --dry-run` is the exception and stays in quick mode (§2): it is
the one gate item nothing in CI re-runs, so there is no green result to
lean on.

The same reasoning does not extend to a minor release, where the range
contains commits whose behaviour is new. Quick mode is patch-only by
construction (§1).

## Non-goals

- A separate tool set for quick mode. All four scripts (§3) are used by both
  modes; the modes differ only in what they skip between the same gates.
- Automating the bump decision beyond a patch/not-patch verdict. Choosing
  between minor and major stays a judgement call.
- Automating release notes. They describe what a change means to a user,
  which is not derivable from commit subjects.
- Replacing the four-auditor landing audit. It stays unconditional for
  minor and major releases.

## 1. Mode selection

`/release quick` requests the mode. `/release` is unchanged, except that it
now reports when the range looks patch-shaped.

The request is not the authority. `release-preflight.mjs` (§3.1) is: its
verdict decides, and `quick: refused` sends the release down the full path
even when the user asked for quick.

Quick is allowed only when every non-merge commit in
`$(git describe --tags --abbrev=0)..HEAD` matches
`^(fix|docs|chore|ci|test|style|refactor)(\(.+\))?:` with no `!` marker and
no `BREAKING CHANGE` in the body. Merge commits are ignored — a squashed PR
merge (`Merge pull request #15 from …`) carries no type prefix and is not
evidence of anything.

Everything else refuses: `feat:` and `perf:` (a user can tell the
difference in the terminal, so it is a minor by this repo's rules), any
unrecognised prefix (conservative default — an unclassifiable commit is not
a patch until a human says so), an empty range, and a range whose HEAD has
no green `ci` run.

## 2. What quick skips, and why each is safe

| Full-mode step | Quick mode | Why |
| --- | --- | --- |
| Local gate: `fmt`, `clippy`, `test`, `node --test` | replaced by "CI green on HEAD", checked by preflight | The release commit touches only version files; the code tree is the one CI already passed |
| `npm pack --dry-run` | **kept** | The only gate item CI does not cover: `ci.yml` runs `fmt`, `clippy`, `cargo test --locked`, `test:node` and e2e, and nothing reads `package.json`'s `files` whitelist or builds the tarball. A fix that moves a file `postinstall.js` requires is green everywhere and still ships a package that fails to install. Also the only failure that cannot be undone — a published npm version is immutable and `latest` has already moved. Two seconds |
| `node scripts/check-version.js` | kept | Instant, and guards exactly what quick mode changes — a version drift is one of the two mistakes this repo has actually made (`492012d`) |
| README `## Highlights` section | not touched | A patch usually changes nothing it describes. When it does, `bump-version.mjs` runs first (it refuses a dirty tree), then the edit, then one commit of four files. Other docs are still updated when the fix changes documented behaviour |
| Wait for `ci` green on the release commit before tagging | **tag immediately** | See below |
| Re-confirm `HEAD == origin/master` before tagging | **kept, and new in both modes** | Preflight checks this before the bump, so it says nothing about the release commit. A PR merging mid-release rejects the push, and the natural `git pull --rebase` recovery tags a commit that was never classified. Not in `release-preflight.mjs`: it runs before the bump, which is the wrong moment |
| Post-release verification | kept, scripted (§3.4) | Cheap, and a script cannot answer from memory |
| Release notes | kept, shorter | A patch whose notes do not say what was fixed is as useless as a minor's |
| Four-auditor landing audit | replaced by scripted version sync (§3.3) | Numbers are the historical drift source and are now closed by construction |

**Decision (open to reversal at review): tag without waiting for `ci` on the
release commit.** The release workflow's own `check` job re-runs versions
and tests before `build` and `publish`, so a failure produces a dead tag,
never a partial publish. Recovery is already documented in the skill
(`git tag -d` + re-create) and is safe until npm publish succeeds. This
trades a rare tag cleanup for 5–10 minutes on every patch.

**Caveat, recorded rather than glossed over.** Swapping the local gate for
CI is a move between two different check surfaces, not a strict narrowing.
The local gate runs on Windows and catches things a Linux CI never will —
a `#[cfg(unix)]`-only test module leaves `use super::*` unused on Windows,
which fails local `clippy -D warnings` while CI stays green. Conversely CI
covers targets that cannot be built locally. For a release commit that
touches only version files the practical difference is nil, which is why
the swap is acceptable here and nowhere else.

## 3. Scripts

Each script is a pure core plus a thin IO shell, so the core is testable
without git, `gh`, Docker or npm. `package.json` in `pi` uses an explicit
`files` whitelist, so nothing added to `scripts/` ships in the npm tarball.

### 3.1 `scripts/release-preflight.mjs` (pi) — read-only

Fetches `origin/master`, then reports:

- working tree clean;
- `HEAD == origin/master`;
- the commit range since the last tag, classified per §1;
- current version and the computed next patch version;
- the `ci` run for HEAD's sha: green / failed / absent.

Prints a verdict line, `quick: allowed (X.Y.Z)` or
`quick: refused (<reason>)`, and exits 0 in both cases — a refusal is a
normal outcome, not a script error. Exit 2 is reserved for the script
itself failing (no `gh`, not a repository).

Every `git` and `gh` call runs with `cwd` set to the repository root
resolved from `import.meta.url`, never the inherited working directory —
same as `bump-version.mjs`. `release-verify.mjs` does the same. The quick
checklist tells the operator to `cd` into the landing-page repo, and a
shell's working directory persists, so reading `package.json` from one repo
while classifying another repo's commits is a reachable state, not a
hypothetical one.

Testable core: `classifyRange(commits) -> {bump, quickAllowed, reason}` and
`nextPatch(version)`.

### 3.2 `scripts/bump-version.mjs <X.Y.Z>` (pi) — mutating

Refuses a dirty tree. Rewrites `[workspace.package] version` in
`Cargo.toml` and the top-level `version` in `package.json` by targeted
replacement, not by re-serialising — a reformatted `package.json` in a
release commit is noise. Runs `cargo update --workspace`.

Then two assertions, both fatal:

- `git diff --name-only` is exactly `Cargo.toml`, `package.json`,
  `Cargo.lock`. Anything else means the release commit is not
  version-only, and quick mode must fall back to the full local gate.
- every changed line in `Cargo.lock` is a `version = "…"` line moving from
  the old version to the new one, **and** belongs to a workspace crate. The
  diff is read at `git diff -U3`, not `-U0` as the plan first said: the
  ownership check finds the owning crate through the nearest preceding
  `name = "…"` line, and at `-U0` that context line is not in the hunk at
  all. Any other change means the lockfile picked up an unrelated dependency
  bump, which belongs in its own commit.

Finishes by running the existing `check-version.js`.

Exit codes follow the same convention as the other three scripts: 1 is a
refusal the operator must act on (dirty tree, a non-version-only diff, a
foreign lockfile change, a `check-version.js` mismatch); 2 is the script
being unable to complete (no `cargo` or `node` on PATH, a registry outage).
If it fails after writing `Cargo.toml` and `package.json`, it prints the
`git checkout --` line that restores the tree — otherwise the dirty-tree
guard blocks the retry with no stated way out.

Testable core: `bumpCargoToml(text, v)`, `bumpPackageJson(text, v)`,
`assertLockfileDiff(diffText, from, to)`, `failureReport(msg, wrote)`,
`exitCodeFor(err)`.

### 3.3 `scripts/sync-version.mjs <X.Y.Z>` (rpi-deploy-site) — mutating

Wired as `npm run sync-version -- X.Y.Z`, next to the existing
`npm run og`.

The landing hardcodes the version in three places in `src/index.html`: the
`og:image` cache-busting query (`?v=`), the hero terminal line
(`agent 0.25.1 (api v1)`) and the quick-start output (`rpi 0.25.1`). The
script does not encode those three locations. It works by an invariant
instead:

1. collect every `\d+\.\d+\.\d+` match under `src/`, in every file the
   content sniffer does not judge binary (a NUL byte in the first 8 KiB).
   Not an extension whitelist: a whitelist silently misses the next file
   type someone adds — `site.webmanifest`, a `.json` — and a missed file is
   exactly the stale version string this script exists to make impossible.
   Sniffing content is what makes the guarantee below real rather than
   conditional on somebody maintaining a list;
2. if the matched values are **not all identical**, fail and list every
   `file:line` — a foreign semver has appeared on the page and a human
   decides what it is;
3. if they are all identical and already equal the target, report
   "in sync" and exit 0 (idempotent);
4. otherwise replace all of them and print each replacement with its
   `file:line` and surrounding context.

Today `src/` contains exactly three semver matches, all `0.25.1`, and no
foreign semver at all — so the invariant holds and the failure branch is
not a theoretical nicety.

What this buys: after the script runs, a stale version string cannot
survive anywhere under `src/` **by construction**, including a fourth
occurrence somebody adds later. That is what makes reducing the audit to a
version sync honest rather than hopeful. It also updates the `og:image`
`?v=` automatically, since that is one of the matches.

Because the scan is not limited to `index.html`, the script also warns when
any replacement lands elsewhere, naming the CDN purge that such a deploy
requires (§4). Exit codes match the pi scripts: 1 for a refusal (mixed or
missing semver), 2 for the script being unable to run — a wrong working
directory, an unwritable file — never a raw Node stack trace.

Testable core: `syncVersions(files, target)` over an in-memory
`{path: content}` map — no filesystem — and `purgeWarning(replacements)`.

### 3.4 `scripts/release-verify.mjs <X.Y.Z>` (pi) — read-only

One pass/fail over what is currently four separate one-liners:

- the `release` workflow run for `vX.Y.Z`: every job green. Job names are
  **matrix-expanded** — a real run reports six entries (`check`, three
  `build (<os>, <triple>)` instances, `release`, `npm-publish`), not four,
  so the check matches a matrix parent by the `<name> (` prefix and verifies
  every instance it finds. The " (" separator is required, so a job named
  `builder` cannot satisfy `build`. The reported count is the number of
  entries actually verified, not the number of required names — "all four
  jobs green" is the myth that already caused one defect here, and a message
  claiming four while GitHub shows six is exactly the mismatch this script
  exists to prevent;
- `gh release view vX.Y.Z`: three archives named
  `rpi-vX.Y.Z-<triple>.*` plus `SHA256SUMS`. The release does not exist for
  the first 5-10 minutes after the tag, so `release not found` is reported
  as a failing `assets` check with the other results still printed, never as
  a crash. Genuine `gh` failures (missing binary, expired token, HTTP
  401/403, network) stay fatal at exit 2;
- `npm view rpi-deploy version` equals `X.Y.Z`;
- `docker run --rm node:20-slim npx -y rpi-deploy@X.Y.Z --version` prints
  `rpi X.Y.Z`. The container is not optional: a local machine can have a
  global install or an npx cache that shadows the resolution and passes
  against stale state. Elapsed time is reported, and a run longer than
  ~90s is flagged — that means npx fell back to a cargo build instead of
  the prebuilt binary.

Exits non-zero on any failure.

## 4. Quick-mode checklist

1. `node scripts/release-preflight.mjs` → `quick: allowed (X.Y.Z)`.
2. `node scripts/bump-version.mjs X.Y.Z`.
3. `npm pack --dry-run` — kept; see §2.
4. Commit `chore: release X.Y.Z`, push.
5. `git fetch origin master`, then `git rev-parse HEAD` ==
   `git rev-parse origin/master` — re-confirmed here, after the release
   commit exists; see §2.
6. `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.
7. `gh run watch <run>`, then `node scripts/release-verify.mjs X.Y.Z`. The
   wait is part of the step: the GitHub Release is created by the `release`
   job minutes after the tag, so verifying immediately reports a release
   that simply does not exist yet.
8. Rewrite the release notes: a **What changed** section with one bullet
   per `fix:` commit in the range, each stating the old and the new
   behaviour. `gh release edit vX.Y.Z --notes-file notes.md`.
9. Landing: `git pull` → `npm run sync-version -- X.Y.Z` → `npm run og` →
   commit → push → `rpi deploy` → confirm the live page and re-fetch
   `og:image`.

No CDN purge — **conditional on `sync-version` reporting every replacement
in `src/index.html`**, which is the case today and is what the claim rests
on: `index.html` is never edge-cached, and `og.png` is reached only through
the versioned `?v=` query the sync script just updated. But §3.3's scan is
deliberately not limited to `index.html`, so a version string added later to
`copy.js`, `styles.css` or `assets/` would be synced and would need a purge.
The script prints each replacement with its file, and warns explicitly when
one lands outside `index.html` — read that output rather than assuming. A
purge is likewise required in full mode when a deploy touches `styles.css`,
`copy.js` or other assets under `src/assets/`.

## 5. Changes to the full mode

The full mode keeps every step it has, but three of them stop being
prose-driven:

- version bump → `bump-version.mjs`;
- post-release verification → `release-verify.mjs`;
- the landing audit's version checks → `sync-version.mjs`, run **before**
  the four auditors so they spend their attention on feature text, CLI
  transcripts and `llms.txt` rather than on re-reading numbers.

`release-preflight.mjs` runs there too; its `minor` / `major` verdict does
not block anything, it informs the bump decision.

## 6. Testing

`node --test`, alongside the existing `scripts/postinstall.test.js` and
`install.test.mjs`. The pure cores make this straightforward:

- `classifyRange`: a `fix:`-only range → quick allowed; a range containing
  `feat:` → refused with `minor`; `fix!:` and a `BREAKING CHANGE` body →
  refused; merge commits ignored; an unknown prefix → refused; an empty
  range → refused. `nextPatch("0.25.1") === "0.25.2"`.
- `bumpCargoToml` / `bumpPackageJson`: the version changes and nothing else
  in the file does. `assertLockfileDiff`: a workspace-only diff passes; a
  diff carrying a third-party bump fails.
- `syncVersions`: mixed semvers → error naming every location; already at
  target → no writes; replacement covers all matches; running twice is a
  no-op. `purgeWarning`: silent while every replacement is in
  `index.html`, and names the purge, deduplicated and sorted, as soon as one
  is not.
- `classifyReleaseViewFailure`: gh's "release not found" wording is the
  not-yet-published state; a missing binary, an expired token, HTTP 401/403,
  a network failure and an empty message are all genuine errors. Tested as
  pure classification — the point is the decision, not the subprocess.
- `failureReport` / `exitCodeFor`: the recovery line appears only after the
  files were written, and a `BumpError` is exit 1 while a spawn failure is
  exit 2.
- `sync-version`'s entrypoint, spawned in a temp directory: a mixed-semver
  tree exits 1, a missing `src/` exits 2, and neither prints a stack trace.

The site repo has no tests today, so `sync-version.test.mjs` is its first —
`node --test` needs no new dependency.

## 7. Documentation

- `.claude/skills/release/SKILL.md` — a quick-mode section, the checklist
  in §4, and the existing steps rewritten to call the scripts. The
  "why" paragraphs stay: they are what stops the steps being answered from
  memory.
- `rpi-deploy-site/docs/landing-audit.md` — record that version sync is
  scripted and runs before the auditors, and that patch releases reach the
  landing through the sync alone.

## Risks

- **Non-numeric landing drift accumulating across consecutive patches.**
  The sync script guarantees versions only; feature text, CLI transcripts
  and `llms.txt` are still checked by the four auditors, which now run on
  minor and major releases only. Accepted on the grounds that numbers have
  historically been the drift (a quick-start stuck at `rpi 0.12.0` for five
  releases) and are now closed by construction. If a long run of patches
  ever ships without an intervening minor, the audit debt is real and a
  full audit should be run deliberately.
- **A dead tag from tagging before `ci`.** Bounded: `check` gates `build`
  and `publish`, so nothing is published, and the recovery is two commands
  while npm publish has not succeeded.
- **Preflight trusting a `ci` run that does not cover the release
  platform.** Documented in §2 rather than solved; the release commit's
  content makes it moot.
- **`bump-version.mjs` regex-editing `Cargo.toml`.** It reuses the same
  `[workspace.package]` pattern `check-version.js` already relies on, and
  `check-version.js` runs immediately after as the cross-check.

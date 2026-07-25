# Release Quick Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `/release quick` mode for patch releases, backed by four
scripts that replace the hand-driven steps of the release checklist in both
modes.

**Architecture:** Each script is a pure core (string/array in, result out)
plus a thin IO shell that talks to git, `gh`, npm and Docker. Only the pure
core is unit-tested, which is why every script exports its logic separately
from its `main()`. Three scripts live in this repo's `scripts/`; the landing
version sync lives in the sibling `rpi-deploy-site` repo next to
`generate-og.mjs`.

**Tech Stack:** Node.js ESM (`.mjs`), `node:test`, `node:assert/strict`, no
new dependencies in either repo.

**Spec:** `docs/superpowers/specs/2026-07-25-release-quick-mode-design.md`

## Global Constraints

- **Work happens in a worktree.** This repo's work is done in
  `C:\Users\Khmil\RustProjects\pi\.worktrees\release-quick-mode`, on branch
  `feat/release-quick-mode` based on `master`. Never `cd` to the parent
  checkout `C:\Users\Khmil\RustProjects\pi` — it sits on an unrelated
  branch. Commit your task's work there with `git add <exact paths>`; never
  `git add .` or `git add -A`.
- **The site repo is not a worktree.** Task 1 works directly in
  `C:\Users\Khmil\RustProjects\rpi-deploy-site` on branch `main`, whose tree
  is clean. Commit there with explicit paths too.
- **No Rust file is touched by this plan.** Do not run `cargo fmt --all`,
  and do not "fix" any Rust file you happen to see.
- **New scripts are ESM `.mjs`.** `pi/package.json` has no `"type"` field,
  so `.js` there is CommonJS (`scripts/check-version.js`,
  `scripts/postinstall.js`). New files use the `.mjs` extension so they can
  use `export`/`import`, matching the existing `scripts/install.test.mjs`.
  `rpi-deploy-site/package.json` has `"type": "module"`, so `.mjs` there is
  ESM as well.
- **No new dependencies.** Both repos must remain installable without any
  `package.json` dependency change.
- **Semver regex is exactly `/\b\d+\.\d+\.\d+\b/g`** wherever a version is
  scanned or replaced. Do not use a looser or stricter pattern in one place
  and not another.
- **Version floor for a released tag** is the three-part form `X.Y.Z`
  validated by `/^\d+\.\d+\.\d+$/`. Tags are `v` + that string, lowercase.
- **Release asset names** (verified against `v0.25.1`):
  `rpi-v<X.Y.Z>-x86_64-pc-windows-msvc.zip`,
  `rpi-v<X.Y.Z>-x86_64-unknown-linux-musl.tar.gz`,
  `rpi-v<X.Y.Z>-aarch64-unknown-linux-musl.tar.gz`, `SHA256SUMS`.
- **Release workflow jobs** are exactly four: `check`, `build`, `release`,
  `npm-publish`.
- **Quick-mode commit types:** allowed
  `fix docs chore ci test style refactor`; refusing types include
  `feat perf`; merge commits are excluded from classification entirely.

Tasks 1–4 are independent and may run in parallel. Task 5 depends on all of
them.

---

### Task 1: Landing version sync script

**Repo:** `C:\Users\Khmil\RustProjects\rpi-deploy-site` (a sibling of this
one — every path in this task is relative to that directory, not to `pi`).

**Files:**
- Create: `scripts/sync-version.mjs`
- Create: `scripts/sync-version.test.mjs`
- Modify: `package.json` (add one `scripts` entry)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `npm run sync-version -- <X.Y.Z>`, used by Task 5's
  documentation. Exported for tests:
  `scanVersions(files) -> Array<{path, line, value, text}>`,
  `syncVersions(files, target) -> {status, from, to, replacements, files}`,
  `class VersionSyncError extends Error`.
  `files` is a plain object mapping path to file content.

- [ ] **Step 1: Write the failing test**

Create `scripts/sync-version.test.mjs`:

```js
import test from "node:test";
import assert from "node:assert/strict";

import { scanVersions, syncVersions, VersionSyncError } from "./sync-version.mjs";

const page = {
  "src/index.html": [
    '<meta property="og:image" content="https://rpi.iiskelo.com/assets/og.png?v=0.25.1">',
    '<span class="tline">agent 0.25.1 (api v1)</span>',
    "rpi 0.25.1",
  ].join("\n"),
};

test("scanVersions reports every match with its location", () => {
  const hits = scanVersions(page);
  assert.equal(hits.length, 3);
  assert.deepEqual(
    hits.map((h) => [h.path, h.line, h.value]),
    [
      ["src/index.html", 1, "0.25.1"],
      ["src/index.html", 2, "0.25.1"],
      ["src/index.html", 3, "0.25.1"],
    ],
  );
});

test("syncVersions rewrites every occurrence", () => {
  const result = syncVersions(page, "0.25.2");
  assert.equal(result.status, "synced");
  assert.equal(result.from, "0.25.1");
  assert.equal(result.to, "0.25.2");
  assert.equal(result.replacements.length, 3);
  const out = result.files["src/index.html"];
  assert.ok(out.includes("og.png?v=0.25.2"));
  assert.ok(out.includes("agent 0.25.2 (api v1)"));
  assert.ok(out.includes("rpi 0.25.2"));
  assert.ok(!out.includes("0.25.1"));
});

test("syncVersions is idempotent", () => {
  const once = syncVersions(page, "0.25.2");
  const twice = syncVersions(once.files, "0.25.2");
  assert.equal(twice.status, "in-sync");
  assert.deepEqual(twice.files, {});
});

test("syncVersions refuses a page carrying a foreign semver", () => {
  const mixed = {
    "src/index.html": "rpi 0.25.1",
    "src/llms.txt": "requires node 20.11.0",
  };
  assert.throws(
    () => syncVersions(mixed, "0.25.2"),
    (err) => {
      assert.ok(err instanceof VersionSyncError);
      assert.match(err.message, /0\.25\.1/);
      assert.match(err.message, /20\.11\.0/);
      assert.match(err.message, /src\/llms\.txt:1/);
      return true;
    },
  );
});

test("syncVersions refuses a page with no version at all", () => {
  assert.throws(
    () => syncVersions({ "src/index.html": "no version here" }, "0.25.2"),
    VersionSyncError,
  );
});

test("syncVersions validates the target version", () => {
  assert.throws(() => syncVersions(page, "0.25"), VersionSyncError);
  assert.throws(() => syncVersions(page, "v0.25.2"), VersionSyncError);
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run from the site repo root:
```bash
node --test scripts/sync-version.test.mjs
```
Expected: FAIL — `Cannot find module ... sync-version.mjs`.

- [ ] **Step 3: Write the implementation**

Create `scripts/sync-version.mjs`:

```js
#!/usr/bin/env node
// Syncs the released rpi version into the landing page.
//
// The page hardcodes the version in three places today (the og:image cache
// buster, the hero terminal line and the quick-start output), but this
// script does not encode those locations. It works by an invariant: every
// semver string under src/ must be the same one. That way a fourth
// occurrence added later is synced automatically, and a foreign semver is
// reported instead of being silently rewritten.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const SEMVER = /\b\d+\.\d+\.\d+\b/g;
const SRC_DIR = "src";
const EXTENSIONS = new Set([".html", ".txt", ".xml", ".js", ".css"]);

export class VersionSyncError extends Error {}

export function scanVersions(files) {
  const hits = [];
  for (const [file, content] of Object.entries(files)) {
    content.split("\n").forEach((text, index) => {
      for (const match of text.matchAll(SEMVER)) {
        hits.push({ path: file, line: index + 1, value: match[0], text: text.trim() });
      }
    });
  }
  return hits;
}

export function syncVersions(files, target) {
  if (!/^\d+\.\d+\.\d+$/.test(target)) {
    throw new VersionSyncError(`invalid target version: ${target} (expected X.Y.Z)`);
  }

  const hits = scanVersions(files);
  if (hits.length === 0) {
    throw new VersionSyncError(
      `no version strings found under ${SRC_DIR}/ — the landing is expected to show the released version`,
    );
  }

  const distinct = [...new Set(hits.map((h) => h.value))];
  if (distinct.length > 1) {
    const where = hits.map((h) => `  ${h.path}:${h.line}  ${h.value}  ${h.text}`).join("\n");
    throw new VersionSyncError(
      `mixed version strings (${distinct.join(", ")}) — a foreign semver is on the page, resolve by hand:\n${where}`,
    );
  }

  const from = distinct[0];
  if (from === target) {
    return { status: "in-sync", from, to: target, replacements: [], files: {} };
  }

  const changed = {};
  for (const [file, content] of Object.entries(files)) {
    if (SEMVER.test(content)) {
      SEMVER.lastIndex = 0;
      changed[file] = content.replace(SEMVER, target);
    }
    SEMVER.lastIndex = 0;
  }

  return {
    status: "synced",
    from,
    to: target,
    replacements: hits.map((h) => ({ ...h, value: target })),
    files: changed,
  };
}

function readTree(dir) {
  const files = {};
  for (const entry of fs.readdirSync(dir, { withFileTypes: true, recursive: true })) {
    if (!entry.isFile()) continue;
    if (!EXTENSIONS.has(path.extname(entry.name))) continue;
    const full = path.join(entry.parentPath ?? entry.path, entry.name);
    files[path.relative(".", full).split(path.sep).join("/")] = fs.readFileSync(full, "utf8");
  }
  return files;
}

function main() {
  const target = process.argv[2];
  if (!target) {
    console.error("usage: npm run sync-version -- <X.Y.Z>");
    process.exit(2);
  }

  let result;
  try {
    result = syncVersions(readTree(SRC_DIR), target);
  } catch (err) {
    if (err instanceof VersionSyncError) {
      console.error(`sync-version: ${err.message}`);
      process.exit(1);
    }
    throw err;
  }

  if (result.status === "in-sync") {
    console.log(`sync-version: already at ${target}, nothing to do`);
    return;
  }

  for (const [file, content] of Object.entries(result.files)) {
    fs.writeFileSync(file, content);
  }
  console.log(`sync-version: ${result.from} -> ${result.to}`);
  for (const hit of result.replacements) {
    console.log(`  ${hit.path}:${hit.line}  ${hit.text.replace(new RegExp(result.from, "g"), result.to)}`);
  }
  console.log(`sync-version: ${result.replacements.length} replacement(s) in ${Object.keys(result.files).length} file(s)`);
  console.log("sync-version: run `npm run og` next — the hero terminal shows the version");
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  main();
}
```

The `main()` guard uses `fileURLToPath` and `path.resolve` on both sides —
the only form that is correct on Windows, where `process.argv[1]` is a
backslash path and `import.meta.url` is a `file:///C:/...` URL. The same
guard appears in all four scripts of this plan; do not vary it. It is what
lets the test file import the module without running `main()` — verify
that in Step 4.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
node --test scripts/sync-version.test.mjs
```
Expected: PASS, 6/6. If importing the module printed a usage error, the
`main()` guard is wrong — fix it before continuing.

- [ ] **Step 5: Wire the npm script**

In `package.json`, add to `scripts` (keep the existing entries):

```json
"sync-version": "node scripts/sync-version.mjs"
```

- [ ] **Step 6: Verify against the real page**

```bash
node scripts/sync-version.mjs 0.25.1
```
Expected: `sync-version: already at 0.25.1, nothing to do`, and
`git status --porcelain` stays empty. This confirms the real `src/` tree
satisfies the invariant (three matches, all identical) without modifying
anything.

Then check the failure branch reads well:
```bash
node scripts/sync-version.mjs 0.25.2 && git diff --stat && git checkout -- src/
```
Expected: three replacements listed with `file:line`, `src/index.html`
modified, then restored by the checkout. `git checkout -- src/` is safe
here: this repo's tree is clean apart from what this task creates.

- [ ] **Step 7: Commit and report**

```bash
git add scripts/sync-version.mjs scripts/sync-version.test.mjs package.json
git commit -m "feat: add sync-version script for release version sync"
```

Report the test output and the two command outputs from Step 6.

---

### Task 2: Release preflight script

**Repo:** `C:\Users\Khmil\RustProjects\pi` (this repo).

**Files:**
- Create: `scripts/release-preflight.mjs`
- Create: `scripts/release-preflight.test.mjs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `node scripts/release-preflight.mjs`, printing a final line
  `quick: allowed (X.Y.Z)` or `quick: refused (<reason>)`. Exported for
  tests: `classifyRange(commits) -> {bump, quickAllowed, reason}`,
  `nextPatch(version) -> string`,
  `ciStatusForSha(runs, sha) -> {ok, reason}`,
  `parseCommits(rawLog) -> Array<{sha, subject, body}>`.
  A commit is `{sha, subject, body}`; merge commits never reach
  `classifyRange` because the log is read with `--no-merges`.

- [ ] **Step 1: Write the failing test**

Create `scripts/release-preflight.test.mjs`:

```js
import test from "node:test";
import assert from "node:assert/strict";

import {
  classifyRange,
  nextPatch,
  ciStatusForSha,
  parseCommits,
} from "./release-preflight.mjs";

const commit = (subject, body = "") => ({ sha: "a".repeat(40), subject, body });

test("a fix-only range allows quick", () => {
  const result = classifyRange([
    commit("fix(scheduler): do not treat an already-finished deploy as cancellable"),
    commit("docs: correct the secrets example"),
    commit("chore(deps): bump serde"),
  ]);
  assert.equal(result.bump, "patch");
  assert.equal(result.quickAllowed, true);
});

test("a feat commit forces minor and refuses quick", () => {
  const result = classifyRange([commit("fix: a real fix"), commit("feat: new --json flag")]);
  assert.equal(result.bump, "minor");
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /feat/);
});

test("perf counts as minor — a user can see the difference", () => {
  const result = classifyRange([commit("perf: stream logs without buffering")]);
  assert.equal(result.bump, "minor");
  assert.equal(result.quickAllowed, false);
});

test("a bang marker refuses quick even on an allowed type", () => {
  const result = classifyRange([commit("refactor!: drop the legacy agent endpoint")]);
  assert.equal(result.bump, "minor");
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /breaking/i);
});

test("a BREAKING CHANGE body refuses quick", () => {
  const result = classifyRange([
    commit("fix: reject invalid modes", "BREAKING CHANGE: 0777 is no longer accepted"),
  ]);
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /breaking/i);
});

test("an unclassifiable subject refuses quick", () => {
  const result = classifyRange([commit("update stuff")]);
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /update stuff/);
});

test("an empty range refuses quick", () => {
  const result = classifyRange([]);
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /no commits/i);
});

test("nextPatch increments only the patch component", () => {
  assert.equal(nextPatch("0.25.1"), "0.25.2");
  assert.equal(nextPatch("0.25.9"), "0.25.10");
  assert.equal(nextPatch("1.0.0"), "1.0.1");
  assert.throws(() => nextPatch("0.25"), /invalid/i);
});

test("parseCommits splits the NUL-delimited log", () => {
  const raw = ["abc123\u0000fix: one\u0000", "def456\u0000fix: two\u0000BREAKING CHANGE: x"].join("\u0001");
  const commits = parseCommits(raw);
  assert.equal(commits.length, 2);
  assert.equal(commits[0].subject, "fix: one");
  assert.equal(commits[1].body, "BREAKING CHANGE: x");
});

test("parseCommits returns nothing for an empty log", () => {
  assert.deepEqual(parseCommits(""), []);
  assert.deepEqual(parseCommits("\n"), []);
});

test("ciStatusForSha demands a completed successful run for that exact sha", () => {
  const sha = "a".repeat(40);
  assert.equal(ciStatusForSha([{ headSha: sha, status: "completed", conclusion: "success" }], sha).ok, true);
  assert.equal(ciStatusForSha([{ headSha: sha, status: "completed", conclusion: "failure" }], sha).ok, false);
  assert.equal(ciStatusForSha([{ headSha: sha, status: "in_progress", conclusion: null }], sha).ok, false);
  assert.equal(ciStatusForSha([{ headSha: "b".repeat(40), status: "completed", conclusion: "success" }], sha).ok, false);
  assert.match(ciStatusForSha([], sha).reason, /no ci run/i);
});
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
node --test scripts/release-preflight.test.mjs
```
Expected: FAIL — module not found.

- [ ] **Step 3: Write the implementation**

Create `scripts/release-preflight.mjs`:

```js
#!/usr/bin/env node
// Read-only gate for `/release`. Decides whether the range since the last
// tag is a patch (quick mode allowed) or carries something a user can see
// (minor — full mode). Prints a verdict and exits 0 either way; a refusal
// is a normal outcome, not a script error. Exit 2 means the script itself
// could not run.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const QUICK_TYPES = new Set(["fix", "docs", "chore", "ci", "test", "style", "refactor"]);
const MINOR_TYPES = new Set(["feat", "perf"]);
const RECORD = "\u0001";
const FIELD = "\u0000";

export function nextPatch(version) {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!m) throw new Error(`invalid version: ${version} (expected X.Y.Z)`);
  return `${m[1]}.${m[2]}.${Number(m[3]) + 1}`;
}

export function parseCommits(raw) {
  return raw
    .split(RECORD)
    .map((chunk) => chunk.trim())
    .filter(Boolean)
    .map((chunk) => {
      const [sha, subject, ...rest] = chunk.split(FIELD);
      return { sha, subject: (subject ?? "").trim(), body: rest.join(FIELD).trim() };
    });
}

export function classifyRange(commits) {
  if (commits.length === 0) {
    return { bump: "none", quickAllowed: false, reason: "no commits since the last tag" };
  }

  for (const c of commits) {
    const m = /^([a-z]+)(\([^)]*\))?(!)?:/.exec(c.subject);
    if (!m) {
      return {
        bump: "unknown",
        quickAllowed: false,
        reason: `unclassifiable commit subject: "${c.subject}"`,
      };
    }
    if (m[3] === "!" || /^BREAKING CHANGE:/m.test(c.body)) {
      return {
        bump: "minor",
        quickAllowed: false,
        reason: `breaking change declared by "${c.subject}"`,
      };
    }
    if (MINOR_TYPES.has(m[1])) {
      return {
        bump: "minor",
        quickAllowed: false,
        reason: `"${m[1]}" commit gives users something new: "${c.subject}"`,
      };
    }
    if (!QUICK_TYPES.has(m[1])) {
      return {
        bump: "unknown",
        quickAllowed: false,
        reason: `unrecognised commit type "${m[1]}": "${c.subject}"`,
      };
    }
  }

  return { bump: "patch", quickAllowed: true, reason: `${commits.length} commit(s), all patch-level` };
}

export function ciStatusForSha(runs, sha) {
  const run = runs.find((r) => r.headSha === sha);
  const short = sha.slice(0, 7);
  if (!run) return { ok: false, reason: `no ci run found for ${short}` };
  if (run.status !== "completed") return { ok: false, reason: `ci run for ${short} is ${run.status}` };
  if (run.conclusion !== "success") return { ok: false, reason: `ci run for ${short} concluded ${run.conclusion}` };
  return { ok: true, reason: `ci green for ${short}` };
}

function git(...args) {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function main() {
  const root = path.resolve(fileURLToPath(import.meta.url), "..", "..");
  const checks = [];
  let quick = { quickAllowed: false, reason: "preflight did not complete" };

  try {
    git("fetch", "origin", "master", "--tags", "--quiet");
  } catch {
    checks.push("!  could not fetch origin/master — results may be stale");
  }

  const dirty = git("status", "--porcelain");
  checks.push(dirty ? `x  working tree is dirty:\n${dirty}` : "ok working tree clean");

  const head = git("rev-parse", "HEAD");
  const upstream = git("rev-parse", "origin/master");
  checks.push(head === upstream ? "ok HEAD == origin/master" : `x  HEAD ${head.slice(0, 7)} != origin/master ${upstream.slice(0, 7)}`);

  const lastTag = git("describe", "--tags", "--abbrev=0");
  const raw = git("log", "--no-merges", `--format=%H${FIELD}%s${FIELD}%b${RECORD}`, `${lastTag}..HEAD`);
  const commits = parseCommits(raw);
  const verdict = classifyRange(commits);
  checks.push(`   range ${lastTag}..HEAD: ${commits.length} non-merge commit(s), bump = ${verdict.bump}`);
  for (const c of commits) checks.push(`     ${c.sha.slice(0, 7)} ${c.subject}`);

  const pkg = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  const target = verdict.bump === "patch" ? nextPatch(pkg.version) : null;
  checks.push(`   current version ${pkg.version}${target ? `, next patch ${target}` : ""}`);

  let ci = { ok: false, reason: "gh unavailable" };
  try {
    const runs = JSON.parse(
      execFileSync(
        "gh",
        ["run", "list", "--workflow", "ci", "--branch", "master", "--limit", "20", "--json", "headSha,status,conclusion"],
        { encoding: "utf8" },
      ),
    );
    ci = ciStatusForSha(runs, head);
  } catch (err) {
    ci = { ok: false, reason: `could not read ci runs (${err.message.split("\n")[0]})` };
  }
  checks.push(`${ci.ok ? "ok" : "x "} ${ci.reason}`);

  const blockers = [];
  if (dirty) blockers.push("working tree is dirty");
  if (head !== upstream) blockers.push("HEAD is not origin/master");
  if (!verdict.quickAllowed) blockers.push(verdict.reason);
  if (!ci.ok) blockers.push(ci.reason);
  quick = blockers.length === 0 ? { quickAllowed: true } : { quickAllowed: false, reason: blockers.join("; ") };

  console.log(checks.join("\n"));
  console.log("");
  console.log(`bump: ${verdict.bump}`);
  console.log(quick.quickAllowed ? `quick: allowed (${target})` : `quick: refused (${quick.reason})`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  try {
    main();
  } catch (err) {
    console.error(`release-preflight: ${err.message}`);
    process.exit(2);
  }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
node --test scripts/release-preflight.test.mjs
```
Expected: PASS, 11/11.

- [ ] **Step 5: Run it against the real repository**

```bash
node scripts/release-preflight.mjs
```
Expected: it completes and prints a `bump:` line and a `quick:` line. The
repository currently has an unrelated dirty tree, so `quick: refused
(working tree is dirty; ...)` is the correct output — that is the check
working, not a failure. Confirm the range listing shows the commits since
`v0.25.1` and that `docs:`-only commits classify as `patch`.

- [ ] **Step 6: Commit and report**

```bash
git add scripts/release-preflight.mjs scripts/release-preflight.test.mjs
git commit -m "feat(release): add release-preflight gate script"
```

Report the test output and the real-repository output verbatim.

Note on Step 5's real run: this worktree sits on branch
`feat/release-quick-mode`, not `master`, so `HEAD == origin/master` will be
false and the verdict will be `quick: refused`. That is the check working.
Confirm the range listing and the `bump:` line are correct; do not change
the script to make the verdict come out `allowed` here.

---

### Task 3: Version bump script

**Repo:** `C:\Users\Khmil\RustProjects\pi` (this repo).

**Files:**
- Create: `scripts/bump-version.mjs`
- Create: `scripts/bump-version.test.mjs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `node scripts/bump-version.mjs <X.Y.Z>`. Exported for tests:
  `bumpCargoToml(text, version) -> string`,
  `bumpPackageJson(text, version) -> string`,
  `assertLockfileDiff(diffText, from, to) -> void` (throws on any change
  that is not a workspace version line),
  `assertChangedFiles(names) -> void` (throws unless the set is exactly
  `Cargo.toml`, `package.json`, `Cargo.lock`),
  `class BumpError extends Error`.

- [ ] **Step 1: Write the failing test**

Create `scripts/bump-version.test.mjs`:

```js
import test from "node:test";
import assert from "node:assert/strict";

import {
  bumpCargoToml,
  bumpPackageJson,
  assertLockfileDiff,
  assertChangedFiles,
  BumpError,
} from "./bump-version.mjs";

const CARGO = `[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.25.1"
edition = "2021"

[workspace.dependencies]
serde = { version = "1.0.0" }
`;

const PKG = `{
  "name": "rpi-deploy",
  "version": "0.25.1",
  "description": "Deployment tool",
  "engines": { "node": ">=18.0.0" }
}
`;

test("bumpCargoToml changes only the workspace package version", () => {
  const out = bumpCargoToml(CARGO, "0.25.2");
  assert.match(out, /\[workspace\.package\]\nversion = "0\.25\.2"/);
  assert.match(out, /serde = \{ version = "1\.0\.0" \}/);
  assert.equal(out.split("\n").length, CARGO.split("\n").length);
});

test("bumpCargoToml fails when the workspace block is missing", () => {
  assert.throws(() => bumpCargoToml('[package]\nversion = "1.0.0"\n', "0.25.2"), BumpError);
});

test("bumpPackageJson changes only the top-level version", () => {
  const out = bumpPackageJson(PKG, "0.25.2");
  assert.match(out, /"version": "0\.25\.2"/);
  assert.match(out, /"node": ">=18\.0\.0"/);
  assert.equal(out.split("\n").length, PKG.split("\n").length);
  assert.equal(JSON.parse(out).version, "0.25.2");
});

test("bumpPackageJson preserves formatting byte-for-byte apart from the version", () => {
  const out = bumpPackageJson(PKG, "0.25.2");
  assert.equal(out, PKG.replace('"version": "0.25.1"', '"version": "0.25.2"'));
});

test("assertLockfileDiff accepts a workspace-only diff", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -100 +100 @@
-version = "0.25.1"
+version = "0.25.2"
@@ -120 +120 @@
-version = "0.25.1"
+version = "0.25.2"
`;
  assert.doesNotThrow(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"));
});

test("assertLockfileDiff rejects an unrelated dependency bump", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -100 +100 @@
-version = "0.25.1"
+version = "0.25.2"
@@ -300 +300 @@
-version = "1.0.210"
+version = "1.0.211"
`;
  assert.throws(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"), (err) => {
    assert.ok(err instanceof BumpError);
    assert.match(err.message, /1\.0\.211/);
    return true;
  });
});

test("assertLockfileDiff rejects added or removed non-version lines", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -100 +100,2 @@
-version = "0.25.1"
+version = "0.25.2"
+name = "brand-new-crate"
`;
  assert.throws(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"), BumpError);
});

test("assertLockfileDiff rejects an empty diff", () => {
  assert.throws(() => assertLockfileDiff("", "0.25.1", "0.25.2"), /no changes/i);
});

test("assertChangedFiles accepts exactly the three version files", () => {
  assert.doesNotThrow(() => assertChangedFiles(["Cargo.lock", "Cargo.toml", "package.json"]));
});

test("assertChangedFiles rejects anything else in the tree", () => {
  assert.throws(
    () => assertChangedFiles(["Cargo.toml", "package.json", "Cargo.lock", "crates/domain/src/lib.rs"]),
    (err) => {
      assert.match(err.message, /crates\/domain\/src\/lib\.rs/);
      return true;
    },
  );
  assert.throws(() => assertChangedFiles(["Cargo.toml", "package.json"]), BumpError);
});
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
node --test scripts/bump-version.test.mjs
```
Expected: FAIL — module not found.

- [ ] **Step 3: Write the implementation**

Create `scripts/bump-version.mjs`:

```js
#!/usr/bin/env node
// Bumps the version in the three files that must agree — Cargo.toml,
// package.json and Cargo.lock — and refuses to leave the tree in any other
// state. The two assertions are the point of the script: a version drift
// and a lockfile carrying an unrelated dependency bump are both mistakes
// this repository has room to make by hand.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const EXPECTED_FILES = ["Cargo.lock", "Cargo.toml", "package.json"];

export class BumpError extends Error {}

export function bumpCargoToml(text, version) {
  const re = /(^\[workspace\.package\][^[]*?^version\s*=\s*")([^"]+)(")/ms;
  if (!re.test(text)) {
    throw new BumpError("cannot find [workspace.package] version in Cargo.toml");
  }
  return text.replace(re, `$1${version}$3`);
}

export function bumpPackageJson(text, version) {
  const re = /^(\s*"version"\s*:\s*")([^"]+)(")/m;
  if (!re.test(text)) {
    throw new BumpError("cannot find a top-level version in package.json");
  }
  return text.replace(re, `$1${version}$3`);
}

export function assertLockfileDiff(diffText, from, to) {
  const lines = diffText.split("\n").filter((l) => /^[-+]/.test(l) && !/^(---|\+\+\+)/.test(l));
  if (lines.length === 0) {
    throw new BumpError("Cargo.lock shows no changes — did `cargo update --workspace` run?");
  }
  const offending = lines.filter((l) => {
    const m = /^([-+])version = "([^"]+)"$/.exec(l);
    if (!m) return true;
    return m[1] === "-" ? m[2] !== from : m[2] !== to;
  });
  if (offending.length > 0) {
    throw new BumpError(
      `Cargo.lock carries changes beyond the workspace version bump — commit those separately:\n${offending.map((l) => `  ${l}`).join("\n")}`,
    );
  }
}

export function assertChangedFiles(names) {
  const actual = [...names].sort();
  const expected = [...EXPECTED_FILES].sort();
  if (actual.length !== expected.length || actual.some((n, i) => n !== expected[i])) {
    throw new BumpError(
      `the release commit must touch exactly ${expected.join(", ")} — found: ${actual.join(", ") || "(nothing)"}.\n` +
        "Fall back to the full local gate: this is not a version-only release commit.",
    );
  }
}

function git(root, ...args) {
  return execFileSync("git", args, { encoding: "utf8", cwd: root }).trim();
}

function main() {
  const version = process.argv[2];
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    console.error("usage: node scripts/bump-version.mjs <X.Y.Z>");
    process.exit(2);
  }

  const root = path.resolve(fileURLToPath(import.meta.url), "..", "..");
  const dirty = git(root, "status", "--porcelain");
  if (dirty) {
    throw new BumpError(`working tree is dirty, refusing to bump:\n${dirty}`);
  }

  const cargoPath = path.join(root, "Cargo.toml");
  const pkgPath = path.join(root, "package.json");
  const from = JSON.parse(fs.readFileSync(pkgPath, "utf8")).version;

  fs.writeFileSync(cargoPath, bumpCargoToml(fs.readFileSync(cargoPath, "utf8"), version));
  fs.writeFileSync(pkgPath, bumpPackageJson(fs.readFileSync(pkgPath, "utf8"), version));
  console.log(`bump-version: ${from} -> ${version} in Cargo.toml, package.json`);

  execFileSync("cargo", ["update", "--workspace"], { cwd: root, stdio: "inherit" });

  assertLockfileDiff(git(root, "diff", "-U0", "--", "Cargo.lock"), from, version);
  console.log("bump-version: Cargo.lock diff is workspace-only");

  assertChangedFiles(git(root, "diff", "--name-only").split("\n").filter(Boolean));
  console.log("bump-version: tree contains only the three version files");

  execFileSync("node", [path.join(root, "scripts", "check-version.js")], { cwd: root, stdio: "inherit" });
  console.log(`bump-version: ready to commit \`chore: release ${version}\``);
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  try {
    main();
  } catch (err) {
    console.error(`bump-version: ${err.message}`);
    process.exit(1);
  }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
node --test scripts/bump-version.test.mjs
```
Expected: PASS, 10/10.

- [ ] **Step 5: Verify the dirty-tree guard on the real repository**

The worktree is clean, so make it dirty deliberately, confirm the guard
fires, then clean up:

```bash
echo scratch > dirty-guard-probe.txt
node scripts/bump-version.mjs 0.25.2 ; echo "exit=$?"
git status --porcelain
rm dirty-guard-probe.txt
```

Expected: exit 1 with `bump-version: working tree is dirty, refusing to
bump:` listing `?? dirty-guard-probe.txt`, and `git status --porcelain`
showing that `Cargo.toml`, `package.json` and `Cargo.lock` were **not**
modified — the guard fires before anything is written.

- [ ] **Step 6: Verify the happy path, then revert it**

```bash
node scripts/bump-version.mjs 0.25.2 ; echo "exit=$?"
git diff --stat
git checkout -- Cargo.toml package.json Cargo.lock
git status --porcelain
```

Expected: exit 0, ending with
`bump-version: ready to commit \`chore: release 0.25.2\`` and
`check-version: ok (0.25.2)` from the nested check. `git diff --stat` must
show exactly three files. The `git checkout` then restores them, and the
final `git status --porcelain` must print nothing but your new script files.

This is the only end-to-end proof that the two assertions pass on a real
`cargo update --workspace` run rather than only on the fixtures. If
`assertLockfileDiff` rejects the real lockfile, that is a bug in the
assertion — fix it, do not loosen it to "anything goes".

**Do not commit the version bump.** Only `scripts/bump-version.mjs` and
`scripts/bump-version.test.mjs` belong in this task's commit.

- [ ] **Step 7: Commit and report**

```bash
git add scripts/bump-version.mjs scripts/bump-version.test.mjs
git commit -m "feat(release): add bump-version script"
```

Report the test output, the Step 5 and Step 6 outputs, and confirmation
that `git status --porcelain` is clean afterwards.

---

### Task 4: Release verification script

**Repo:** `C:\Users\Khmil\RustProjects\pi` (this repo).

**Files:**
- Create: `scripts/release-verify.mjs`
- Create: `scripts/release-verify.test.mjs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `node scripts/release-verify.mjs <X.Y.Z>`, exit 0 when every
  check passes and non-zero otherwise. Exported for tests:
  `expectedAssets(version) -> string[]`,
  `checkAssets(actualNames, version) -> {ok, missing, extra}`,
  `checkJobs(jobs) -> {ok, reason}`,
  `checkNpmVersion(actual, version) -> {ok, reason}`,
  `checkSmokeOutput(stdout, version, elapsedMs) -> {ok, reason, slow}`.

- [ ] **Step 1: Write the failing test**

Create `scripts/release-verify.test.mjs`:

```js
import test from "node:test";
import assert from "node:assert/strict";

import {
  expectedAssets,
  checkAssets,
  checkJobs,
  checkNpmVersion,
  checkSmokeOutput,
} from "./release-verify.mjs";

test("expectedAssets names the three archives and the checksum file", () => {
  assert.deepEqual(expectedAssets("0.25.2"), [
    "SHA256SUMS",
    "rpi-v0.25.2-aarch64-unknown-linux-musl.tar.gz",
    "rpi-v0.25.2-x86_64-pc-windows-msvc.zip",
    "rpi-v0.25.2-x86_64-unknown-linux-musl.tar.gz",
  ]);
});

test("checkAssets passes on the exact published set", () => {
  const result = checkAssets(expectedAssets("0.25.2"), "0.25.2");
  assert.equal(result.ok, true);
  assert.deepEqual(result.missing, []);
});

test("checkAssets reports a missing archive", () => {
  const names = expectedAssets("0.25.2").filter((n) => !n.endsWith(".zip"));
  const result = checkAssets(names, "0.25.2");
  assert.equal(result.ok, false);
  assert.deepEqual(result.missing, ["rpi-v0.25.2-x86_64-pc-windows-msvc.zip"]);
});

test("checkAssets reports an asset from another version", () => {
  const names = [...expectedAssets("0.25.2"), "rpi-v0.25.1-x86_64-pc-windows-msvc.zip"];
  const result = checkAssets(names, "0.25.2");
  assert.equal(result.ok, false);
  assert.deepEqual(result.extra, ["rpi-v0.25.1-x86_64-pc-windows-msvc.zip"]);
});

test("checkJobs demands all four jobs green", () => {
  const green = ["check", "build", "release", "npm-publish"].map((name) => ({
    name,
    status: "completed",
    conclusion: "success",
  }));
  assert.equal(checkJobs(green).ok, true);
});

test("checkJobs reports a failed job by name", () => {
  const jobs = [
    { name: "check", status: "completed", conclusion: "success" },
    { name: "build", status: "completed", conclusion: "failure" },
    { name: "release", status: "completed", conclusion: "skipped" },
    { name: "npm-publish", status: "completed", conclusion: "skipped" },
  ];
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build/);
});

test("checkJobs reports a missing job", () => {
  const jobs = [{ name: "check", status: "completed", conclusion: "success" }];
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build|release|npm-publish/);
});

test("checkNpmVersion compares exactly", () => {
  assert.equal(checkNpmVersion("0.25.2", "0.25.2").ok, true);
  assert.equal(checkNpmVersion("0.25.1", "0.25.2").ok, false);
  assert.match(checkNpmVersion("0.25.1", "0.25.2").reason, /0\.25\.1/);
});

test("checkSmokeOutput accepts the expected banner", () => {
  const result = checkSmokeOutput("rpi 0.25.2\n", "0.25.2", 20_000);
  assert.equal(result.ok, true);
  assert.equal(result.slow, false);
});

test("checkSmokeOutput rejects a mismatched version", () => {
  assert.equal(checkSmokeOutput("rpi 0.25.1\n", "0.25.2", 20_000).ok, false);
});

test("checkSmokeOutput flags a slow install as a source build", () => {
  const result = checkSmokeOutput("rpi 0.25.2\n", "0.25.2", 120_000);
  assert.equal(result.ok, true);
  assert.equal(result.slow, true);
  assert.match(result.reason, /prebuilt|source|slow/i);
});
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
node --test scripts/release-verify.test.mjs
```
Expected: FAIL — module not found.

- [ ] **Step 3: Write the implementation**

Create `scripts/release-verify.mjs`:

```js
#!/usr/bin/env node
// Post-release verification as one pass/fail instead of four one-liners a
// reader can answer from memory. The npx smoke test runs inside a
// throwaway container on purpose: a dev machine can have a global install
// or an npx cache that shadows the resolution and passes against stale
// state instead of the published package.

import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TRIPLES = [
  ["x86_64-pc-windows-msvc", "zip"],
  ["x86_64-unknown-linux-musl", "tar.gz"],
  ["aarch64-unknown-linux-musl", "tar.gz"],
];
const JOBS = ["check", "build", "release", "npm-publish"];
const SLOW_INSTALL_MS = 90_000;

export function expectedAssets(version) {
  return ["SHA256SUMS", ...TRIPLES.map(([triple, ext]) => `rpi-v${version}-${triple}.${ext}`)].sort();
}

export function checkAssets(actualNames, version) {
  const expected = expectedAssets(version);
  const actual = [...actualNames].sort();
  const missing = expected.filter((n) => !actual.includes(n));
  const extra = actual.filter((n) => !expected.includes(n));
  return { ok: missing.length === 0 && extra.length === 0, missing, extra };
}

export function checkJobs(jobs) {
  const byName = new Map(jobs.map((j) => [j.name, j]));
  const problems = [];
  for (const name of JOBS) {
    const job = byName.get(name);
    if (!job) problems.push(`${name}: missing`);
    else if (job.status !== "completed") problems.push(`${name}: ${job.status}`);
    else if (job.conclusion !== "success") problems.push(`${name}: ${job.conclusion}`);
  }
  return problems.length === 0
    ? { ok: true, reason: `all ${JOBS.length} jobs green` }
    : { ok: false, reason: problems.join(", ") };
}

export function checkNpmVersion(actual, version) {
  return actual === version
    ? { ok: true, reason: `npm latest is ${version}` }
    : { ok: false, reason: `npm latest is ${actual}, expected ${version}` };
}

export function checkSmokeOutput(stdout, version, elapsedMs) {
  const banner = stdout.trim();
  const slow = elapsedMs > SLOW_INSTALL_MS;
  if (banner !== `rpi ${version}`) {
    return { ok: false, slow, reason: `npx printed ${JSON.stringify(banner)}, expected "rpi ${version}"` };
  }
  return {
    ok: true,
    slow,
    reason: slow
      ? `printed "rpi ${version}" but took ${Math.round(elapsedMs / 1000)}s — npx likely built from source instead of using the prebuilt binary`
      : `printed "rpi ${version}" in ${Math.round(elapsedMs / 1000)}s`,
  };
}

function sh(cmd, args) {
  return execFileSync(cmd, args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024 }).trim();
}

function main() {
  const version = process.argv[2];
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    console.error("usage: node scripts/release-verify.mjs <X.Y.Z>");
    process.exit(2);
  }
  const tag = `v${version}`;
  const results = [];

  const runs = JSON.parse(
    sh("gh", ["run", "list", "--workflow", "release", "--limit", "20", "--json", "databaseId,headBranch"]),
  );
  const runId = runs.find((r) => r.headBranch === tag)?.databaseId;

  if (runId === undefined) {
    results.push({ ok: false, label: "workflow", reason: `no release run found for ${tag}` });
  } else {
    const jobs = JSON.parse(sh("gh", ["run", "view", String(runId), "--json", "jobs"])).jobs.map((j) => ({
      name: j.name,
      status: j.status,
      conclusion: j.conclusion,
    }));
    results.push({ label: "workflow", ...checkJobs(jobs) });
  }

  const assets = JSON.parse(sh("gh", ["release", "view", tag, "--json", "assets"])).assets.map((a) => a.name);
  const assetCheck = checkAssets(assets, version);
  results.push({
    label: "assets",
    ok: assetCheck.ok,
    reason: assetCheck.ok
      ? `${assets.length} assets as expected`
      : `missing: ${assetCheck.missing.join(", ") || "none"}; unexpected: ${assetCheck.extra.join(", ") || "none"}`,
  });

  results.push({ label: "npm", ...checkNpmVersion(sh("npm", ["view", "rpi-deploy", "version"]), version) });

  const started = Date.now();
  const stdout = sh("docker", ["run", "--rm", "node:20-slim", "npx", "-y", `rpi-deploy@${version}`, "--version"]);
  results.push({ label: "npx smoke", ...checkSmokeOutput(stdout, version, Date.now() - started) });

  for (const r of results) console.log(`${r.ok ? "ok" : "FAIL"}  ${r.label}: ${r.reason}`);
  const slow = results.find((r) => r.slow);
  if (slow) console.log("warn  install was slow — check that postinstall found the prebuilt binary");

  const failed = results.filter((r) => !r.ok);
  if (failed.length > 0) {
    console.error(`release-verify: ${failed.length} check(s) failed for ${tag}`);
    process.exit(1);
  }
  console.log(`release-verify: ${tag} is fully published`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  try {
    main();
  } catch (err) {
    console.error(`release-verify: ${err.message}`);
    process.exit(2);
  }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
node --test scripts/release-verify.test.mjs
```
Expected: PASS, 11/11.

- [ ] **Step 5: Run it against the already-published v0.25.1**

```bash
node scripts/release-verify.mjs 0.25.1
```
Expected: every line `ok`, ending with `release-verify: v0.25.1 is fully
published`. This is a real end-to-end check against a known-good release —
`v0.25.1` has all four assets and is the current npm `latest`.

If Docker is not running on this machine, the smoke line will fail with a
Docker error. In that case report exactly that: do not weaken the check or
add a fallback that runs npx outside a container.

- [ ] **Step 6: Commit and report**

```bash
git add scripts/release-verify.mjs scripts/release-verify.test.mjs
git commit -m "feat(release): add release-verify script"
```

Report the test output and the full output of Step 5.

---

### Task 5: Rewrite the release skill and the landing audit brief

**Depends on:** Tasks 1–4 complete, so every command referenced here exists.

**Files:**
- Modify: `.claude/skills/release/SKILL.md`
- Modify: `C:\Users\Khmil\RustProjects\rpi-deploy-site\docs\landing-audit.md`

**Interfaces:**
- Consumes: `node scripts/release-preflight.mjs`,
  `node scripts/bump-version.mjs <X.Y.Z>`,
  `node scripts/release-verify.mjs <X.Y.Z>` (this repo);
  `npm run sync-version -- <X.Y.Z>` (site repo).
- Produces: documentation only.

- [ ] **Step 1: Read the current skill and the spec**

Read `.claude/skills/release/SKILL.md` in full and
`docs/superpowers/specs/2026-07-25-release-quick-mode-design.md` §2, §4, §5.
The skill's explanatory paragraphs exist because steps written as bare
commands got answered from memory — keep that prose, rewrite the mechanics
around it.

- [ ] **Step 2: Add the quick-mode section to `SKILL.md`**

Insert after the "Choosing the bump" section, before "Release checklist":

````markdown
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
````

- [ ] **Step 3: Rewrite the affected full-mode steps**

Four edits inside the existing "Release checklist" and
"Post-release verification" sections. Keep the surrounding prose.

**Fix the test glob first.** The local gate in checklist step 4 currently
reads `node --test "scripts/**/*.test.js"`, which cannot see the
`.test.mjs` files this plan adds — verified in this worktree: the `.js`
glob discovers 6 tests, `scripts/**/*.test.*` discovers 8. Change that line
to:

```
node --test "scripts/**/*.test.*"   # postinstall + release tooling tests; CI runs these too
```

Leaving the old glob would silently drop every test written by Tasks 1–4
from the release gate — the exact class of miss this plan exists to close.

Replace checklist step 2 ("Bump versions") body with:

```markdown
2. **Bump versions**: `node scripts/bump-version.mjs X.Y.Z`. It writes
   `Cargo.toml` `[workspace.package] version` and `package.json` `version`,
   runs `cargo update --workspace`, and fails if `Cargo.lock` carries any
   change beyond the workspace version lines or if the tree holds anything
   but those three files. A stale lockfile is a guaranteed CI failure
   (`--locked` everywhere), which is what the assertion is for.
```

Add a new step 0 at the top of the checklist:

```markdown
0. **Preflight**: `node scripts/release-preflight.mjs`. It reports a clean
   tree, `HEAD == origin/master`, the classified commit range, the current
   and next-patch version, and whether `ci` is green on HEAD. Its `bump:`
   line is input to the decision above, not a replacement for it — minor
   versus major is still your call.
```

Replace the whole "Post-release verification" code block with:

```markdown
node scripts/release-verify.mjs X.Y.Z
```

and keep the paragraph explaining why the npx check runs in a container —
move it under the command as the reason the script does it that way.

- [ ] **Step 4: Fold `sync-version` into the landing audit step**

In the "Landing page audit" section, insert a new step between the current
steps 1 and 2:

```markdown
2. **Sync the version first**: `npm run sync-version -- X.Y.Z` in the site
   repo. It rewrites every semver under `src/` and fails loudly if they are
   not all the same string, so the auditors never spend attention on
   numbers. If it reports a mixed set, resolve that by hand before
   continuing — a foreign semver on the page is exactly the kind of thing
   the audit exists to catch.
```

Renumber the following steps. In the (now) auditor-spawning step, add one
sentence: "Version strings are already handled by `sync-version`; auditors
report on feature text, CLI transcripts and discovery files."

- [ ] **Step 5: Update the landing audit brief in the site repo**

In `C:\Users\Khmil\RustProjects\rpi-deploy-site\docs\landing-audit.md`, add
a short subsection near the top stating that:
- version strings under `src/` are synced by `npm run sync-version -- X.Y.Z`
  before the auditors run, and auditors should not report on them;
- patch releases reach the landing through that sync alone, so anything an
  auditor would have caught in feature text accumulates until the next
  minor.

Match the file's existing heading level and tone — read it first.

- [ ] **Step 6: Verify every command in the rewritten docs exists**

For each command written into the two documents, confirm it runs:

```bash
node scripts/release-preflight.mjs --help 2>&1 | head -3
ls scripts/bump-version.mjs scripts/release-verify.mjs
node -e "const p=require('C:/Users/Khmil/RustProjects/rpi-deploy-site/package.json'); if(!p.scripts['sync-version']) { throw new Error('sync-version npm script missing'); } console.log('sync-version wired ok')"
```

Expected: the two `.mjs` files listed, and `sync-version wired ok`. The
preflight `--help` invocation may simply run the preflight — that is fine,
the point is that the file resolves.

- [ ] **Step 7: Commit and report**

In the worktree:
```bash
git add .claude/skills/release/SKILL.md
git commit -m "docs(release): document quick mode and the release scripts"
```

In the site repo:
```bash
git add docs/landing-audit.md
git commit -m "docs: note scripted version sync in the landing audit brief"
```

Report a diff summary of both documents and confirm Step 6's output.

---

## Verification

After all five tasks, from the `pi` repo root:

```bash
node --test scripts/release-preflight.test.mjs scripts/bump-version.test.mjs scripts/release-verify.test.mjs
```
Expected: 32 tests passing, 0 failing.

From the site repo root:

```bash
node --test scripts/sync-version.test.mjs
```
Expected: 6 tests passing.

The repo's own gate (`cargo fmt`, `cargo clippy`, `cargo test`) is not
affected by this plan — no Rust file is touched, and the worktree branches
from released `master`.

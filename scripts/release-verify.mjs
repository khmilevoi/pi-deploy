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
  const problems = [];
  let matched = 0;
  for (const name of JOBS) {
    const exact = jobs.find((j) => j.name === name);
    // Matrix jobs report as "<name> (<matrix values>)", never the bare name.
    // The " (" separator is required so e.g. a job named "builder" cannot
    // satisfy a requirement for "build".
    const instances = exact ? [exact] : jobs.filter((j) => j.name.startsWith(`${name} (`));
    if (instances.length === 0) {
      problems.push(`${name}: missing`);
      continue;
    }
    matched += instances.length;
    for (const job of instances) {
      if (job.status !== "completed") problems.push(`${job.name}: ${job.status}`);
      else if (job.conclusion !== "success") problems.push(`${job.name}: ${job.conclusion}`);
    }
  }
  // Report the number of job entries actually matched and verified (which
  // includes every matrix instance), not JOBS.length — a real release
  // reports six job rows (three of them "build (...)" matrix instances) for
  // four required names, and a message claiming "4" while GitHub shows six
  // rows is exactly the kind of mismatch this script exists to prevent.
  return problems.length === 0
    ? { ok: true, reason: `all ${matched} jobs green` }
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

// `cwd` is always passed explicitly by the `gh` call sites, pinned to the
// repository root resolved from this file rather than the inherited working
// directory: `gh` picks the repository from the current directory, and the
// quick-mode checklist tells the operator to `cd` into the landing-page
// repo, whose shell working directory persists into later commands.
function sh(cmd, args, opts = {}) {
  return execFileSync(cmd, args, { encoding: "utf8", maxBuffer: 32 * 1024 * 1024, ...opts }).trim();
}

// `gh release view <tag>` fails with "release not found" for the whole
// window between pushing the tag and the `release` job publishing it —
// roughly 5-10 minutes, and the normal state for anyone following the
// checklist in order. That is a check that has not passed yet, not the
// script breaking, so it must be reported alongside the other results
// rather than aborting the run. Everything else — no `gh` on PATH, an
// expired token, HTTP 401/403, a network failure — is the script being
// unable to complete and still exits 2. The match is deliberately narrow:
// only gh's own not-found wording counts, so an auth or transport failure
// can never be mistaken for "not published yet".
const RELEASE_NOT_FOUND = /\brelease not found\b/i;

export function classifyReleaseViewFailure(text) {
  return RELEASE_NOT_FOUND.test(String(text ?? "")) ? "not-published" : "error";
}

// The same split for the registry-facing calls (`npm view`, and the `npx`
// inside the container). `npm-publish` has `needs: release` in the
// workflow, so for the whole 5-10 minute window after the tag "the release
// is not published" also means "the version is not on npm" — the registry
// answers E404 (package or version absent) or ETARGET (package there, this
// version not yet). Those are checks that have not passed yet and belong in
// the printed results. Everything else — no `npm`/`docker` binary, a dead
// Docker daemon, DNS or TLS failure, a proxy refusing the connection — is
// the script unable to complete, and still exits 2. `ENOTFOUND` (DNS)
// deliberately does not match: only a registry's own 404/no-such-version
// wording counts.
const NPM_VERSION_ABSENT = /\bE404\b|\bETARGET\b|\b404 Not Found\b|no matching version found|is not in this registry/i;

export function classifyNpmFailure(text) {
  return NPM_VERSION_ABSENT.test(String(text ?? "")) ? "not-published" : "error";
}

// execFileSync puts the captured stderr on the error and repeats it in the
// message; read both so a classification never depends on which one the
// caller's failure mode filled in.
export function commandFailureText(err) {
  return [err?.stderr, err?.message].filter(Boolean).join("\n");
}

// Package names reaching npmView must match this narrow, conservative set
// before they are allowed anywhere near cmd.exe on Windows (see npmView).
const SAFE_PACKAGE_NAME = /^[\w.-]+$/;

export function assertSafePackageName(pkg) {
  if (!SAFE_PACKAGE_NAME.test(pkg)) {
    throw new Error(`npmView: unsafe package name ${JSON.stringify(pkg)}`);
  }
  return pkg;
}

// On Windows, npm is npm.cmd, and Windows can only run .cmd/.bat files
// through a shell — Node's execFile family refuses to spawn them directly
// (confirmed: EINVAL), extension or not. cmd.exe itself is a real .exe, so
// invoking it as the command (rather than setting the shell:true option)
// runs npm.cmd without Node's shell-argument-concatenation deprecation
// warning. This is NOT the same as execFile's normal argv safety: cmd.exe
// re-parses its own /c command line, so a value like `"&calc.exe&"` passed
// straight through as `pkg` gets interpreted by cmd.exe and launches an
// arbitrary process (verified). The call site below only ever passes the
// hardcoded literal "rpi-deploy", but that is not what makes this call
// safe — the assertSafePackageName() guard is, because it rejects anything
// outside a narrow character set before pkg ever reaches cmd.exe, so any
// future caller passing a variable inherits the same protection.
function npmView(pkg) {
  assertSafePackageName(pkg);
  return process.platform === "win32"
    ? sh("cmd.exe", ["/d", "/s", "/c", "npm", "view", pkg, "version"])
    : sh("npm", ["view", pkg, "version"]);
}

// Why the smoke test would be pointless right now, or null if it is worth
// running. It costs a docker pull plus an install, and it can only answer
// E404 while the version is absent from the registry — `npm-publish` has
// `needs: release` in the workflow, so an unpublished release already
// settles it. The npm-latest arm mirrors checkNpmVersion's own exact-match
// rule: if the dist-tag is not this version, the "npm" check has already
// failed and the smoke test adds a slow second opinion on the same fact.
// A blocker never becomes a passing check — the caller reports it as FAIL,
// because the check has not passed, it has not run.
export function smokeBlocker({ releasePublished, npmLatest, version, tag }) {
  if (!releasePublished) return `GitHub Release ${tag} is not published, and npm-publish runs only after it`;
  if (npmLatest === null) return `the registry does not serve rpi-deploy at all yet`;
  if (npmLatest !== version) return `npm latest is ${npmLatest}, not ${version}`;
  return null;
}

// Every check appends its verdict here and the caller prints the lot, even
// when a later check dies outright: an operator who ran this too early must
// still see which checks did answer, and an operator hitting a genuine
// infrastructure failure must still see the ones that ran before it.
function runChecks(results, { version, tag, root }) {
  const runs = JSON.parse(
    sh("gh", ["run", "list", "--workflow", "release", "--limit", "20", "--json", "databaseId,headBranch"], { cwd: root }),
  );
  const runId = runs.find((r) => r.headBranch === tag)?.databaseId;

  if (runId === undefined) {
    results.push({ ok: false, label: "workflow", reason: `no release run found for ${tag}` });
  } else {
    const jobs = JSON.parse(sh("gh", ["run", "view", String(runId), "--json", "jobs"], { cwd: root })).jobs.map((j) => ({
      name: j.name,
      status: j.status,
      conclusion: j.conclusion,
    }));
    results.push({ label: "workflow", ...checkJobs(jobs) });
  }

  let assets = null;
  let releasePublished = true;
  try {
    // stderr is captured rather than echoed: during the normal post-tag
    // window this call prints a bare "release not found", which would land
    // above the results and read as a crash. The text is still on the error
    // for classification, and for a genuine failure it reaches the operator
    // through the exit-2 message.
    const opts = { cwd: root, stdio: ["ignore", "pipe", "pipe"] };
    assets = JSON.parse(sh("gh", ["release", "view", tag, "--json", "assets"], opts)).assets.map((a) => a.name);
  } catch (err) {
    if (classifyReleaseViewFailure(commandFailureText(err)) !== "not-published") throw err;
    releasePublished = false;
    results.push({
      label: "assets",
      ok: false,
      reason: `GitHub Release ${tag} is not published yet — the release job creates it minutes after the tag; wait with \`gh run watch\` and re-run`,
    });
  }

  if (assets !== null) {
    const assetCheck = checkAssets(assets, version);
    results.push({
      label: "assets",
      ok: assetCheck.ok,
      reason: assetCheck.ok
        ? `${assets.length} assets as expected`
        : `missing: ${assetCheck.missing.join(", ") || "none"}; unexpected: ${assetCheck.extra.join(", ") || "none"}`,
    });
  }

  let npmLatest = null;
  try {
    npmLatest = npmView("rpi-deploy");
  } catch (err) {
    if (classifyNpmFailure(commandFailureText(err)) !== "not-published") throw err;
    results.push({
      label: "npm",
      ok: false,
      reason: `the registry has no rpi-deploy yet — the npm-publish job runs after release; wait for the workflow and re-run`,
    });
  }
  if (npmLatest !== null) results.push({ label: "npm", ...checkNpmVersion(npmLatest, version) });

  const npmBlocker = smokeBlocker({ releasePublished, npmLatest, version, tag });

  if (npmBlocker !== null) {
    results.push({
      label: "npx smoke",
      ok: false,
      reason: `not run — rpi-deploy@${version} cannot be on npm yet (${npmBlocker}); re-run once the release workflow has finished`,
    });
  } else {
    const started = Date.now();
    try {
      const stdout = sh("docker", ["run", "--rm", "node:20-slim", "npx", "-y", `rpi-deploy@${version}`, "--version"]);
      results.push({ label: "npx smoke", ...checkSmokeOutput(stdout, version, Date.now() - started) });
    } catch (err) {
      // A container that ran and could not resolve the version is a check
      // that has not passed (the registry can lag its own dist-tag by
      // seconds). No docker, a dead daemon or a failed image pull is the
      // script unable to run, and is rethrown to exit 2.
      if (classifyNpmFailure(commandFailureText(err)) !== "not-published") throw err;
      results.push({
        label: "npx smoke",
        ok: false,
        reason: `npx could not resolve rpi-deploy@${version} inside the container — the registry does not serve that version yet; re-run in a minute`,
      });
    }
  }
}

function printResults(results) {
  for (const r of results) console.log(`${r.ok ? "ok" : "FAIL"}  ${r.label}: ${r.reason}`);
  if (results.some((r) => r.slow)) {
    console.log("warn  install was slow — check that postinstall found the prebuilt binary");
  }
}

function main() {
  const version = process.argv[2];
  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    console.error("usage: node scripts/release-verify.mjs <X.Y.Z>");
    process.exit(2);
  }
  const tag = `v${version}`;
  const root = path.resolve(fileURLToPath(import.meta.url), "..", "..");
  const results = [];

  try {
    runChecks(results, { version, tag, root });
  } catch (err) {
    // Print what did get answered before handing the failure to the exit-2
    // path: an operator debugging a broken `gh` or Docker still learns
    // whether the release itself is fine.
    printResults(results);
    throw err;
  }

  printResults(results);

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

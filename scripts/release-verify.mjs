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

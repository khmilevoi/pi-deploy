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
      const [sha, ...rest] = chunk.split(FIELD);
      const message = rest.join(FIELD);
      const newline = message.indexOf("\n");
      const subject = (newline === -1 ? message : message.slice(0, newline)).trim();
      const body = (newline === -1 ? "" : message.slice(newline + 1)).trim();
      return { sha, subject, body };
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
    const wholeMessage = c.body ? `${c.subject}\n${c.body}` : c.subject;
    if (m[3] === "!" || /^BREAKING[- ]CHANGE:/m.test(wholeMessage)) {
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
  const raw = git("log", "--no-merges", "--format=%H%x00%B%x01", `${lastTag}..HEAD`);
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
        ["run", "list", "--workflow", "ci", "--branch", "master", "--limit", "100", "--json", "headSha,status,conclusion"],
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

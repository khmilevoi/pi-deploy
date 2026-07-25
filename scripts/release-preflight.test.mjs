import test from "node:test";
import assert from "node:assert/strict";

import {
  classifyRange,
  nextPatch,
  ciStatusForSha,
  parseCommits,
} from "./release-preflight.mjs";

const commit = (subject, body = "") => ({ sha: "a".repeat(40), subject, body });
const FIELD = "\u0000";
const RECORD = "\u0001";

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
  const raw = [`abc123${FIELD}fix: one`, `def456${FIELD}fix: two\nBREAKING CHANGE: x`].join(RECORD);
  const commits = parseCommits(raw);
  assert.equal(commits.length, 2);
  assert.equal(commits[0].subject, "fix: one");
  assert.equal(commits[0].body, "");
  assert.equal(commits[1].subject, "fix: two");
  assert.equal(commits[1].body, "BREAKING CHANGE: x");
});

test("BREAKING CHANGE glued to the subject with no blank line still refuses quick (regression)", () => {
  const raw = `${"a".repeat(40)}${FIELD}fix: some header\nBREAKING CHANGE: it's broken`;
  const result = classifyRange(parseCommits(raw));
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /breaking/i);
});

test("BREAKING CHANGE with a proper blank line before it also refuses quick", () => {
  const raw = `${"a".repeat(40)}${FIELD}fix: some header\n\nBREAKING CHANGE: it's broken`;
  const result = classifyRange(parseCommits(raw));
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /breaking/i);
});

test("BREAKING-CHANGE (hyphen spelling) also refuses quick", () => {
  const raw = `${"a".repeat(40)}${FIELD}fix: some header\nBREAKING-CHANGE: it's broken`;
  const result = classifyRange(parseCommits(raw));
  assert.equal(result.quickAllowed, false);
  assert.match(result.reason, /breaking/i);
});

test("a multi-paragraph fix commit with no footer is still allowed", () => {
  const raw = `${"a".repeat(40)}${FIELD}fix: some header\n\nExplains the change in more detail.\n\nA second paragraph, still no footer.`;
  const result = classifyRange(parseCommits(raw));
  assert.equal(result.bump, "patch");
  assert.equal(result.quickAllowed, true);
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

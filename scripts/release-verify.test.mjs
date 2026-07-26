import test from "node:test";
import assert from "node:assert/strict";

import {
  expectedAssets,
  checkAssets,
  checkJobs,
  checkNpmVersion,
  checkSmokeOutput,
  assertSafePackageName,
  classifyReleaseViewFailure,
  releaseViewFailureText,
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

const REAL_V0_25_1_JOBS = [
  { name: "check", status: "completed", conclusion: "success" },
  { name: "build (windows-latest, x86_64-pc-windows-msvc)", status: "completed", conclusion: "success" },
  { name: "build (ubuntu-latest, x86_64-unknown-linux-musl)", status: "completed", conclusion: "success" },
  { name: "build (ubuntu-24.04-arm, aarch64-unknown-linux-musl)", status: "completed", conclusion: "success" },
  { name: "release", status: "completed", conclusion: "success" },
  { name: "npm-publish", status: "completed", conclusion: "success" },
];

test("checkJobs accepts the real six-job matrix run", () => {
  const result = checkJobs(REAL_V0_25_1_JOBS);
  assert.equal(result.ok, true);
  assert.match(result.reason, /all 6 jobs green/);
});

test("checkJobs reports a failed matrix instance by its full name", () => {
  const jobs = REAL_V0_25_1_JOBS.map((j) =>
    j.name === "build (windows-latest, x86_64-pc-windows-msvc)" ? { ...j, conclusion: "failure" } : j,
  );
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build \(windows-latest, x86_64-pc-windows-msvc\): failure/);
});

test("checkJobs reports build missing when no matrix instance exists", () => {
  const jobs = REAL_V0_25_1_JOBS.filter((j) => !j.name.startsWith("build"));
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build: missing/);
});

test("checkJobs requires the ' (' separator, so 'builder' does not satisfy 'build'", () => {
  const jobs = [
    { name: "check", status: "completed", conclusion: "success" },
    { name: "release", status: "completed", conclusion: "success" },
    { name: "npm-publish", status: "completed", conclusion: "success" },
    { name: "builder", status: "completed", conclusion: "success" },
  ];
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build: missing/);
});

test("checkJobs reports an in-progress matrix instance", () => {
  const jobs = REAL_V0_25_1_JOBS.map((j) =>
    j.name === "build (ubuntu-latest, x86_64-unknown-linux-musl)"
      ? { ...j, status: "in_progress", conclusion: null }
      : j,
  );
  const result = checkJobs(jobs);
  assert.equal(result.ok, false);
  assert.match(result.reason, /build \(ubuntu-latest, x86_64-unknown-linux-musl\): in_progress/);
});

test("assertSafePackageName rejects shell metacharacters and accepts a normal package name", () => {
  assert.throws(() => assertSafePackageName('"&calc.exe&"'));
  assert.doesNotThrow(() => assertSafePackageName("rpi-deploy"));
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

test("classifyReleaseViewFailure reads gh's not-found wording as 'not published yet'", () => {
  // The literal shape of the failure a releaser hits when running step 5
  // immediately after step 4, before the release job has created the release.
  const err = Object.assign(new Error("Command failed: gh release view v0.25.2 --json assets\nrelease not found\n"), {
    status: 1,
    stderr: "release not found\n",
  });
  assert.equal(classifyReleaseViewFailure(releaseViewFailureText(err)), "not-published");
  assert.equal(classifyReleaseViewFailure("release not found"), "not-published");
  assert.equal(classifyReleaseViewFailure("RELEASE NOT FOUND"), "not-published");
});

test("classifyReleaseViewFailure keeps a broken or unauthenticated gh as a genuine error", () => {
  const cases = [
    "gh: command not found",
    "spawnSync gh ENOENT",
    "To get started with GitHub CLI, please run:  gh auth login",
    "HTTP 401: Bad credentials (https://api.github.com/repos/khmilevoi/pi/releases/tags/v0.25.2)",
    "HTTP 403: Resource not accessible by personal access token",
    "error connecting to api.github.com",
  ];
  for (const text of cases) {
    assert.equal(classifyReleaseViewFailure(text), "error", `expected an error for: ${text}`);
  }
});

test("classifyReleaseViewFailure treats an absent message as an error, never as 'not published'", () => {
  assert.equal(classifyReleaseViewFailure(undefined), "error");
  assert.equal(classifyReleaseViewFailure(null), "error");
  assert.equal(classifyReleaseViewFailure(""), "error");
});

test("releaseViewFailureText reads both stderr and message, and survives either being absent", () => {
  assert.match(releaseViewFailureText({ stderr: "release not found\n", message: "Command failed" }), /release not found/);
  assert.equal(releaseViewFailureText({ message: "Command failed" }), "Command failed");
  assert.equal(releaseViewFailureText({ stderr: "release not found" }), "release not found");
  assert.equal(releaseViewFailureText({}), "");
});

test("checkSmokeOutput flags a slow install as a source build", () => {
  const result = checkSmokeOutput("rpi 0.25.2\n", "0.25.2", 120_000);
  assert.equal(result.ok, true);
  assert.equal(result.slow, true);
  assert.match(result.reason, /prebuilt|source|slow/i);
});

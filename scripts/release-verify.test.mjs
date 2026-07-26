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
  classifyNpmFailure,
  commandFailureText,
  smokeBlocker,
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
  assert.equal(classifyReleaseViewFailure(commandFailureText(err)), "not-published");
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

test("commandFailureText reads both stderr and message, and survives either being absent", () => {
  assert.match(commandFailureText({ stderr: "release not found\n", message: "Command failed" }), /release not found/);
  assert.equal(commandFailureText({ message: "Command failed" }), "Command failed");
  assert.equal(commandFailureText({ stderr: "release not found" }), "release not found");
  assert.equal(commandFailureText({}), "");
});

test("classifyNpmFailure reads a registry miss as 'not published yet'", () => {
  // The literal shapes npm and npx produce while npm-publish has not run:
  // the package or version is simply absent from the registry.
  const cases = [
    "npm error code E404\nnpm error 404 Not Found - GET https://registry.npmjs.org/rpi-deploy - Not found",
    "npm error 404  'rpi-deploy@0.25.2' is not in this registry.",
    "npm error code ETARGET\nnpm error notarget No matching version found for rpi-deploy@0.25.2.",
    "npm ERR! code E404",
  ];
  for (const text of cases) {
    assert.equal(classifyNpmFailure(text), "not-published", `expected not-published for: ${text}`);
  }
});

test("classifyNpmFailure keeps missing docker, a dead daemon and network trouble as genuine errors", () => {
  // These must still exit 2: the check never ran, so calling it "not
  // published yet" would report a broken machine as a pending release.
  const cases = [
    "docker: command not found",
    "spawnSync docker ENOENT",
    "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
    "error during connect: this error may indicate that the docker daemon is not running",
    "docker: Error response from daemon: pull access denied for node:20-slim",
    "npm error code ENOTFOUND\nnpm error network request to https://registry.npmjs.org/rpi-deploy failed, reason: getaddrinfo ENOTFOUND",
    "npm error code ECONNREFUSED",
    "npm error code E401\nnpm error Incorrect or missing password.",
    "npm error code EPERM",
    "Command failed: docker run --rm node:20-slim npx -y rpi-deploy@0.25.2 --version",
  ];
  for (const text of cases) {
    assert.equal(classifyNpmFailure(text), "error", `expected an error for: ${text}`);
  }
});

test("classifyNpmFailure treats an absent message as an error, never as 'not published'", () => {
  assert.equal(classifyNpmFailure(undefined), "error");
  assert.equal(classifyNpmFailure(null), "error");
  assert.equal(classifyNpmFailure(""), "error");
});

test("smokeBlocker skips the container while the release is unpublished", () => {
  const blocker = smokeBlocker({ releasePublished: false, npmLatest: "0.25.1", version: "0.25.2", tag: "v0.25.2" });
  assert.match(blocker, /not published/);
  assert.match(blocker, /npm-publish/);
});

test("smokeBlocker skips the container while npm still serves the previous version", () => {
  const blocker = smokeBlocker({ releasePublished: true, npmLatest: "0.25.1", version: "0.25.2", tag: "v0.25.2" });
  assert.match(blocker, /0\.25\.1/);
});

test("smokeBlocker skips the container when npm has no such package at all", () => {
  const blocker = smokeBlocker({ releasePublished: true, npmLatest: null, version: "0.25.2", tag: "v0.25.2" });
  assert.match(blocker, /registry/);
});

test("smokeBlocker runs the container once the release and npm both show the version", () => {
  assert.equal(
    smokeBlocker({ releasePublished: true, npmLatest: "0.25.2", version: "0.25.2", tag: "v0.25.2" }),
    null,
  );
});

test("checkSmokeOutput flags a slow install as a source build", () => {
  const result = checkSmokeOutput("rpi 0.25.2\n", "0.25.2", 120_000);
  assert.equal(result.ok, true);
  assert.equal(result.slow, true);
  assert.match(result.reason, /prebuilt|source|slow/i);
});

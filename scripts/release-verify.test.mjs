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

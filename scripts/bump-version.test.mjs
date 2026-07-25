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

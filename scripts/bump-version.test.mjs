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

test("bumpPackageJson rewrites the top-level version and leaves a nested engines.version untouched", () => {
  const pkg = '{\n  "name": "x",\n  "engines": {\n    "version": "9.9.9"\n  },\n  "version": "0.25.1"\n}\n';
  const out = bumpPackageJson(pkg, "0.25.2");
  assert.equal(JSON.parse(out).version, "0.25.2");
  assert.equal(JSON.parse(out).engines.version, "9.9.9");
  assert.match(out, /"version": "9\.9\.9"/);
  assert.match(out, /"version": "0\.25\.2"\n\}/);
});

test("bumpPackageJson throws when there is no top-level version key", () => {
  const pkg = '{\n  "name": "x",\n  "engines": {\n    "version": "9.9.9"\n  }\n}\n';
  assert.throws(() => bumpPackageJson(pkg, "0.25.2"), BumpError);
});

test("assertLockfileDiff accepts a workspace-only diff", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,3 @@
 [[package]]
 name = "pi"
-version = "0.25.1"
+version = "0.25.2"
`;
  assert.doesNotThrow(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"));
});

test("assertLockfileDiff accepts a paired change owned by pi and by pi-domain", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,3 @@
 [[package]]
 name = "pi"
-version = "0.25.1"
+version = "0.25.2"
@@ -40,3 +40,3 @@
 [[package]]
 name = "pi-domain"
-version = "0.25.1"
+version = "0.25.2"
`;
  assert.doesNotThrow(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"));
});

test("assertLockfileDiff rejects a changed version line with no visible owning name line", () => {
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
  assert.throws(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"), BumpError);
});

test("assertLockfileDiff rejects a lone unpaired +version line", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,3 @@
 [[package]]
 name = "pi"
-version = "0.25.1"
+version = "0.25.2"
@@ -40,2 +40,3 @@
 [[package]]
 name = "pi-domain"
+version = "0.25.2"
`;
  assert.throws(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"), BumpError);
});

test("assertLockfileDiff rejects a properly paired change owned by pin-project", () => {
  const diff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,3 @@
 [[package]]
 name = "pin-project"
-version = "0.25.1"
+version = "0.25.2"
`;
  assert.throws(() => assertLockfileDiff(diff, "0.25.1", "0.25.2"), (err) => {
    assert.ok(err instanceof BumpError);
    assert.match(err.message, /pin-project/);
    return true;
  });
});

test("assertLockfileDiff accepts the existing valid fixture with CRLF line endings", () => {
  const lfDiff = `diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,3 +10,3 @@
 [[package]]
 name = "pi"
-version = "0.25.1"
+version = "0.25.2"
`;
  const diff = lfDiff.replace(/\n/g, "\r\n");
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

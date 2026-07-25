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

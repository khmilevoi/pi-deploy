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
  const n = text.length;

  function skipString(startIdx) {
    // startIdx points at the opening quote; returns the index of the
    // closing quote (or the end of the text if unterminated).
    let j = startIdx + 1;
    while (j < n) {
      const c = text[j];
      if (c === "\\") {
        j += 2;
        continue;
      }
      if (c === '"') {
        return j;
      }
      j++;
    }
    return j;
  }

  let depth = 0;
  let i = 0;
  while (i < n) {
    const c = text[i];
    if (c === '"') {
      const strStart = i;
      const strEnd = skipString(i);
      const strValue = text.slice(strStart + 1, strEnd);

      let k = strEnd + 1;
      while (k < n && /\s/.test(text[k])) k++;
      const isKey = text[k] === ":";

      if (isKey && strValue === "version" && depth === 1) {
        let v = k + 1;
        while (v < n && /\s/.test(text[v])) v++;
        if (text[v] !== '"') {
          throw new BumpError("top-level \"version\" value in package.json is not a string");
        }
        const valStart = v;
        const valEnd = skipString(v);
        return text.slice(0, valStart + 1) + version + text.slice(valEnd);
      }

      i = strEnd + 1;
      continue;
    }
    if (c === "{" || c === "[") {
      depth++;
      i++;
      continue;
    }
    if (c === "}" || c === "]") {
      depth--;
      i++;
      continue;
    }
    i++;
  }

  throw new BumpError("cannot find a top-level version in package.json");
}

const VERSION_LINE_RE = /^([-+])version = "([^"]+)"\r?$/;
const NAME_LINE_RE = /^[-+ ]name = "([^"]+)"\r?$/;

function isWorkspaceCrate(name) {
  return name === "pi" || name.startsWith("pi-");
}

export function assertLockfileDiff(diffText, from, to) {
  const allLines = diffText.split("\n");
  const changeLines = allLines.filter((l) => /^[-+]/.test(l) && !/^(---|\+\+\+)/.test(l));
  if (changeLines.length === 0) {
    throw new BumpError("Cargo.lock shows no changes — did `cargo update --workspace` run?");
  }

  // Value check: every changed line must be a version line moving exactly
  // from -> to. This alone does not prevent an unrelated crate's version
  // from being smuggled in if it happens to share the same from/to values,
  // which is why the structural checks below exist.
  const offending = changeLines.filter((l) => {
    const m = VERSION_LINE_RE.exec(l);
    if (!m) return true;
    return m[1] === "-" ? m[2] !== from : m[2] !== to;
  });
  if (offending.length > 0) {
    throw new BumpError(
      `Cargo.lock carries changes beyond the workspace version bump — commit those separately:\n${offending.map((l) => `  ${l}`).join("\n")}`,
    );
  }

  // Structural checks, per hunk: (1) removed and added version lines must
  // pair up, and (2) every changed version line must belong to a workspace
  // crate, found via the nearest preceding `name = "..."` line in the same
  // hunk (context or changed).
  const hunks = [];
  let current = null;
  for (const line of allLines) {
    if (/^@@/.test(line)) {
      current = [];
      hunks.push(current);
    } else if (current) {
      current.push(line);
    }
  }

  for (const hunk of hunks) {
    let removed = 0;
    let added = 0;
    for (let i = 0; i < hunk.length; i++) {
      const m = VERSION_LINE_RE.exec(hunk[i]);
      if (!m) continue;
      if (m[1] === "-") {
        removed++;
      } else {
        added++;
      }

      let owner = null;
      for (let j = i - 1; j >= 0; j--) {
        const nm = NAME_LINE_RE.exec(hunk[j]);
        if (nm) {
          owner = nm[1];
          break;
        }
      }
      if (owner === null) {
        throw new BumpError(`cannot determine the owning crate for changed line: ${hunk[i]}`);
      }
      if (!isWorkspaceCrate(owner)) {
        throw new BumpError(
          `Cargo.lock version change belongs to "${owner}", not a workspace crate — commit that separately:\n  ${hunk[i]}`,
        );
      }
    }
    if (removed !== added) {
      const culprits = hunk.filter((l) => VERSION_LINE_RE.test(l)).map((l) => `  ${l}`);
      throw new BumpError(
        `Cargo.lock has an unpaired version change (${removed} removed vs ${added} added in one hunk):\n${culprits.join("\n")}`,
      );
    }
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
  const cargoText = fs.readFileSync(cargoPath, "utf8");
  const pkgText = fs.readFileSync(pkgPath, "utf8");
  const from = JSON.parse(pkgText).version;

  // Compute both new contents before writing either — a failure bumping
  // the second file must not leave the first one half-bumped on disk.
  const nextCargoText = bumpCargoToml(cargoText, version);
  const nextPkgText = bumpPackageJson(pkgText, version);

  fs.writeFileSync(cargoPath, nextCargoText);
  fs.writeFileSync(pkgPath, nextPkgText);
  console.log(`bump-version: ${from} -> ${version} in Cargo.toml, package.json`);

  execFileSync("cargo", ["update", "--workspace"], { cwd: root, stdio: "inherit" });

  // -U3 (rather than -U0) is required so each hunk carries the `name = "..."`
  // line that assertLockfileDiff needs to verify crate ownership.
  assertLockfileDiff(git(root, "diff", "-U3", "--", "Cargo.lock"), from, version);
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

# Configuration Variables Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the overlay-only, `BRANCH_NAME`-only variable system with three disjoint namespaces — arbitrary `--vars` usable anywhere in the config, lowercase dotted resolver inputs (`git.*`, `env.*`), and an `RPI_*` namespace that exists only at runtime and is injected into containers and exec'd processes.

**Architecture:** The CLI resolver stops walking typed structs and instead substitutes over the raw `toml::Value` tree of both `rpi.toml` and `rpi.<env>.toml` before deserializing, in two phases (`source.branch` first, because `env.slug` derives from it). The agent computes the `RPI_*` map from registry state it already holds — no wire-protocol change — and delivers it three ways: the `docker compose` process environment, an `environment:` block per service in the generated override, and `-e` flags on `exec`.

**Tech Stack:** Rust 2021, `toml` (Value tree + serde), `serde_yaml` (override emission), `rusqlite` + `rusqlite_migration` (registry), `mockall` (`--features mocks` on `pi-domain`), `tokio` + `async-trait`.

**Spec:** `docs/superpowers/specs/2026-07-26-config-variables-design.md`

## Global Constraints

- Worktree: `C:/Users/Khmil/RustProjects/pi/.worktrees/config-variables`, branch `config-variables`. All commands run from there.
- Before considering any task complete, run all three (this is what CI runs on Linux; a mismatch here is a guaranteed CI failure):
  ```
  cargo fmt --all -- --check
  cargo clippy --all-targets --locked -- -D warnings
  cargo test --locked
  ```
  If `fmt --check` reports a diff, run `cargo fmt --all` and commit the result — never hand-edit formatting.
- Every task must leave the workspace compiling and all tests green. Signature changes that break call sites are fixed inside the same task with a behavior-neutral value; a later task fills in the real value.
- User variable names: `^[A-Z][A-Z0-9_]*$`. Resolver input names: `^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$`. The `RPI_` prefix is reserved and rejected everywhere a user can supply a name.
- Resolver input namespaces are exactly `git` and `env`. Inputs are `git.branch`, `git.sha`, `git.short_sha`, `env.name`, `env.slug`.
- Runtime variables are exactly: `RPI_PROJECT`, `RPI_PROJECT_BASE`, `RPI_ENV`, `RPI_ENV_SLUG`, `RPI_BRANCH_NAME`, `RPI_HOSTNAME`, `RPI_HOST_PORT`, `RPI_COMMIT_SHA`. An unavailable one is omitted from the map, never set to an empty string.
- `$${` yields a literal `${`. `$$` is special only immediately before `{`.
- Substitution is forbidden in `schema` and `[project].name`.
- New agent capability string: `runtime-vars`, `Feature::RuntimeVars`, since `0.27.0`, `Policy::Degradable`.
- Commit messages follow Conventional Commits, as the repository already does (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`).

## File Structure

**Created**

| File | Responsibility |
|---|---|
| `crates/bin/src/cli/vars.rs` | The variable engine: `--vars` parsing, the `${...}` tokenizer with `$$` escaping, name classification into the three namespaces, and every variable-related error message. Pure, no I/O. |
| `crates/bin/src/cli/gitctx.rs` | Resolves `git.branch` / `git.sha` / `git.short_sha` by shelling out to `git` in a given directory. The only place that knows about detached `HEAD`. |
| `crates/domain/src/runtimevars.rs` | `rpi_vars(&Project, Option<&str>) -> BTreeMap<String, String>` — the single definition of the `RPI_*` map. Pure function over entities. |

**Modified**

| File | Change |
|---|---|
| `crates/bin/src/cli/overlay.rs` | Interpolation moves from a typed field whitelist to a two-phase `toml::Value` walk; new slug rule; unused-variable check; no-slug warning. |
| `crates/bin/src/cli/rpitoml.rs` | `RpiToml::from_value` / `default_branch` exposed so the resolver can deserialize an already-substituted tree. |
| `crates/bin/src/cli/envcmds.rs` | `--key` path for `env destroy` / `env reset-data`. |
| `crates/bin/src/cli/commands.rs` | `RPI_` rejection in `secrets_send`; `[runtime]` block in `config_show`; `RuntimeVars` gate in `deploy`. |
| `crates/bin/src/main.rs` | `--key` flags. |
| `crates/bin/src/compat.rs` | `Feature::RuntimeVars`. |
| `crates/bin/src/cli/mod.rs` | Register the two new modules. |
| `crates/domain/src/entities.rs` | `ComposeStack.env`. |
| `crates/domain/src/contracts.rs` | `OverrideStore::write` and `ContainerRuntime::services` signatures; `mark_deploy_success` gains the sha. |
| `crates/domain/src/lib.rs` | Register `runtimevars`. |
| `crates/infrastructure/src/overrides.rs` | Multi-service YAML emission via `serde_yaml`. |
| `crates/infrastructure/src/docker.rs` | Process environment from `stack.env`; `-e` flags on `exec`; `services()` discovery. |
| `crates/infrastructure/src/repo.rs`, `sqlite.rs` | `last_commit_sha` column and migration. |
| `crates/application/src/deploy.rs` | Service enumeration, override wiring, stack env, sha on success. |
| `crates/application/src/{command,lifecycle,remove,secrets,environments}.rs` | Fill `ComposeStack.env`. |

---

### Task 1: Variable engine

**Files:**
- Create: `crates/bin/src/cli/vars.rs`
- Modify: `crates/bin/src/cli/mod.rs`
- Modify: `crates/bin/src/cli/overlay.rs` (delete the old `parse_vars` and `is_valid_var_name`, re-export from `vars`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum VarRef { User(String), Input(String, String) }` — `Input(namespace, field)`, both lowercase.
  - `pub struct VarSet { pub user: BTreeMap<String, String>, pub inputs: BTreeMap<String, String> }` — `inputs` is keyed by the dotted name, e.g. `"git.branch"`.
  - `pub fn parse_vars(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>>`
  - `pub fn refs(field: &str, value: &str) -> anyhow::Result<Vec<VarRef>>`
  - `pub fn substitute(field: &str, value: &str, vars: &VarSet) -> anyhow::Result<String>`
  - `impl VarRef { pub fn name(&self) -> String }` — `"FOO"` or `"git.branch"`.

- [ ] **Step 1: Write the failing tests**

Create `crates/bin/src/cli/vars.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> VarSet {
        let mut v = VarSet::default();
        v.user.insert("BRANCH_NAME".into(), "feature/login".into());
        v.inputs.insert("git.branch".into(), "main".into());
        v.inputs.insert("env.slug".into(), "feature-login".into());
        v
    }

    #[test]
    fn parse_vars_accepts_arbitrary_names() {
        let vars = parse_vars(&["FOO=bar".into(), "BRANCH_NAME=x/y".into()]).unwrap();
        assert_eq!(vars["FOO"], "bar");
        assert_eq!(vars["BRANCH_NAME"], "x/y");
        assert!(parse_vars(&[]).unwrap().is_empty());
    }

    #[test]
    fn parse_vars_keeps_everything_after_the_first_equals() {
        let vars = parse_vars(&["DSN=postgres://u:p@h/db?a=b".into()]).unwrap();
        assert_eq!(vars["DSN"], "postgres://u:p@h/db?a=b");
    }

    #[test]
    fn parse_vars_allows_an_empty_value() {
        let vars = parse_vars(&["EMPTY=".into()]).unwrap();
        assert_eq!(vars["EMPTY"], "");
    }

    #[test]
    fn parse_vars_rejects_bad_names_rpi_prefix_and_duplicates() {
        for (bad, needle) in [
            ("NOEQUALS", "KEY=VALUE"),
            ("lower=x", "^[A-Z][A-Z0-9_]*$"),
            ("1BAD=x", "^[A-Z][A-Z0-9_]*$"),
            ("RPI_X=1", "reserved"),
        ] {
            let err = parse_vars(&[bad.to_string()]).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad}: {err}");
        }
        let err = parse_vars(&["A=1".into(), "A=2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn refs_lists_both_namespaces_in_order() {
        let found = refs("ingress.hostname", "${env.slug}.${BRANCH_NAME}.example.com").unwrap();
        assert_eq!(
            found,
            vec![
                VarRef::Input("env".into(), "slug".into()),
                VarRef::User("BRANCH_NAME".into()),
            ]
        );
        assert!(refs("source.repo", "git@github.com:a/b.git")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn substitutes_both_namespaces_and_repeated_references() {
        let out = substitute(
            "ingress.hostname",
            "${env.slug}.${env.slug}.${git.branch}.example.com",
            &set(),
        )
        .unwrap();
        assert_eq!(out, "feature-login.feature-login.main.example.com");
    }

    #[test]
    fn dollar_dollar_brace_escapes_to_a_literal_reference() {
        let out = substitute("commands.backup", "sh -c 'tar -C $${HOME} .'", &set()).unwrap();
        assert_eq!(out, "sh -c 'tar -C ${HOME} .'");
        // The escape is inert as a reference: refs() must not report HOME.
        assert!(refs("commands.backup", "sh -c 'tar -C $${HOME} .'")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lone_dollars_are_literal() {
        for text in ["echo $$", "cost is $5", "$", "a$b"] {
            assert_eq!(substitute("commands.x", text, &set()).unwrap(), text);
            assert!(refs("commands.x", text).unwrap().is_empty());
        }
    }

    #[test]
    fn rpi_reference_names_the_runtime_rule() {
        let err = refs("ingress.hostname", "${RPI_ENV_SLUG}.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(err.contains("${env.slug}"), "must suggest the fix: {err}");

        let err = refs("source.branch", "${RPI_BRANCH_NAME}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(
            !err.contains("${env.slug}"),
            "the slug hint is only for RPI_ENV_SLUG: {err}"
        );
    }

    #[test]
    fn unknown_namespace_lists_the_available_ones() {
        let err = refs("ingress.hostname", "${foo.bar}").unwrap_err().to_string();
        assert!(err.contains("unknown namespace 'foo'"), "got: {err}");
        assert!(err.contains("git, env"), "got: {err}");
    }

    #[test]
    fn malformed_names_and_unclosed_braces_are_errors() {
        for bad in ["${}", "${a.b.c}", "${Mixed.case}", "${git.}", "${.branch}"] {
            let err = refs("ingress.hostname", bad).unwrap_err().to_string();
            assert!(err.contains("ingress.hostname"), "{bad}: {err}");
        }
        let err = refs("source.branch", "${BRANCH_NAME")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unclosed"), "got: {err}");
    }

    #[test]
    fn substituting_an_absent_variable_lists_what_is_available() {
        let err = substitute("source.branch", "${NOPE}", &set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("NOPE"), "got: {err}");
        assert!(err.contains("BRANCH_NAME"), "got: {err}");
        assert!(err.contains("git.branch"), "got: {err}");

        let err = substitute("ingress.hostname", "${env.name}", &set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("env.name"), "got: {err}");
    }

    #[test]
    fn var_ref_name_round_trips_both_forms() {
        assert_eq!(VarRef::User("FOO".into()).name(), "FOO");
        assert_eq!(VarRef::Input("git".into(), "branch".into()).name(), "git.branch");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-bin cli::vars 2>&1 | tail -20`
Expected: FAIL — the module is not registered and the items do not exist.

- [ ] **Step 3: Register the module**

In `crates/bin/src/cli/mod.rs`, add `pub mod vars;` alphabetically among the existing `pub mod` lines.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/bin/src/cli/vars.rs`, above the test module:

```rust
//! The configuration variable engine (spec 2026-07-26).
//!
//! Three disjoint namespaces, told apart by syntax alone:
//!
//! - `${NAME}` — a user variable supplied via `--vars`.
//! - `${ns.field}` — a resolver input: a fact about the machine running rpi
//!   (`git.*`) or about the resolution itself (`env.*`).
//! - `RPI_*` — runtime variables. They exist only inside containers and
//!   exec'd processes, never in a TOML file, so a reference to one here is a
//!   dedicated error rather than "unknown variable".
//!
//! The syntactic split is the point: nobody who sees `${git.branch}` in a
//! TOML file will go looking for `git.branch` in `printenv`.

use std::collections::BTreeMap;

/// The namespaces `${ns.field}` accepts.
const NAMESPACES: &[&str] = &["git", "env"];

/// One `${...}` reference found in a configuration string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum VarRef {
    User(String),
    /// `Input(namespace, field)` — both already validated as lowercase.
    Input(String, String),
}

impl VarRef {
    /// The reference as it is written in a TOML file, without `${}`.
    pub fn name(&self) -> String {
        match self {
            VarRef::User(name) => name.clone(),
            VarRef::Input(ns, field) => format!("{ns}.{field}"),
        }
    }
}

/// Everything one substitution pass may resolve. `inputs` is keyed by the
/// dotted name (`"git.branch"`), so a caller can insert a lazily computed
/// value without reconstructing a `VarRef`.
#[derive(Debug, Default, Clone)]
pub struct VarSet {
    pub user: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, String>,
}

impl VarSet {
    fn get(&self, r: &VarRef) -> Option<&String> {
        match r {
            VarRef::User(name) => self.user.get(name),
            VarRef::Input(..) => self.inputs.get(&r.name()),
        }
    }

    /// Sorted, comma-separated names — the "(available: ...)" error tail.
    fn available(&self) -> String {
        let mut names: Vec<&str> = self.user.keys().map(String::as_str).collect();
        names.extend(self.inputs.keys().map(String::as_str));
        names.sort_unstable();
        names.join(", ")
    }
}

fn is_user_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z')) && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

fn is_input_part(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z')) && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// `--vars KEY=VALUE` pairs. Names are arbitrary within the user charset;
/// `RPI_` is refused because that namespace only exists at runtime, so a
/// user value there could never be the one a container observes.
pub fn parse_vars(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            anyhow::bail!("--vars expects KEY=VALUE, got '{pair}'");
        };
        if !is_user_name(key) {
            anyhow::bail!("--vars: variable name '{key}' must match ^[A-Z][A-Z0-9_]*$");
        }
        if key.starts_with("RPI_") {
            anyhow::bail!("--vars: the RPI_ prefix is reserved for runtime variables ('{key}')");
        }
        if vars.insert(key.to_string(), value.to_string()).is_some() {
            anyhow::bail!("--vars: duplicate variable '{key}'");
        }
    }
    Ok(vars)
}

/// Classifies one reference body, or explains why it is not a valid one.
fn classify(field: &str, name: &str) -> anyhow::Result<VarRef> {
    if name.starts_with("RPI_") {
        let hint = if name == "RPI_ENV_SLUG" {
            "; did you mean ${env.slug}?"
        } else {
            ""
        };
        anyhow::bail!("{field}: RPI_* variables exist only at runtime{hint}");
    }
    if is_user_name(name) {
        return Ok(VarRef::User(name.to_string()));
    }
    if let Some((ns, rest)) = name.split_once('.') {
        if is_input_part(ns) && is_input_part(rest) {
            if !NAMESPACES.contains(&ns) {
                anyhow::bail!(
                    "{field}: unknown namespace '{ns}' (available: {})",
                    NAMESPACES.join(", ")
                );
            }
            return Ok(VarRef::Input(ns.to_string(), rest.to_string()));
        }
    }
    anyhow::bail!(
        "{field}: invalid variable name '{name}' (use ${{NAME}} for a --vars variable or ${{ns.field}} for a resolver input)"
    )
}

/// One step of the tokenizer: what sits at `rest`.
enum Token<'a> {
    /// `$${` — emit a literal `${`; `rest` continues after it.
    Escape(&'a str),
    /// A reference body plus the remainder after its closing brace.
    Reference(&'a str, &'a str),
}

/// Finds the next `$${` or `${` in `rest`. Returns the literal text before
/// it and what it was, or `None` when the remainder holds neither.
fn next_token<'a>(field: &str, rest: &'a str) -> anyhow::Result<Option<(&'a str, Token<'a>)>> {
    let mut from = 0usize;
    while let Some(offset) = rest[from..].find('$') {
        let at = from + offset;
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("${") {
            return Ok(Some((&rest[..at], Token::Escape(tail))));
        }
        if let Some(body) = after.strip_prefix('{') {
            let Some(end) = body.find('}') else {
                anyhow::bail!("{field}: unclosed ${{...}} in '{rest}'");
            };
            return Ok(Some((&rest[..at], Token::Reference(&body[..end], &body[end + 1..]))));
        }
        // A `$` that starts neither form is ordinary text; keep scanning.
        from = at + 1;
    }
    Ok(None)
}

/// Every reference in `value`, in order of appearance. Needs no values, so
/// callers use it to decide which lazy inputs to compute before substituting.
pub fn refs(field: &str, value: &str) -> anyhow::Result<Vec<VarRef>> {
    let mut found = Vec::new();
    let mut rest = value;
    while let Some((_, token)) = next_token(field, rest)? {
        rest = match token {
            Token::Escape(tail) => tail,
            Token::Reference(name, tail) => {
                found.push(classify(field, name)?);
                tail
            }
        };
    }
    Ok(found)
}

/// Substitutes every reference and turns `$${` into a literal `${`.
pub fn substitute(field: &str, value: &str, vars: &VarSet) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some((literal, token)) = next_token(field, rest)? {
        out.push_str(literal);
        rest = match token {
            Token::Escape(tail) => {
                out.push_str("${");
                tail
            }
            Token::Reference(name, tail) => {
                let r = classify(field, name)?;
                let Some(v) = vars.get(&r) else {
                    anyhow::bail!(
                        "{field}: unknown variable '{}' (available: {})",
                        r.name(),
                        vars.available()
                    );
                };
                out.push_str(v);
                tail
            }
        };
    }
    out.push_str(rest);
    Ok(out)
}
```

- [ ] **Step 5: Delete the superseded helpers in `overlay.rs`**

In `crates/bin/src/cli/overlay.rs`, delete `is_valid_var_name` (line ~28) and the whole `parse_vars` function (lines ~34-58), then add near the top:

```rust
pub use crate::cli::vars::parse_vars;
```

Delete the now-dead test `parse_vars_accepts_branch_name_only` from `overlay.rs`'s test module — its cases are covered by the new `vars.rs` tests and its central assertion ("only `BRANCH_NAME` is supported") is exactly what this change removes.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p pi-bin cli::vars 2>&1 | tail -20`
Expected: PASS, 13 tests.

Run: `cargo test --locked 2>&1 | tail -20`
Expected: the `overlay.rs` interpolation tests still pass (they use the old code path, which Task 3 replaces).

- [ ] **Step 7: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/cli/vars.rs crates/bin/src/cli/mod.rs crates/bin/src/cli/overlay.rs
git commit -m "feat(vars): variable engine with three namespaces and \$\$ escaping"
```

---

### Task 2: Git context inputs

**Files:**
- Create: `crates/bin/src/cli/gitctx.rs`
- Modify: `crates/bin/src/cli/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn branch(dir: &Path) -> anyhow::Result<String>`
  - `pub fn sha(dir: &Path) -> anyhow::Result<String>`
  - `pub fn short_sha(dir: &Path) -> anyhow::Result<String>`

  All three take the directory to run `git` in so tests can use a temporary repository instead of the ambient one. Production callers pass `Path::new(".")`.

- [ ] **Step 1: Write the failing tests**

Create `crates/bin/src/cli/gitctx.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A throwaway repository with one commit on branch `work`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet", "--initial-branch", "work"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "init"]);
        dir
    }

    #[test]
    fn reads_branch_and_shas() {
        let dir = repo();
        assert_eq!(branch(dir.path()).unwrap(), "work");

        let full = sha(dir.path()).unwrap();
        assert_eq!(full.len(), 40, "got: {full}");
        assert!(full.chars().all(|c| c.is_ascii_hexdigit()), "got: {full}");

        let short = short_sha(dir.path()).unwrap();
        assert!((7..=40).contains(&short.len()), "got: {short}");
        assert!(full.starts_with(&short), "{short} must prefix {full}");
    }

    #[test]
    fn detached_head_names_the_workaround() {
        let dir = repo();
        let head = sha(dir.path()).unwrap();
        let status = Command::new("git")
            .args(["checkout", "--quiet", &head])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let err = branch(dir.path()).unwrap_err().to_string();
        assert!(err.contains("detached"), "got: {err}");
        assert!(err.contains("--vars"), "must name the workaround: {err}");
        // The shas are still perfectly readable while detached.
        assert_eq!(sha(dir.path()).unwrap(), head);
    }

    #[test]
    fn outside_a_repository_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for err in [
            branch(dir.path()).unwrap_err().to_string(),
            sha(dir.path()).unwrap_err().to_string(),
        ] {
            assert!(err.contains("not a git repository"), "got: {err}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-bin cli::gitctx 2>&1 | tail -20`
Expected: FAIL — module not registered, functions missing.

- [ ] **Step 3: Register the module**

In `crates/bin/src/cli/mod.rs`, add `pub mod gitctx;` alphabetically.

- [ ] **Step 4: Write the implementation**

Prepend to `crates/bin/src/cli/gitctx.rs`:

```rust
//! Resolver inputs read from the local git repository (`${git.*}`).
//!
//! Every function shells out to `git` in an explicit directory rather than
//! the process working directory, so tests run against a throwaway
//! repository instead of whichever checkout happens to be current.
//!
//! These are computed lazily by the resolver — only when a configuration
//! actually references one — so `rpi config show` keeps working outside a
//! git repository for configurations that use no `${git.*}` variable.

use std::path::Path;
use std::process::Command;

fn run(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run git: {e}"))
}

fn inside_repository(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--git-dir"]).is_ok_and(|o| o.status.success())
}

fn trimmed(output: std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `${git.branch}`. A detached `HEAD` — the default state of a GitHub
/// Actions `push` job — is a hard error rather than the literal string
/// "HEAD": that would silently derive the project key `myapp--branch--head`
/// and let unrelated CI runs collide on one environment.
pub fn branch(dir: &Path) -> anyhow::Result<String> {
    // `symbolic-ref` exits nonzero on a detached HEAD, which is a cleaner
    // signal than matching "HEAD" out of `rev-parse --abbrev-ref`.
    let out = run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if out.status.success() {
        return Ok(trimmed(out));
    }
    if !inside_repository(dir) {
        anyhow::bail!("${{git.branch}}: not a git repository");
    }
    anyhow::bail!(
        "${{git.branch}}: HEAD is detached; pass the branch explicitly via --vars"
    )
}

/// `${git.sha}` — the full 40-character sha of `HEAD`.
pub fn sha(dir: &Path) -> anyhow::Result<String> {
    rev_parse(dir, "git.sha", &["rev-parse", "HEAD"])
}

/// `${git.short_sha}` — git's own abbreviation of `HEAD`, whose length is
/// whatever git considers unambiguous in that repository.
pub fn short_sha(dir: &Path) -> anyhow::Result<String> {
    rev_parse(dir, "git.short_sha", &["rev-parse", "--short", "HEAD"])
}

fn rev_parse(dir: &Path, var: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = run(dir, args)?;
    if out.status.success() {
        return Ok(trimmed(out));
    }
    if !inside_repository(dir) {
        anyhow::bail!("${{{var}}}: not a git repository");
    }
    anyhow::bail!(
        "${{{var}}}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p pi-bin cli::gitctx 2>&1 | tail -20`
Expected: PASS, 3 tests.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/cli/gitctx.rs crates/bin/src/cli/mod.rs
git commit -m "feat(vars): resolve git.branch/sha/short_sha as resolver inputs"
```

---

### Task 3: Two-phase resolution over the TOML value tree

This is the core task. It replaces `overlay.rs`'s `interpolate` (typed, two whitelisted fields, overlay only) with a `toml::Value` walk covering both files and every string field.

**Files:**
- Modify: `crates/bin/src/cli/overlay.rs` (replace `interpolate`, rework `resolve_from`)
- Modify: `crates/bin/src/cli/rpitoml.rs` (add `from_value`, expose `default_branch`)

**Interfaces:**
- Consumes: `vars::{parse_vars, refs, substitute, VarRef, VarSet}` (Task 1); `gitctx::{branch, sha, short_sha}` (Task 2).
- Produces:
  - `pub fn resolve_from(base_text: &str, overlay: Option<(&str, &str)>, vars: &[String]) -> anyhow::Result<Resolved>` — unchanged signature, new behavior.
  - `pub fn resolve(env: Option<&str>, vars: &[String]) -> anyhow::Result<Resolved>` — unchanged signature.
  - `pub fn derive_slug(branch: &str) -> anyhow::Result<String>` — unchanged.
  - `RpiToml::from_value(v: toml::Value) -> anyhow::Result<RpiToml>` and `RpiTomlOverlay::from_value(v: toml::Value, file: &str) -> anyhow::Result<RpiTomlOverlay>`.
  - `pub(crate) fn default_branch() -> String` in `rpitoml.rs` (currently private).

- [ ] **Step 1: Write the failing tests**

Replace the interpolation tests in `crates/bin/src/cli/overlay.rs`'s test module. Delete these now-obsolete tests: `interpolates_branch_and_hostname`, `static_overlay_is_not_parameterized`, `unknown_var_and_unclosed_brace_are_errors`, `interpolation_outside_allowed_fields_is_rejected`, `missing_branch_name_for_parameterized_overlay_is_an_error`, `static_overlay_ignores_underivable_branch_name_for_slug`, `multiple_references_in_one_string_are_substituted`, `interpolation_in_argv_and_table_commands_is_rejected`, `resolve_without_env_keeps_base_and_rejects_vars`, `vars_for_static_overlay_are_rejected`, `resolve_parameterized_env_uses_slug_in_key`.

Add:

```rust
    #[test]
    fn substitutes_in_every_field_kind_of_an_overlay() {
        let r = resolve_from(
            BASE,
            Some((
                "test",
                concat!(
                    "[source]\nbranch = \"${BRANCH_NAME}\"\n\n",
                    "[build]\ncompose = \"compose.${STAGE}.yml\"\n\n",
                    "[ingress]\nhostname = \"${STAGE}.example.com\"\nservice = \"${STAGE}\"\n\n",
                    "[secrets]\nenv = \".env.${STAGE}\"\nfiles = [\"certs/${STAGE}.pem\"]\n\n",
                    "[timeouts]\nbuild = \"${BUILD_BUDGET}\"\n\n",
                    "[commands]\nseed = \"node seed.js --env ${STAGE}\"\n",
                    "migrate = [\"npx\", \"migrate\", \"${STAGE}\"]\n",
                ),
            )),
            &[
                "BRANCH_NAME=develop".into(),
                "STAGE=test".into(),
                "BUILD_BUDGET=20m".into(),
            ],
        )
        .unwrap();
        let c = r.rpitoml;
        assert_eq!(c.source.branch, "develop");
        assert_eq!(c.build.compose, "compose.test.yml");
        assert_eq!(c.ingress.hostname.as_deref(), Some("test.example.com"));
        assert_eq!(c.ingress.service, "test");
        assert_eq!(c.secrets.env.as_deref(), Some(".env.test"));
        assert_eq!(c.secrets.files, vec!["certs/test.pem".to_string()]);
        assert_eq!(c.timeouts.build.as_deref(), Some("20m"));
        let commands = c.to_project_config().commands;
        assert_eq!(
            commands["seed"].argv,
            vec!["node", "seed.js", "--env", "test"]
        );
        assert_eq!(commands["migrate"].argv, vec!["npx", "migrate", "test"]);
    }

    #[test]
    fn substitutes_in_the_base_file_without_any_env() {
        let base = BASE.replace("branch = \"main\"", "branch = \"${BRANCH_NAME}\"");
        let r = resolve_from(&base, None, &["BRANCH_NAME=develop".into()]).unwrap();
        assert_eq!(r.rpitoml.source.branch, "develop");
        assert_eq!(r.rpitoml.project.name, "myapp", "no env, no derived key");
        assert!(r.env.is_none());
    }

    #[test]
    fn env_slug_derives_from_the_merged_branch_and_drives_the_key() {
        let r = resolve_from(
            BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"${env.slug}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--branch--feature-login");
        assert_eq!(r.rpitoml.source.branch, "feature/login");
        assert_eq!(
            r.rpitoml.ingress.hostname.as_deref(),
            Some("feature-login.preview.example.com")
        );
        assert_eq!(r.env.unwrap().slug.as_deref(), Some("feature-login"));
    }

    #[test]
    fn a_variable_that_is_not_the_slug_does_not_add_a_slug_suffix() {
        // The old rule granted the suffix for using ANY variable, which turned
        // a shared stand into a per-branch environment demanding a branch name.
        let r = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"test.example.com\"\n\n[secrets]\nenv = \".env.${STAGE}\"\n",
            )),
            &["STAGE=qa".into()],
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--test");
        assert_eq!(r.env.unwrap().slug, None);
    }

    #[test]
    fn env_name_is_available_as_an_input() {
        let r = resolve_from(
            BASE,
            Some((
                "test",
                "[ingress]\nhostname = \"${env.name}.example.com\"\n\n[secrets]\nenv = \".env.${env.name}\"\n",
            )),
            &[],
        )
        .unwrap();
        assert_eq!(r.rpitoml.ingress.hostname.as_deref(), Some("test.example.com"));
        assert_eq!(r.rpitoml.secrets.env.as_deref(), Some(".env.test"));
    }

    #[test]
    fn env_inputs_are_unavailable_without_an_env() {
        for text in ["${env.name}", "${env.slug}"] {
            let base = BASE.replace("branch = \"main\"", &format!("branch = \"{text}\""));
            let err = resolve_from(&base, None, &[]).unwrap_err().to_string();
            assert!(err.contains("unknown variable"), "{text}: {err}");
        }
    }

    #[test]
    fn escaped_reference_survives_into_a_command() {
        let base = BASE.replace(
            "seed = \"node seed.js\"",
            "seed = \"sh -c 'tar -C $${HOME} .'\"",
        );
        let r = resolve_from(&base, None, &[]).unwrap();
        assert_eq!(
            r.rpitoml.to_project_config().commands["seed"].argv,
            vec!["sh", "-c", "tar -C ${HOME} ."]
        );
    }

    #[test]
    fn unreferenced_vars_are_rejected_by_name() {
        let err = resolve_from(
            BASE,
            Some(("test", "[ingress]\nhostname = \"test.example.com\"\n")),
            &["TYPO=1".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("TYPO"), "got: {err}");
        assert!(err.contains("never referenced"), "got: {err}");
    }

    #[test]
    fn interpolation_in_the_project_name_is_rejected() {
        let base = BASE.replace("name = \"myapp\"", "name = \"app-${STAGE}\"");
        let err = resolve_from(&base, None, &["STAGE=qa".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("[project].name"), "got: {err}");
    }

    #[test]
    fn env_slug_inside_source_branch_is_a_circular_reference() {
        let err = resolve_from(
            BASE,
            Some(("branch", "[source]\nbranch = \"${env.slug}\"\n")),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("circular"), "got: {err}");
    }

    #[test]
    fn rpi_variables_are_rejected_with_the_runtime_hint() {
        let err = resolve_from(
            BASE,
            Some((
                "branch",
                "[ingress]\nhostname = \"${RPI_ENV_SLUG}.preview.example.com\"\n",
            )),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("runtime"), "got: {err}");
        assert!(err.contains("${env.slug}"), "got: {err}");
    }

    #[test]
    fn parameterized_env_without_a_slug_reference_warns_about_the_key() {
        let mut warnings = Vec::new();
        let r = resolve_with(
            BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"fixed.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert_eq!(r.rpitoml.project.name, "myapp--branch");
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(warnings[0].contains("${env.slug}"), "got: {warnings:?}");
    }

    #[test]
    fn a_static_env_with_no_vars_never_warns() {
        let mut warnings = Vec::new();
        resolve_with(
            BASE,
            Some(("test", "[ingress]\nhostname = \"test.example.com\"\n")),
            &[],
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert!(warnings.is_empty(), "got: {warnings:?}");
    }

    #[test]
    fn substituted_values_are_revalidated() {
        // A raw branch name substituted into a hostname is invalid DNS; the
        // check runs post-substitution, which is why ${env.slug} exists.
        let err = resolve_from(
            BASE,
            Some((
                "branch",
                "[ingress]\nhostname = \"${BRANCH_NAME}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hostname"), "got: {err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-bin cli::overlay 2>&1 | tail -30`
Expected: FAIL — `resolve_with` does not exist; the new behaviors are unimplemented.

- [ ] **Step 3: Expose what the resolver needs from `rpitoml.rs`**

In `crates/bin/src/cli/rpitoml.rs`:

Change `fn default_branch()` to `pub(crate) fn default_branch()`.

Split `RpiToml::parse` so the checks can run on an already-substituted tree:

```rust
    pub fn parse(text: &str) -> anyhow::Result<RpiToml> {
        RpiToml::from_value(toml::from_str(text)?)
    }

    /// Same checks as `parse`, but starting from an already-parsed (and,
    /// in the resolver's case, already-substituted) document.
    pub fn from_value(value: toml::Value) -> anyhow::Result<RpiToml> {
        let parsed: RpiToml = value.try_into()?;
        if parsed.schema != 1 {
            anyhow::bail!(
                "unsupported rpi.toml schema {} (this rpi supports schema 1)",
                parsed.schema
            );
        }
        if parsed.project.name.contains("--") {
            anyhow::bail!(
                "rpi.toml [project].name '{}' must not contain '--' (reserved for environment keys; rename the project)",
                parsed.project.name
            );
        }
        if parsed.environment_section.is_some() {
            anyhow::bail!(
                "rpi.toml: [environment] is only allowed in overlay files (rpi.<env>.toml)"
            );
        }
        parsed.validate_common()?;
        Ok(parsed)
    }
```

In `crates/bin/src/cli/overlay.rs`, give `RpiTomlOverlay` the same split:

```rust
    pub fn parse(text: &str, file: &str) -> anyhow::Result<RpiTomlOverlay> {
        let value: toml::Value = toml::from_str(text).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
        RpiTomlOverlay::from_value(value, file)
    }

    pub fn from_value(value: toml::Value, file: &str) -> anyhow::Result<RpiTomlOverlay> {
        let parsed: RpiTomlOverlay = value.try_into().map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
        if parsed.schema.is_some() {
            anyhow::bail!("{file}: `schema` is not allowed in an overlay (set it in rpi.toml)");
        }
        if parsed.project.is_some() {
            anyhow::bail!("{file}: [project] is not allowed in an overlay (the project key is derived by the CLI)");
        }
        Ok(parsed)
    }
```

- [ ] **Step 4: Replace `interpolate` with the value-tree walk**

In `crates/bin/src/cli/overlay.rs`, delete `substitute`, `forbid`, `command_strings` and `interpolate` entirely, and add:

```rust
use crate::cli::vars::{self, VarRef, VarSet};

/// Path of the one field resolved in phase 1. `env.slug` derives from it, so
/// it cannot itself see the slug — that is the circular reference guarded
/// below.
const BRANCH_PATH: &str = "source.branch";

/// Walks every string leaf of a TOML document, handing each one its dotted
/// path (`ingress.hostname`, `commands.seed`, `secrets.files.0`) so errors
/// and warnings can name the field the user actually wrote.
fn walk_strings(
    value: &mut toml::Value,
    path: &mut Vec<String>,
    f: &mut impl FnMut(&str, &mut String) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match value {
        toml::Value::String(s) => f(&path.join("."), s),
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                path.push(key.clone());
                walk_strings(child, path, f)?;
                path.pop();
            }
            Ok(())
        }
        toml::Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                path.push(i.to_string());
                walk_strings(child, path, f)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Every reference in the document, paired with the path it was found at.
fn collect_refs(value: &mut toml::Value) -> anyhow::Result<Vec<(String, VarRef)>> {
    let mut found = Vec::new();
    walk_strings(value, &mut Vec::new(), &mut |path, s| {
        for r in vars::refs(path, s)? {
            found.push((path.to_string(), r));
        }
        Ok(())
    })?;
    Ok(found)
}

/// Substitutes into the fields selected by `want`, leaving the rest alone.
fn substitute_where(
    value: &mut toml::Value,
    set: &VarSet,
    want: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    walk_strings(value, &mut Vec::new(), &mut |path, s| {
        if want(path) {
            *s = vars::substitute(path, s, set)?;
        }
        Ok(())
    })
}

/// Reads a string at a dotted path without disturbing the tree.
fn string_at(value: &toml::Value, path: &str) -> Option<String> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    current.as_str().map(str::to_string)
}

/// Computes the `${git.*}` inputs the documents actually reference. Lazy on
/// purpose: `rpi config show` must keep working outside a git repository for
/// a configuration that never asks for one.
fn git_inputs(refs: &[(String, VarRef)]) -> anyhow::Result<BTreeMap<String, String>> {
    let dir = std::path::Path::new(".");
    let mut out = BTreeMap::new();
    for (_, r) in refs {
        let VarRef::Input(ns, field) = r else { continue };
        if ns != "git" || out.contains_key(&r.name()) {
            continue;
        }
        let value = match field.as_str() {
            "branch" => crate::cli::gitctx::branch(dir)?,
            "sha" => crate::cli::gitctx::sha(dir)?,
            "short_sha" => crate::cli::gitctx::short_sha(dir)?,
            other => anyhow::bail!(
                "unknown variable 'git.{other}' (available: git.branch, git.sha, git.short_sha)"
            ),
        };
        out.insert(r.name(), value);
    }
    Ok(out)
}
```

- [ ] **Step 5: Rewrite `resolve_from` around the two phases**

Replace the whole body of `resolve_from` in `crates/bin/src/cli/overlay.rs`, and add the warning-sink variant the tests use:

```rust
/// Same as `resolve_from`, but the caller supplies the warning sink. Tests
/// capture warnings; `resolve_from` sends them to `output::warn`.
pub fn resolve_with(
    base_text: &str,
    overlay: Option<(&str, &str)>,
    vars_arg: &[String],
    warn: &mut dyn FnMut(&str),
) -> anyhow::Result<Resolved> {
    let user = vars::parse_vars(vars_arg)?;
    let mut base_v: toml::Value =
        toml::from_str(base_text).map_err(|e| anyhow::anyhow!("rpi.toml: {e}"))?;

    // The project key must stay static: it drives base--env[--slug]
    // derivation and the production-key-hijack check, neither of which is
    // verifiable against a computed name.
    if let Some(name) = string_at(&base_v, "project.name") {
        if !vars::refs("[project].name", &name)?.is_empty() {
            anyhow::bail!("[project].name: ${{...}} is not allowed (the project key must be static)");
        }
    }

    let env_name = overlay.map(|(name, _)| name);
    if let Some(name) = env_name {
        validate_env_name(name)?;
    }
    let file = env_name.map(|n| format!("rpi.{n}.toml")).unwrap_or_default();
    let mut overlay_v = match overlay {
        None => None,
        Some((_, text)) => {
            Some(toml::from_str::<toml::Value>(text).map_err(|e| anyhow::anyhow!("{file}: {e}"))?)
        }
    };

    // One reference sweep over both documents feeds every decision below:
    // which git inputs to compute, whether the slug is wanted, and which
    // --vars keys went unused.
    let mut all_refs = collect_refs(&mut base_v)?;
    if let Some(o) = &mut overlay_v {
        all_refs.extend(collect_refs(o)?);
    }

    for key in user.keys() {
        if !all_refs.iter().any(|(_, r)| r == &VarRef::User(key.clone())) {
            anyhow::bail!(
                "--vars: variable '{key}' is never referenced in rpi.toml{}",
                if file.is_empty() {
                    String::new()
                } else {
                    format!(" or {file}")
                }
            );
        }
    }

    let slug_ref = VarRef::Input("env".into(), "slug".into());
    if all_refs
        .iter()
        .any(|(path, r)| path == BRANCH_PATH && r == &slug_ref)
    {
        anyhow::bail!("{BRANCH_PATH}: circular reference - env.slug is derived from {BRANCH_PATH}");
    }

    let mut set = VarSet {
        user: user.clone(),
        inputs: git_inputs(&all_refs)?,
    };
    if let Some(name) = env_name {
        set.inputs.insert("env.name".into(), name.to_string());
    }

    // Phase 1: source.branch only.
    substitute_where(&mut base_v, &set, |p| p == BRANCH_PATH)?;
    if let Some(o) = &mut overlay_v {
        substitute_where(o, &set, |p| p == BRANCH_PATH)?;
    }

    let effective_branch = overlay_v
        .as_ref()
        .and_then(|o| string_at(o, BRANCH_PATH))
        .or_else(|| string_at(&base_v, BRANCH_PATH))
        .unwrap_or_else(crate::cli::rpitoml::default_branch);

    let wants_slug = all_refs.iter().any(|(_, r)| r == &slug_ref);
    if wants_slug {
        set.inputs
            .insert("env.slug".into(), derive_slug(&effective_branch)?);
    }

    // Phase 2: everything except the field already done.
    substitute_where(&mut base_v, &set, |p| p != BRANCH_PATH)?;
    if let Some(o) = &mut overlay_v {
        substitute_where(o, &set, |p| p != BRANCH_PATH)?;
    }

    let mut base = RpiToml::from_value(base_v)?;
    let Some(env_name) = env_name else {
        return Ok(Resolved {
            rpitoml: base,
            env: None,
        });
    };
    let mut overlay = RpiTomlOverlay::from_value(overlay_v.expect("env implies an overlay"), &file)?;

    let slug = wants_slug.then(|| derive_slug(&effective_branch)).transpose()?;
    if !user.is_empty() && slug.is_none() {
        warn(&format!(
            "{file} uses --vars but never ${{env.slug}}, so the key stays '{}--{env_name}' with no per-branch suffix",
            base.project.name
        ));
    }

    let environment = overlay.environment.take();
    let base_name = base.project.name.clone();
    let base_hostname = base.ingress.hostname.clone();
    apply_overlay(&mut base, overlay);
    let key = derive_key(&base_name, env_name, slug.as_deref());
    base.project.name = key.clone();
    base.validate_common()
        .map_err(|e| anyhow::anyhow!("{file}: merged configuration is invalid: {e}"))?;

    if let (Some(base_h), Some(merged_h)) = (&base_hostname, &base.ingress.hostname) {
        if base_h == merged_h {
            anyhow::bail!(
                "{file}: [ingress].hostname equals the base hostname '{base_h}' - an environment must override it or clear it (hostname = \"\")"
            );
        }
    }

    let ttl_secs = match environment.as_ref().and_then(|e| e.ttl.as_deref()) {
        Some(ttl) => Some(
            crate::duration::parse_duration_secs(ttl)
                .map_err(|e| anyhow::anyhow!("{file} [environment].ttl: {e}"))?,
        ),
        None => None,
    };
    let on_create = environment.and_then(|e| e.on_create);
    if let Some(cmd) = &on_create {
        let declared = base.commands.as_ref().is_some_and(|c| c.contains_key(cmd));
        if !declared {
            anyhow::bail!(
                "{file} [environment].on_create: command '{cmd}' is not declared in the merged [commands]"
            );
        }
    }

    Ok(Resolved {
        rpitoml: base,
        env: Some(EnvSelection {
            env: env_name.to_string(),
            base: base_name,
            slug,
            ttl_secs,
            on_create,
        }),
    })
}

/// Same as `resolve_with`, sending warnings to the standard output helper.
pub fn resolve_from(
    base_text: &str,
    overlay: Option<(&str, &str)>,
    vars_arg: &[String],
) -> anyhow::Result<Resolved> {
    resolve_with(base_text, overlay, vars_arg, &mut |w| {
        crate::output::warn(w)
    })
}
```

Delete the now-unused `RESERVED_ENV_NAMES` guard duplication in `resolve` (it calls `validate_env_name` before reading the file, which stays) — no change needed there beyond leaving `resolve` as it is.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p pi-bin cli::overlay 2>&1 | tail -40`
Expected: PASS. If `merge_replaces_scalars_field_wise` or the `[environment]`/hostname tests fail, the merge and validation order was disturbed — they must keep passing untouched.

- [ ] **Step 7: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/cli/overlay.rs crates/bin/src/cli/rpitoml.rs
git commit -m "feat(vars): substitute across the whole config in two phases"
```

---

### Task 4: `rpi env destroy --key` / `reset-data --key`

The new slug derivation reads `source.branch` from the overlay, so key derivation from `--vars` alone is no longer possible. `--key` restores the ability to clean up an environment whose overlay was deleted or no longer resolves.

**Files:**
- Modify: `crates/bin/src/main.rs` (the `EnvCmd::Destroy` and `EnvCmd::ResetData` variants and their dispatch arms)
- Modify: `crates/bin/src/cli/envcmds.rs`

**Interfaces:**
- Consumes: `overlay::{resolve, derive_key, validate_env_name}`.
- Produces: `envcmds::env_destroy(env: Option<String>, key: Option<String>, vars: Vec<String>, yes: bool, connect: ConnectOpts)` and the same shape for `env_reset_data`.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `crates/bin/src/cli/envcmds.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_key_is_accepted_as_is() {
        assert_eq!(
            target_key(None, Some("myapp--branch--feature-login".into()), &[]).unwrap(),
            "myapp--branch--feature-login"
        );
        assert_eq!(
            target_key(None, Some("myapp--test".into()), &[]).unwrap(),
            "myapp--test"
        );
    }

    #[test]
    fn a_malformed_key_is_rejected_before_any_agent_call() {
        for bad in [
            "myapp",                       // no env part at all
            "myapp--",                     // empty env part
            "--test",                      // empty base
            "myapp--Test",                 // uppercase
            "myapp--test--slug--extra",    // too many parts
            "myapp--test--",               // empty slug
            "myapp---test",                // leading '-' in the env part
        ] {
            let err = target_key(None, Some(bad.into()), &[]).unwrap_err().to_string();
            assert!(err.contains("--key"), "{bad}: {err}");
        }
    }

    #[test]
    fn key_and_env_are_mutually_exclusive_and_one_is_required() {
        let err = target_key(Some("test".into()), Some("myapp--test".into()), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--key"), "got: {err}");

        let err = target_key(None, None, &[]).unwrap_err().to_string();
        assert!(err.contains("--key"), "got: {err}");
    }

    #[test]
    fn key_path_ignores_vars_entirely() {
        // --key exists for a directory that no longer resolves, so it must
        // not consult --vars, rpi.toml, or the overlay.
        assert_eq!(
            target_key(None, Some("myapp--test".into()), &["TYPO=1".into()]).unwrap(),
            "myapp--test"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-bin cli::envcmds 2>&1 | tail -20`
Expected: FAIL — `target_key` does not exist.

- [ ] **Step 3: Replace `resolve_key` with `target_key`**

In `crates/bin/src/cli/envcmds.rs`, delete `resolve_key` and its imports of `derive_slug`/`parse_vars`, and add:

```rust
/// One part of a derived key (`base`, `env`, or `slug`): lowercase, no `--`,
/// no leading or trailing `-`. Mirrors the agent's own `is_valid_env_part`.
fn is_valid_key_part(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

/// The environment key `destroy`/`reset-data` should act on.
///
/// `--key` names it outright and reads no configuration file at all — not
/// the overlay, and not `rpi.toml` either. It is the escape hatch for a
/// project directory that no longer resolves, so depending on any local file
/// would defeat it. `rpi env ls` prints the exact string to pass.
///
/// The `<env>` form resolves the overlay the same way `rpi deploy` does,
/// because the slug now derives from the merged `source.branch`.
fn target_key(env: Option<String>, key: Option<String>, vars: &[String]) -> anyhow::Result<String> {
    match (env, key) {
        (Some(_), Some(_)) => {
            anyhow::bail!("--key and <env> are mutually exclusive: pass one or the other")
        }
        (None, None) => anyhow::bail!("pass an environment name, or --key <full-key>"),
        (None, Some(key)) => {
            let parts: Vec<&str> = key.split("--").collect();
            let shaped = matches!(parts.len(), 2 | 3) && parts.iter().all(|p| is_valid_key_part(p));
            if !shaped {
                anyhow::bail!(
                    "--key '{key}' is not an environment key (expected base--env or base--env--slug); run `rpi env ls` to see the exact key"
                );
            }
            Ok(key)
        }
        (Some(env), None) => {
            let resolved = crate::cli::overlay::resolve(Some(&env), vars)?;
            Ok(resolved.rpitoml.project.name)
        }
    }
}
```

Note the `<env>` branch now returns `rpitoml.project.name`, which `resolve` has already overwritten with the derived key — so `derive_key` and `validate_env_name` are no longer needed here. Update the `use` line at the top of the file to just `use crate::cli::overlay;` plus what remains in use.

- [ ] **Step 4: Rewire the two commands**

In `crates/bin/src/cli/envcmds.rs`, change both signatures and their first line:

```rust
pub async fn env_destroy(
    env: Option<String>,
    key: Option<String>,
    vars: Vec<String>,
    yes: bool,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let key = target_key(env, key, &vars)?;
    confirm_key(
        "DESTROY (stack, volumes, ingress, DNS, secrets, registry) of",
        &key,
        yes,
    )?;
    // ... rest unchanged
```

```rust
pub async fn env_reset_data(
    env: Option<String>,
    key: Option<String>,
    vars: Vec<String>,
    yes: bool,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let key = target_key(env, key, &vars)?;
    confirm_key("REMOVE ALL DATA (volumes) of", &key, yes)?;
    // ... rest unchanged
```

The final success line of `env_reset_data` interpolates `env`, which is now an `Option` consumed by `target_key`. Replace that message with one built from the key:

```rust
    output::success(format!(
        "environment '{key}' data removed - the next deploy of it re-runs on_create"
    ));
```

- [ ] **Step 5: Add the CLI flags**

In `crates/bin/src/main.rs`, change both `EnvCmd` variants:

```rust
    /// Destroy an environment: stack, volumes, ingress, DNS, secrets, registry
    Destroy {
        /// Environment name from rpi.<env>.toml (resolves the overlay)
        env: Option<String>,
        /// Full environment key from `rpi env ls`; reads no config file
        #[arg(long, conflicts_with_all = ["env", "vars"])]
        key: Option<String>,
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
    /// Remove the environment's volumes and re-run on_create on next deploy
    ResetData {
        /// Environment name from rpi.<env>.toml (resolves the overlay)
        env: Option<String>,
        /// Full environment key from `rpi env ls`; reads no config file
        #[arg(long, conflicts_with_all = ["env", "vars"])]
        key: Option<String>,
        #[arg(long = "vars")]
        vars: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        connect: cli::config::ConnectOpts,
    },
```

Update both dispatch arms to pass `key` through:

```rust
        Cmd::Env {
            cmd: EnvCmd::Destroy { env, key, vars, yes, connect },
        } => cli::envcmds::env_destroy(env, key, vars, yes, connect).await,
        Cmd::Env {
            cmd: EnvCmd::ResetData { env, key, vars, yes, connect },
        } => cli::envcmds::env_reset_data(env, key, vars, yes, connect).await,
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p pi-bin cli::envcmds 2>&1 | tail -20`
Expected: PASS, 4 tests.

Run: `cargo run -p pi-bin --bin rpi -- env destroy --help 2>&1 | tail -20`
Expected: `--key <KEY>` is listed, and `<ENV>` is optional.

- [ ] **Step 7: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/cli/envcmds.rs crates/bin/src/main.rs
git commit -m "feat(env): target an environment by full key with --key"
```

---

### Task 5: `RPI_` guard in secrets, `[runtime]` preview in `config show`

**Files:**
- Modify: `crates/bin/src/cli/commands.rs` (`collect_secrets`, `config_show`)

**Interfaces:**
- Consumes: `overlay::{resolve, render_resolved, Resolved}`.
- Produces: `pub fn render_runtime_preview(r: &Resolved) -> String` in `commands.rs`, used only by `config_show`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/bin/src/cli/commands.rs`'s test module:

```rust
    #[test]
    fn secrets_reject_the_reserved_rpi_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\nRPI_BRANCH_NAME=nope\n").unwrap();
        let err = collect_secrets(dir.path(), &section(None, &[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("RPI_BRANCH_NAME"), "got: {err}");
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn runtime_preview_lists_local_values_and_marks_agent_side_ones() {
        let resolved = crate::cli::overlay::resolve_from(
            OVERLAY_BASE,
            Some((
                "branch",
                "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"${env.slug}.preview.example.com\"\n",
            )),
            &["BRANCH_NAME=feature/login".into()],
        )
        .unwrap();
        let text = render_runtime_preview(&resolved);
        assert!(text.contains("[runtime]"), "got:\n{text}");
        assert!(text.contains("RPI_PROJECT = \"myapp--branch--feature-login\""), "got:\n{text}");
        assert!(text.contains("RPI_PROJECT_BASE = \"myapp\""), "got:\n{text}");
        assert!(text.contains("RPI_ENV = \"branch\""), "got:\n{text}");
        assert!(text.contains("RPI_ENV_SLUG = \"feature-login\""), "got:\n{text}");
        assert!(text.contains("RPI_BRANCH_NAME = \"feature/login\""), "got:\n{text}");
        assert!(
            text.contains("RPI_HOSTNAME = \"feature-login.preview.example.com\""),
            "got:\n{text}"
        );
        assert!(text.contains("RPI_HOST_PORT = \"<assigned by agent>\""), "got:\n{text}");
        assert!(text.contains("RPI_COMMIT_SHA = \"<assigned by agent>\""), "got:\n{text}");
    }

    #[test]
    fn runtime_preview_omits_variables_that_do_not_exist() {
        let base = OVERLAY_BASE.replace("hostname = \"app.example.com\"\n", "");
        let resolved = crate::cli::overlay::resolve_from(&base, None, &[]).unwrap();
        let text = render_runtime_preview(&resolved);
        assert!(text.contains("RPI_PROJECT = \"myapp\""), "got:\n{text}");
        assert!(!text.contains("RPI_ENV "), "no env selected: {text}");
        assert!(!text.contains("RPI_ENV_SLUG"), "no slug: {text}");
        assert!(!text.contains("RPI_HOSTNAME"), "no hostname: {text}");
    }
```

Add the fixture the two preview tests share, next to the other consts in that test module:

```rust
    const OVERLAY_BASE: &str = r#"
schema = 1

[project]
name = "myapp"

[source]
repo = "git@github.com:acme/myapp.git"
branch = "main"

[ingress]
hostname = "app.example.com"
service = "web"
port = 3000
"#;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-bin cli::commands 2>&1 | tail -20`
Expected: FAIL — `render_runtime_preview` missing; `collect_secrets` accepts `RPI_*`.

- [ ] **Step 3: Reject `RPI_` keys in `collect_secrets`**

In `crates/bin/src/cli/commands.rs`, immediately after the `vars` binding inside `collect_secrets` (before the `files` collection), add:

```rust
    // The RPI_ namespace is injected by the agent at deploy time. Accepting a
    // secret with that prefix would make it ambiguous which side wins inside
    // the container, so it is refused here rather than silently overridden.
    if let Some(key) = vars.keys().find(|k| k.starts_with("RPI_")) {
        anyhow::bail!(
            "secret key '{key}' uses the reserved RPI_ prefix (rpi injects those at deploy time) - rename it"
        );
    }
```

- [ ] **Step 4: Render the `[runtime]` preview**

Add to `crates/bin/src/cli/commands.rs`:

```rust
/// The `RPI_*` variables a deploy of this configuration would export, as far
/// as they are knowable locally. `RPI_HOST_PORT` and `RPI_COMMIT_SHA` are
/// assigned by the agent (port allocation and fetch respectively), so they
/// are shown as placeholders rather than omitted — an operator debugging a
/// missing variable needs to see that they exist.
pub fn render_runtime_preview(r: &crate::cli::overlay::Resolved) -> String {
    let mut out = String::from("\n[runtime]\n");
    let mut put = |key: &str, value: &str| {
        out.push_str(&format!("{key} = \"{value}\"\n"));
    };
    put("RPI_PROJECT", &r.rpitoml.project.name);
    match &r.env {
        Some(env) => {
            put("RPI_PROJECT_BASE", &env.base);
            put("RPI_ENV", &env.env);
            if let Some(slug) = &env.slug {
                put("RPI_ENV_SLUG", slug);
            }
        }
        None => put("RPI_PROJECT_BASE", &r.rpitoml.project.name),
    }
    put("RPI_BRANCH_NAME", &r.rpitoml.source.branch);
    if let Some(hostname) = &r.rpitoml.ingress.hostname {
        put("RPI_HOSTNAME", hostname);
    }
    put("RPI_HOST_PORT", "<assigned by agent>");
    put("RPI_COMMIT_SHA", "<assigned by agent>");
    out
}
```

Then extend `config_show`:

```rust
pub async fn config_show(env: Option<String>, vars: Vec<String>) -> anyhow::Result<()> {
    let resolved = crate::cli::overlay::resolve(env.as_deref(), &vars)?;
    print!("{}", crate::cli::overlay::render_resolved(&resolved)?);
    print!("{}", render_runtime_preview(&resolved));
    Ok(())
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p pi-bin cli::commands 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/cli/commands.rs
git commit -m "feat(vars): reserve RPI_ in secrets and preview runtime vars in config show"
```

---

### Task 6: The `RPI_*` map and the `last_commit_sha` column

**Files:**
- Create: `crates/domain/src/runtimevars.rs`
- Modify: `crates/domain/src/lib.rs`, `crates/domain/src/entities.rs` (`Project.last_commit_sha`), `crates/domain/src/contracts.rs` (`mark_deploy_success`)
- Modify: `crates/infrastructure/src/sqlite.rs` (migration), `crates/infrastructure/src/repo.rs`
- Modify: `crates/application/src/deploy.rs` (call site only — passes `None` for now)

**Interfaces:**
- Consumes: `entities::{Project, ProjectConfig, EnvironmentMeta}`.
- Produces:
  - `pub fn rpi_vars(project: &Project, commit_sha: Option<&str>) -> BTreeMap<String, String>`
  - `Project.last_commit_sha: Option<String>`
  - `ProjectRepository::mark_deploy_success(&self, name: &str, at: i64, commit_sha: Option<&str>) -> Result<(), DomainError>`

- [ ] **Step 1: Write the failing tests**

Create `crates/domain/src/runtimevars.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{EnvironmentMeta, ExposeMode, HealthcheckConfig, ProjectConfig, StageTimeoutOverrides};

    fn project(name: &str) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: "git@github.com:acme/myapp.git".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 3000,
                hostname: Some("app.example.com".into()),
                expose: ExposeMode::default(),
                healthcheck: HealthcheckConfig::default(),
                timeouts: StageTimeoutOverrides::default(),
                commands: Default::default(),
                command_timeout_secs: None,
                environment: None,
            },
            host_port: 8000,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
            last_commit_sha: None,
        }
    }

    #[test]
    fn plain_project_exports_the_base_set() {
        let vars = rpi_vars(&project("myapp"), Some("abc123"));
        assert_eq!(vars["RPI_PROJECT"], "myapp");
        assert_eq!(vars["RPI_PROJECT_BASE"], "myapp");
        assert_eq!(vars["RPI_BRANCH_NAME"], "main");
        assert_eq!(vars["RPI_HOSTNAME"], "app.example.com");
        assert_eq!(vars["RPI_HOST_PORT"], "8000");
        assert_eq!(vars["RPI_COMMIT_SHA"], "abc123");
        assert!(!vars.contains_key("RPI_ENV"), "no environment: {vars:?}");
        assert!(!vars.contains_key("RPI_ENV_SLUG"), "{vars:?}");
    }

    #[test]
    fn environment_project_adds_env_base_and_slug() {
        let mut p = project("myapp--branch--feature-login");
        p.config.branch = "feature/login".into();
        p.config.environment = Some(EnvironmentMeta {
            env: "branch".into(),
            base: "myapp".into(),
            slug: Some("feature-login".into()),
            ttl_secs: None,
            on_create: None,
        });
        let vars = rpi_vars(&p, None);
        assert_eq!(vars["RPI_PROJECT"], "myapp--branch--feature-login");
        assert_eq!(vars["RPI_PROJECT_BASE"], "myapp");
        assert_eq!(vars["RPI_ENV"], "branch");
        assert_eq!(vars["RPI_ENV_SLUG"], "feature-login");
        assert_eq!(vars["RPI_BRANCH_NAME"], "feature/login");
    }

    #[test]
    fn absent_values_are_omitted_rather_than_empty() {
        let mut p = project("myapp");
        p.config.hostname = None;
        let vars = rpi_vars(&p, None);
        for absent in ["RPI_HOSTNAME", "RPI_COMMIT_SHA", "RPI_ENV", "RPI_ENV_SLUG"] {
            assert!(!vars.contains_key(absent), "{absent} must be omitted: {vars:?}");
        }
        assert!(vars.values().all(|v| !v.is_empty()), "{vars:?}");
    }

    #[test]
    fn the_registry_sha_is_the_fallback_when_no_deploy_is_in_flight() {
        let mut p = project("myapp");
        p.last_commit_sha = Some("stored".into());
        assert_eq!(rpi_vars(&p, None)["RPI_COMMIT_SHA"], "stored");
        assert_eq!(
            rpi_vars(&p, Some("fresh"))["RPI_COMMIT_SHA"],
            "fresh",
            "the in-flight deploy wins over the stored value"
        );
    }

    #[test]
    fn every_exported_name_uses_the_reserved_prefix() {
        let vars = rpi_vars(&project("myapp"), Some("abc"));
        assert!(vars.keys().all(|k| k.starts_with("RPI_")), "{vars:?}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pi-domain runtimevars 2>&1 | tail -20`
Expected: FAIL — module unregistered, `last_commit_sha` missing.

- [ ] **Step 3: Add the field, the module, and the function**

In `crates/domain/src/entities.rs`, add to `struct Project`:

```rust
    /// Commit sha of the most recent successful deploy. Feeds
    /// `RPI_COMMIT_SHA` outside a deploy (`rpi command`, `rpi restart`),
    /// where no fetch has just happened to supply one.
    pub last_commit_sha: Option<String>,
```

In `crates/domain/src/lib.rs`, add `pub mod runtimevars;`.

Prepend to `crates/domain/src/runtimevars.rs`:

```rust
//! The `RPI_*` runtime variables (spec 2026-07-26).
//!
//! These exist only inside containers and exec'd processes — never in a TOML
//! file. The whole set derives from registry state the agent already holds,
//! which is why nothing about them travels on the wire: computing them in one
//! place makes it impossible for the CLI and the agent to disagree.

use std::collections::BTreeMap;

use crate::entities::Project;

/// The variables a deploy of `project` exports.
///
/// `commit_sha` is the sha of the deploy currently in flight; outside a
/// deploy pass `None` and the project's last successful sha is used. A value
/// that does not exist is omitted from the map rather than set to an empty
/// string, so `${RPI_ENV:-prod}` works inside a container.
pub fn rpi_vars(project: &Project, commit_sha: Option<&str>) -> BTreeMap<String, String> {
    let config = &project.config;
    let mut vars = BTreeMap::new();
    let mut put = |key: &str, value: String| {
        vars.insert(key.to_string(), value);
    };

    put("RPI_PROJECT", config.name.clone());
    match &config.environment {
        Some(env) => {
            put("RPI_PROJECT_BASE", env.base.clone());
            put("RPI_ENV", env.env.clone());
            if let Some(slug) = &env.slug {
                put("RPI_ENV_SLUG", slug.clone());
            }
        }
        None => put("RPI_PROJECT_BASE", config.name.clone()),
    }
    put("RPI_BRANCH_NAME", config.branch.clone());
    if let Some(hostname) = &config.hostname {
        put("RPI_HOSTNAME", hostname.clone());
    }
    put("RPI_HOST_PORT", project.host_port.to_string());
    if let Some(sha) = commit_sha.or(project.last_commit_sha.as_deref()) {
        put("RPI_COMMIT_SHA", sha.to_string());
    }
    vars
}
```

- [ ] **Step 4: Migrate the registry**

In `crates/infrastructure/src/sqlite.rs`, append one more `M::up` to the end of the `Migrations::new(vec![...])` list (order matters — never insert into the middle):

```rust
        M::up("ALTER TABLE projects ADD COLUMN last_commit_sha TEXT;"),
```

In `crates/infrastructure/src/repo.rs`:

Extend `SELECT` with the new column (append, so existing positional indices stay valid):

```rust
const SELECT: &str = "SELECT name, repo, branch, compose_path, service, container_port, hostname, host_port, created_at, expose, commands, command_timeout_secs, env_name, env_base, env_slug, env_ttl_secs, env_on_create, env_on_create_done, last_success_at, last_commit_sha FROM projects";
```

In the row-mapping function, add after `last_success_at: row.get(18)?,`:

```rust
        last_commit_sha: row.get(19)?,
```

Rewrite `mark_deploy_success`:

```rust
    async fn mark_deploy_success(
        &self,
        name: &str,
        at: i64,
        commit_sha: Option<&str>,
    ) -> Result<(), DomainError> {
        let name = name.to_string();
        let commit_sha = commit_sha.map(str::to_string);
        self.db
            .call(move |conn| {
                conn.execute(
                    "UPDATE projects SET last_success_at = ?2, last_commit_sha = COALESCE(?3, last_commit_sha) WHERE name = ?1",
                    params![name, at, commit_sha],
                )
                .map(|_| ())
                .map_err(storage_err)
            })
            .await
    }
```

`COALESCE` so a caller with no sha to offer never erases the stored one.

In `crates/domain/src/contracts.rs`, update the trait method:

```rust
    /// Sets last_success_at (TTL sliding anchor) and, when a sha is given,
    /// last_commit_sha (source of RPI_COMMIT_SHA outside a deploy).
    async fn mark_deploy_success<'a>(
        &self,
        name: &str,
        at: i64,
        commit_sha: Option<&'a str>,
    ) -> Result<(), DomainError>;
```

- [ ] **Step 5: Fix every call site and `Project` literal**

`cargo build --all-targets --locked` now fails at each place that constructs a `Project` or calls `mark_deploy_success`. Fix them mechanically:

- Add `last_commit_sha: None,` to every `Project { ... }` literal. They live in the test modules of `crates/application/src/{deploy,command,lifecycle,remove,secrets,environments,list,gc}.rs` and `crates/infrastructure/src/repo.rs`, plus `crates/bin/src/agent/http.rs` if it builds one.
- In `crates/application/src/deploy.rs`, change the success call to pass `None` for now (Task 9 supplies the real sha):
  ```rust
                if let Err(err) = self
                    .projects
                    .mark_deploy_success(&config.name, finished_at, None)
                    .await
  ```
- Mock expectations written as `.expect_mark_deploy_success().returning(|_, _| Ok(()))` gain a third parameter: `.returning(|_, _, _| Ok(()))`.

Run `cargo build --all-targets --locked 2>&1 | grep -E "^error" | head -30` and repeat until clean.

- [ ] **Step 6: Add a repository round-trip test**

Add to `crates/infrastructure/src/repo.rs`'s test module:

```rust
    #[tokio::test]
    async fn mark_deploy_success_stores_the_sha_and_never_erases_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(&dir, 8000, 8999);
        repo.upsert(&cfg("a")).await.unwrap();

        repo.mark_deploy_success("a", 100, Some("sha-one")).await.unwrap();
        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_success_at, Some(100));
        assert_eq!(p.last_commit_sha.as_deref(), Some("sha-one"));

        repo.mark_deploy_success("a", 200, None).await.unwrap();
        let p = repo.get("a").await.unwrap().unwrap();
        assert_eq!(p.last_success_at, Some(200));
        assert_eq!(
            p.last_commit_sha.as_deref(),
            Some("sha-one"),
            "a call with no sha must not erase the stored one"
        );
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p pi-domain runtimevars 2>&1 | tail -20` → PASS, 5 tests.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 8: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/domain crates/infrastructure crates/application
git commit -m "feat(runtime): derive the RPI_* map and persist last_commit_sha"
```

---

### Task 7: Carry the runtime environment on `ComposeStack`

Putting the map on the stack rather than in six trait signatures means every compose invocation — including `down` and `lifecycle`, which must still parse a compose file that may reference `${RPI_*}` — gets it automatically.

**Files:**
- Modify: `crates/domain/src/entities.rs` (`ComposeStack.env`)
- Modify: `crates/domain/src/contracts.rs` (`ContainerRuntime::services`)
- Modify: `crates/infrastructure/src/docker.rs`
- Modify: all `ComposeStack { .. }` construction sites (behavior-neutral for now)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `ComposeStack.env: BTreeMap<String, String>`
  - `ContainerRuntime::services(&self, stack: &ComposeStack) -> Result<Vec<String>, DomainError>`
  - `docker::discovery_chain(stack: &ComposeStack) -> Vec<PathBuf>` (crate-private)

- [ ] **Step 1: Write the failing tests**

Add to `crates/infrastructure/src/docker.rs`'s test module:

```rust
    fn stack_with_env(workdir: &std::path::Path) -> ComposeStack {
        let mut s = stack(workdir);
        s.env = [
            ("RPI_PROJECT".to_string(), "rateme".to_string()),
            ("RPI_BRANCH_NAME".to_string(), "main".to_string()),
        ]
        .into_iter()
        .collect();
        s
    }

    #[test]
    fn compose_exports_the_runtime_environment_to_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = DockerComposeRuntime.compose(&stack_with_env(dir.path()), &["up", "-d"]);
        let envs: HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        assert_eq!(envs["RPI_PROJECT"], "rateme");
        assert_eq!(envs["RPI_BRANCH_NAME"], "main");
        assert_eq!(
            envs["BUILDKIT_PROGRESS"], "plain",
            "the existing export must survive"
        );
    }

    #[test]
    fn exec_passes_the_runtime_environment_as_e_flags() {
        let env: std::collections::BTreeMap<String, String> = [
            ("RPI_BRANCH_NAME".to_string(), "main".to_string()),
            ("RPI_PROJECT".to_string(), "rateme".to_string()),
        ]
        .into_iter()
        .collect();
        let argv = strings(&["node", "seed.js"]);
        assert_eq!(
            exec_tail("web", &argv, &env),
            vec![
                "exec",
                "-T",
                "-e",
                "RPI_BRANCH_NAME=main",
                "-e",
                "RPI_PROJECT=rateme",
                "web",
                "node",
                "seed.js",
            ],
            "flags precede the service name, in BTreeMap order"
        );
        assert_eq!(
            exec_tail("web", &argv, &Default::default()),
            vec!["exec", "-T", "web", "node", "seed.js"],
            "an empty map must produce the original argv shape"
        );
    }

    #[test]
    fn service_discovery_ignores_the_generated_override() {
        // The generated override is what we are about to write; including it
        // would let a service left over from a previous deploy reappear in
        // the list and be re-created as a phantom with no image.
        let dir = tempfile::tempdir().unwrap();
        let repo_override = dir.path().join("docker-compose.override.yml");
        std::fs::write(&repo_override, "services: {}").unwrap();
        let pi_override = dir.path().join("pi-override.yml");
        std::fs::write(&pi_override, "services: {}").unwrap();
        let s = ComposeStack {
            project_name: "rateme".into(),
            workdir: dir.path().to_path_buf(),
            compose_file: dir.path().join("docker-compose.yml"),
            override_file: pi_override,
            env: Default::default(),
        };
        assert_eq!(
            discovery_chain(&s),
            vec![s.compose_file.clone(), repo_override],
            "the pi override must not take part in discovery"
        );
        assert!(file_chain(&s).len() > discovery_chain(&s).len());
    }
```

Add `use std::collections::HashMap;` to the test module if it is not already imported at the top of the file (it is imported at file scope already, so `HashMap` resolves).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-infrastructure docker 2>&1 | tail -20`
Expected: FAIL — `ComposeStack` has no `env`; `exec_tail` takes three arguments; `discovery_chain` missing.

- [ ] **Step 3: Add the field and the trait method**

In `crates/domain/src/entities.rs`, extend `ComposeStack`:

```rust
pub struct ComposeStack {
    pub project_name: String,
    pub workdir: PathBuf,
    pub compose_file: PathBuf,
    pub override_file: PathBuf,
    /// `RPI_*` runtime variables. Exported into the environment of every
    /// `docker compose` invocation for this stack, so `${RPI_*}` interpolates
    /// inside the project's own compose file, and passed as `-e` flags on
    /// `exec`. Empty for call paths that do not need them.
    pub env: BTreeMap<String, String>,
}
```

`BTreeMap` is already imported at the top of `entities.rs`.

In `crates/domain/src/contracts.rs`, add to `ContainerRuntime`, right after `ps`:

```rust
    /// Service names of the stack, from the compose file plus the
    /// repository's own override — deliberately excluding the generated
    /// override, which is what the caller is about to write.
    async fn services(&self, stack: &ComposeStack) -> Result<Vec<String>, DomainError>;
```

- [ ] **Step 4: Implement in the docker adapter**

In `crates/infrastructure/src/docker.rs`:

Replace `exec_tail`:

```rust
pub(crate) fn exec_tail<'a>(
    service: &'a str,
    argv: &'a [String],
    env: &'a std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut tail = vec!["exec".to_string(), "-T".to_string()];
    // A container started by an earlier deploy carries that deploy's
    // environment; `-e` makes the exec'd process see current values.
    for (key, value) in env {
        tail.push("-e".to_string());
        tail.push(format!("{key}={value}"));
    }
    tail.push(service.to_string());
    tail.extend(argv.iter().cloned());
    tail
}
```

Add the discovery chain next to `file_chain`:

```rust
/// Files consulted when *listing* services, as opposed to acting on them.
/// The generated override is excluded on purpose: it is the file about to be
/// rewritten, and a service it still carries from a previous deploy would be
/// re-created as a phantom with an `environment:` block and no image.
pub(crate) fn discovery_chain(stack: &ComposeStack) -> Vec<PathBuf> {
    let mut files = vec![stack.compose_file.clone()];
    let repo_override = stack.workdir.join("docker-compose.override.yml");
    if repo_override.exists() && repo_override != stack.compose_file {
        files.push(repo_override);
    }
    files
}
```

Export the stack environment in `compose`:

```rust
    fn compose(&self, stack: &ComposeStack, tail: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.args(compose_args(&stack.project_name, &file_chain(stack), tail));
        cmd.current_dir(&stack.workdir);
        // BuildKit's fancy multi-line, cursor-redrawing progress UI corrupts
        // captured output even when stdout is a pipe, not a TTY — plain mode
        // emits ordinary newline-terminated lines instead. Applies uniformly
        // to every subcommand; a no-op for ones that don't build anything.
        cmd.env("BUILDKIT_PROGRESS", "plain");
        // Compose gives the shell environment precedence over `.env`, so this
        // makes `${RPI_*}` interpolate inside the project's compose file
        // without touching the secrets bundle that owns `.env`.
        cmd.envs(&stack.env);
        cmd
    }
```

Update the `exec` implementation and add `services`:

```rust
    async fn services(&self, stack: &ComposeStack) -> Result<Vec<String>, DomainError> {
        let mut cmd = Command::new("docker");
        cmd.args(compose_args(
            &stack.project_name,
            &discovery_chain(stack),
            &["config", "--services"],
        ));
        cmd.current_dir(&stack.workdir);
        cmd.envs(&stack.env);
        let out = run_capture(cmd).await.map_err(DomainError::Runtime)?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn exec(
        &self,
        stack: &ComposeStack,
        service: &str,
        argv: &[String],
        log: Arc<dyn LogSink>,
    ) -> Result<i32, DomainError> {
        log.line(&format!("docker compose exec -T {service} ..."));
        let tail = exec_tail(service, argv, &stack.env);
        let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
        run_streamed_code(self.compose(stack, &tail), log)
            .await
            .map_err(DomainError::Runtime)
    }
```

- [ ] **Step 5: Fix every construction site and hand-written mock**

Add `env: Default::default(),` to every `ComposeStack { ... }` literal — production sites `crates/application/src/{command,deploy,environments,lifecycle,remove,secrets}.rs` and the two test helpers in `crates/infrastructure/src/docker.rs`. Task 10 replaces the defaults with real values.

Add a `services` implementation to the two hand-written `ContainerRuntime` impls in test modules (`CountingRuntime` in `crates/application/src/deploy.rs`, `HangingRuntime` in `crates/application/src/command.rs`):

```rust
            async fn services(&self, _: &ComposeStack) -> Result<Vec<String>, DomainError> {
                Ok(vec![])
            }
```

`MockContainerRuntime` picks the new method up automatically from `automock`.

Run `cargo build --all-targets --locked 2>&1 | grep -E "^error" | head -30` until clean.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p pi-infrastructure docker 2>&1 | tail -20` → PASS.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 7: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/domain crates/infrastructure crates/application
git commit -m "feat(runtime): carry RPI_* on ComposeStack and discover services"
```

---

### Task 8: Multi-service override emission

**Files:**
- Modify: `crates/infrastructure/src/overrides.rs`
- Modify: `crates/domain/src/contracts.rs` (`OverrideStore::write`)
- Modify: `crates/application/src/deploy.rs` (call site only — behavior-neutral for now)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  ```rust
  async fn write(
      &self,
      project: &str,
      service: &str,
      bind: &str,
      host_port: u16,
      container_port: u16,
      services: &[String],
      env: &BTreeMap<String, String>,
  ) -> Result<PathBuf, DomainError>;
  ```
  `service` remains the public service (ports plus restart policy); `services` is every service that should receive `environment`, and may or may not contain `service`.

- [ ] **Step 1: Write the failing tests**

Replace the YAML tests in `crates/infrastructure/src/overrides.rs`'s test module (keep `write_creates_file_in_overrides_dir`, updating its call) and add:

```rust
    fn env() -> BTreeMap<String, String> {
        [
            ("RPI_BRANCH_NAME".to_string(), "feature/login".to_string()),
            ("RPI_PROJECT".to_string(), "myapp--branch".to_string()),
        ]
        .into_iter()
        .collect()
    }

    /// Parse the emitted YAML rather than string-matching it: the whole point
    /// of the rewrite is that values are escaped by an emitter.
    fn parsed(yaml: &str) -> serde_yaml::Value {
        serde_yaml::from_str(yaml).expect("emitted override must be valid YAML")
    }

    #[test]
    fn public_service_keeps_ports_and_restart_policy() {
        let yaml = override_yaml("web", "127.0.0.1", 8000, 3000, &["web".into()], &env());
        let doc = parsed(&yaml);
        let web = &doc["services"]["web"];
        assert_eq!(web["restart"].as_str(), Some("unless-stopped"));
        assert_eq!(
            web["ports"][0].as_str(),
            Some("127.0.0.1:8000:3000"),
            "got:\n{yaml}"
        );
        assert_eq!(
            web["environment"]["RPI_BRANCH_NAME"].as_str(),
            Some("feature/login")
        );
    }

    #[test]
    fn every_other_service_gets_environment_only() {
        let yaml = override_yaml(
            "web",
            "127.0.0.1",
            8000,
            3000,
            &["web".into(), "worker".into(), "db".into()],
            &env(),
        );
        let doc = parsed(&yaml);
        for name in ["worker", "db"] {
            let svc = &doc["services"][name];
            assert_eq!(
                svc["environment"]["RPI_PROJECT"].as_str(),
                Some("myapp--branch"),
                "{name} must receive the runtime environment"
            );
            assert!(svc["ports"].is_null(), "{name} must not publish ports");
            assert!(svc["restart"].is_null(), "{name} must not pin a restart policy");
        }
    }

    #[test]
    fn environment_is_a_mapping_so_compose_merges_it_by_key() {
        let yaml = override_yaml("web", "127.0.0.1", 8000, 3000, &["web".into()], &env());
        let doc = parsed(&yaml);
        assert!(
            doc["services"]["web"]["environment"].is_mapping(),
            "a sequence would append instead of overriding: {yaml}"
        );
    }

    #[test]
    fn public_service_is_emitted_even_when_discovery_missed_it() {
        let yaml = override_yaml("web", "127.0.0.1", 8000, 3000, &[], &env());
        let doc = parsed(&yaml);
        assert_eq!(
            doc["services"]["web"]["ports"][0].as_str(),
            Some("127.0.0.1:8000:3000")
        );
    }

    #[test]
    fn empty_environment_emits_no_environment_key() {
        let yaml = override_yaml(
            "web",
            "127.0.0.1",
            8000,
            3000,
            &["web".into(), "worker".into()],
            &BTreeMap::new(),
        );
        let doc = parsed(&yaml);
        assert!(doc["services"]["web"]["environment"].is_null());
        assert!(
            doc["services"]["worker"].is_null(),
            "a service with nothing to say must not appear at all"
        );
    }

    #[test]
    fn values_needing_quoting_survive_a_round_trip() {
        let mut env = BTreeMap::new();
        env.insert("RPI_BRANCH_NAME".to_string(), "fix/\"quoted\": #1".to_string());
        env.insert("RPI_HOST_PORT".to_string(), "8000".to_string());
        let yaml = override_yaml("web", "127.0.0.1", 8000, 3000, &["web".into()], &env);
        let doc = parsed(&yaml);
        assert_eq!(
            doc["services"]["web"]["environment"]["RPI_BRANCH_NAME"].as_str(),
            Some("fix/\"quoted\": #1")
        );
        assert_eq!(
            doc["services"]["web"]["environment"]["RPI_HOST_PORT"].as_str(),
            Some("8000"),
            "a numeric-looking value must stay a string"
        );
    }

    #[test]
    fn generated_marker_comment_is_preserved() {
        let yaml = override_yaml("web", "127.0.0.1", 8000, 3000, &["web".into()], &env());
        assert!(yaml.starts_with("# generated by pi - do not edit\n"), "got:\n{yaml}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-infrastructure overrides 2>&1 | tail -20`
Expected: FAIL — `override_yaml` takes four arguments.

- [ ] **Step 3: Rewrite the emitter**

Replace the top of `crates/infrastructure/src/overrides.rs`:

```rust
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pi_domain::contracts::OverrideStore;
use pi_domain::error::DomainError;
use serde::Serialize;

/// One service block of the generated override. Every field is skipped when
/// absent, so a non-public service emits `environment` alone.
#[derive(Serialize, Default)]
struct ServiceOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    restart: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ports: Option<Vec<String>>,
    /// A mapping, never a sequence: compose merges mappings key-wise, so the
    /// generated override (last in the file chain) wins over a same-named key
    /// in the project's own compose file. A sequence would append instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<BTreeMap<String, String>>,
}

#[derive(Serialize)]
struct OverrideDoc {
    services: BTreeMap<String, ServiceOverride>,
}

/// Text of the override file (§12.1): the public service's bind address, plus
/// the `RPI_*` runtime environment for every service of the stack.
///
/// The public service also gets `restart: unless-stopped`. pi owns that
/// service's lifecycle, so it must survive a host reboot without manual
/// intervention; without an explicit policy the container inherits Docker's
/// default `no` and stays down until someone runs `docker start`.
/// `unless-stopped` (not `always`) so a deliberate `rpi stop` is respected
/// across reboots.
///
/// `services` comes from compose discovery and may be empty or stale, so the
/// public service is emitted unconditionally — its port mapping is the one
/// thing the deploy cannot work without.
pub(crate) fn override_yaml(
    service: &str,
    bind: &str,
    host_port: u16,
    container_port: u16,
    services: &[String],
    env: &BTreeMap<String, String>,
) -> String {
    let mut doc = OverrideDoc {
        services: BTreeMap::new(),
    };
    let environment = (!env.is_empty()).then(|| env.clone());
    for name in services {
        if name == service {
            continue;
        }
        if environment.is_none() {
            continue;
        }
        doc.services.insert(
            name.clone(),
            ServiceOverride {
                environment: environment.clone(),
                ..Default::default()
            },
        );
    }
    doc.services.insert(
        service.to_string(),
        ServiceOverride {
            restart: Some("unless-stopped".to_string()),
            ports: Some(vec![format!("{bind}:{host_port}:{container_port}")]),
            environment,
        },
    );
    let body = serde_yaml::to_string(&doc).expect("override doc is always serializable");
    format!("# generated by pi - do not edit\n{body}")
}
```

Rewrite the `write` implementation:

```rust
    async fn write(
        &self,
        project: &str,
        service: &str,
        bind: &str,
        host_port: u16,
        container_port: u16,
        services: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<PathBuf, DomainError> {
        let io_err = |e: std::io::Error| DomainError::Storage(format!("override write: {e}"));
        tokio::fs::create_dir_all(&self.dir).await.map_err(io_err)?;
        let path = self.dir.join(format!("{project}.yml"));
        tokio::fs::write(
            &path,
            override_yaml(service, bind, host_port, container_port, services, env),
        )
        .await
        .map_err(io_err)?;
        Ok(path)
    }
```

Update the trait in `crates/domain/src/contracts.rs` to the signature in the Interfaces block above, adding `use std::collections::BTreeMap;` if the file lacks it.

- [ ] **Step 4: Fix the call site and mock expectations**

In `crates/application/src/deploy.rs`, extend the `overrides.write(...)` call with behaviour-neutral arguments (Task 9 replaces them):

```rust
        let override_file = self
            .overrides
            .write(
                &config.name,
                &config.service,
                config.expose.bind_addr(),
                project.host_port,
                config.container_port,
                &[],
                &Default::default(),
            )
            .await?;
```

`crates/application/src/secrets.rs:91` calls `overrides.write` too (the `--apply` path). Extend it the same way:

```rust
        let override_file = self
            .overrides
            .write(
                project,
                &config.service,
                registered.config.expose.bind_addr(),
                registered.host_port,
                config.container_port,
                &[],
                &Default::default(),
            )
            .await?;
```

Task 10 replaces those two arguments with real values.

Every `.expect_write()` on `MockOverrideStore` gains two closure parameters: `.returning(|_, _, _, _, _| ...)` becomes `.returning(|_, _, _, _, _, _, _| ...)`, and `.withf(|p, s, bind, hp, cp| ...)` becomes `.withf(|p, s, bind, hp, cp, _, _| ...)`.

Run `cargo build --all-targets --locked 2>&1 | grep -E "^error" | head -30` until clean.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p pi-infrastructure overrides 2>&1 | tail -20` → PASS.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/infrastructure/src/overrides.rs crates/domain/src/contracts.rs crates/application/src/deploy.rs
git commit -m "feat(runtime): emit a multi-service override with the RPI_* environment"
```

---

### Task 9: Wire the deploy pipeline

**Files:**
- Modify: `crates/application/src/deploy.rs`

**Interfaces:**
- Consumes: `pi_domain::runtimevars::rpi_vars` (Task 6); `ContainerRuntime::services`, `ComposeStack.env` (Task 7); the new `OverrideStore::write` (Task 8).
- Produces: nothing new; this is the integration point.

- [ ] **Step 1: Write the failing tests**

Add to `crates/application/src/deploy.rs`'s test module:

```rust
    #[tokio::test]
    async fn override_receives_every_discovered_service_and_the_runtime_env() {
        let mut m = mocks();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretsBundle::default()));
        m.runtime
            .expect_services()
            .times(1)
            .returning(|_| Ok(vec!["web".into(), "worker".into()]));
        m.overrides
            .expect_write()
            .withf(|_, _, _, _, _, services, env| {
                services == ["web".to_string(), "worker".to_string()]
                    && env["RPI_PROJECT"] == "rateme"
                    && env["RPI_BRANCH_NAME"] == "main"
                    && env["RPI_HOST_PORT"] == "8000"
                    && env["RPI_COMMIT_SHA"] == SHA
                    && env["RPI_HOSTNAME"] == "rateme.isskelo.com"
            })
            .times(1)
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime
            .expect_build()
            .withf(|stack, _| stack.env["RPI_COMMIT_SHA"] == SHA)
            .returning(|_, _| Ok(()));
        m.runtime
            .expect_up()
            .withf(|stack, _| stack.env["RPI_PROJECT"] == "rateme")
            .returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        let result = build(m)
            .execute(
                "dep-env-vars".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, DeploymentStatus::Success);
    }

    #[tokio::test]
    async fn service_discovery_failure_fails_the_deploy_before_build() {
        let mut m = mocks();
        ok_pre_stages_without_override(&mut m);
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretsBundle::default()));
        m.runtime
            .expect_services()
            .returning(|_| Err(DomainError::Runtime("compose config: no such file".into())));
        m.overrides.expect_write().times(0);
        m.runtime.expect_build().times(0);
        m.runtime.expect_up().times(0);

        let err = build(m)
            .execute(
                "dep-svc".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Runtime(_)), "got: {err}");
    }

    #[tokio::test]
    async fn successful_deploy_records_the_commit_sha_on_the_project() {
        let mut m = mocks();
        m.projects.checkpoint();
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.projects
            .expect_mark_deploy_success()
            .withf(|name, _, sha| name == "rateme" && *sha == Some(SHA))
            .times(1)
            .returning(|_, _, _| Ok(()));
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.secrets
            .expect_load()
            .returning(|_| Ok(SecretsBundle::default()));
        m.runtime.expect_services().returning(|_| Ok(vec!["web".into()]));
        m.overrides
            .expect_write()
            .returning(|_, _, _, _, _, _, _| Ok(PathBuf::from("/ov.yml")));
        m.runtime.expect_build().returning(|_, _| Ok(()));
        m.runtime.expect_up().returning(|_, _| Ok(()));
        m.runtime.expect_ps().returning(|_| Ok(vec![]));
        m.health.expect_check().returning(|_, _, _| Ok(()));
        m.ingress
            .expect_upsert()
            .returning(|_, _, _| Ok(IngressOutcome::Applied));
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));

        build(m)
            .execute(
                "dep-sha".into(),
                sample_config(),
                DeployRef::Branch("main".into()),
                CollectSink::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
```

Add the helper the second test uses, next to `ok_pre_stages`:

```rust
    /// `ok_pre_stages` minus the override expectation, for tests that assert
    /// the override is never written.
    fn ok_pre_stages_without_override(m: &mut Mocks) {
        m.projects.expect_upsert().returning(|c| {
            Ok(Project {
                config: c.clone(),
                host_port: 8000,
                created_at: 1,
                on_create_done: false,
                last_success_at: None,
                last_commit_sha: None,
            })
        });
        m.source.expect_fetch().returning(|_, _, _| {
            Ok(FetchedSource {
                workdir: PathBuf::from("/wd"),
                commit_sha: SHA.into(),
            })
        });
        m.history.expect_mark_running().returning(|_, _| Ok(()));
        m.history
            .expect_record_finished()
            .returning(|_, _, _, _, _| Ok(()));
    }
```

Two mock expectations are new for **every** test in this module that reaches the override stage. Add both to `ok_pre_stages` and to each test that builds its own mock set instead of using it:

```rust
        m.runtime
            .expect_services()
            .returning(|_| Ok(vec!["web".into()]));
        // The discovery stack is built before `write` returns a path, so the
        // deploy asks the store where the override lives.
        m.overrides
            .expect_path()
            .returning(|p| PathBuf::from("/ov").join(p));
```

`expect_path` is mandatory: mockall panics on an unexpected call, and `DeployProject` did not call `OverrideStore::path` before this task.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-application deploy 2>&1 | tail -30`
Expected: FAIL — `services` is never called; the override receives empty arguments; `mark_deploy_success` receives `None`.

- [ ] **Step 3: Wire the pipeline**

In `crates/application/src/deploy.rs`, replace the block between the secrets injection and the `ComposeStack` construction:

```rust
        // The RPI_* map is derived from registry state plus the sha this
        // deploy just fetched, so the CLI never has to send it.
        let env = pi_domain::runtimevars::rpi_vars(&project, Some(&fetched.commit_sha));

        // Services are discovered before the override is written, from the
        // compose file plus the repository's own override only. A failure
        // here fails the deploy: a silently missing environment variable is a
        // far worse outcome than an explicit error, and a compose file that
        // cannot be parsed would fail `build` moments later anyway.
        let discovery = ComposeStack {
            project_name: config.name.clone(),
            workdir: fetched.workdir.clone(),
            compose_file: fetched.workdir.join(&config.compose_path),
            override_file: self.overrides.path(&config.name),
            env: env.clone(),
        };
        let services = self.runtime.services(&discovery).await?;
        log.line(&format!(
            "runtime env: {} vars over {} service(s)",
            env.len(),
            services.len()
        ));

        let override_file = self
            .overrides
            .write(
                &config.name,
                &config.service,
                config.expose.bind_addr(),
                project.host_port,
                config.container_port,
                &services,
                &env,
            )
            .await?;

        let stack = ComposeStack {
            project_name: config.name.clone(),
            workdir: fetched.workdir.clone(),
            compose_file: fetched.workdir.join(&config.compose_path),
            override_file,
            env,
        };
```

`DeployProject` does not currently hold anything that resolves the override path before `write` returns, but `OverrideStore::path` is already on the trait — no new dependency is needed.

Then pass the sha on success:

```rust
                if let Err(err) = self
                    .projects
                    .mark_deploy_success(
                        &config.name,
                        finished_at,
                        deployment.commit_sha.as_deref(),
                    )
                    .await
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p pi-application deploy 2>&1 | tail -30` → PASS.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/application/src/deploy.rs
git commit -m "feat(runtime): inject RPI_* into the deploy pipeline"
```

---

### Task 10: Fill the runtime environment on the remaining use cases

`rpi command`, `rpi start/stop/restart`, `rpi secrets send --apply`, `rpi rm` and `rpi env destroy/reset-data` all run compose against a stack whose compose file may reference `${RPI_*}`. Each must build the same map.

**Files:**
- Modify: `crates/application/src/{command,lifecycle,remove,secrets,environments}.rs`

**Interfaces:**
- Consumes: `pi_domain::runtimevars::rpi_vars` (Task 6), `ComposeStack.env` (Task 7).
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

Add to `crates/application/src/command.rs`'s test module:

```rust
    #[tokio::test]
    async fn exec_carries_the_runtime_environment() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_exec()
            .withf(|stack, _, _, _| {
                stack.env["RPI_PROJECT"] == "rateme"
                    && stack.env["RPI_BRANCH_NAME"] == "main"
                    && stack.env["RPI_HOST_PORT"] == "8000"
                    && stack.env["RPI_COMMIT_SHA"] == "stored-sha"
            })
            .returning(|_, _, _, _| Ok(0));

        let mut proj = project("rateme");
        proj.last_commit_sha = Some("stored-sha".into());
        let run = deps_with(runtime, proj);
        run.execute("rateme", "create-invite", &[], CollectSink::new())
            .await
            .unwrap();
    }
```

Add to `crates/application/src/lifecycle.rs`'s test module (mirroring whatever helper it already uses to build a `Project`):

```rust
    #[tokio::test]
    async fn lifecycle_carries_the_runtime_environment() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_lifecycle()
            .withf(|stack, _, _| stack.env["RPI_PROJECT"] == "rateme")
            .times(1)
            .returning(|_, _, _| Ok(()));
        let uc = deps_with(runtime, project("rateme"));
        uc.execute("rateme", LifecycleAction::Restart, CollectSink::new())
            .await
            .unwrap();
    }
```

If `lifecycle.rs` has no `deps_with`/`project` helper, copy the pattern from `command.rs`'s test module verbatim, adjusting the constructor to `Lifecycle::new(...)`'s actual dependencies.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pi-application 2>&1 | tail -20`
Expected: FAIL — `stack.env` is empty at every site.

- [ ] **Step 3: Fill the map at each construction site**

In `crates/application/src/command.rs`, inside `execute`, replace the `ComposeStack` literal:

```rust
        let stack = ComposeStack {
            project_name: registered.config.name.clone(),
            workdir,
            compose_file,
            override_file,
            // No deploy is in flight, so RPI_COMMIT_SHA comes from the
            // registry's record of the last successful one.
            env: pi_domain::runtimevars::rpi_vars(&registered, None),
        };
```

Apply the identical change at:
- `crates/application/src/lifecycle.rs:43` — `env: pi_domain::runtimevars::rpi_vars(&registered, None),`
- `crates/application/src/remove.rs:70` — `env: pi_domain::runtimevars::rpi_vars(&existing, None),`
- `crates/application/src/environments.rs:124` — `env: pi_domain::runtimevars::rpi_vars(&existing, None),`
- `crates/application/src/secrets.rs:101` — the surrounding function already binds `registered` (the `Project` fetched at line 71), so use `env: pi_domain::runtimevars::rpi_vars(&registered, None),`. This site also writes an override; give it the real values too, replacing the placeholders Task 8 left:

```rust
        let stack_env = pi_domain::runtimevars::rpi_vars(&registered, None);
        let services = self
            .runtime
            .services(&ComposeStack {
                project_name: config.name.clone(),
                workdir: workdir.clone(),
                compose_file: workdir.join(&config.compose_path),
                override_file: self.overrides.path(project),
                env: stack_env.clone(),
            })
            .await?;
        let override_file = self
            .overrides
            .write(
                project,
                &config.service,
                registered.config.expose.bind_addr(),
                registered.host_port,
                config.container_port,
                &services,
                &stack_env,
            )
            .await?;
        let stack = ComposeStack {
            project_name: config.name.clone(),
            workdir: workdir.clone(),
            compose_file: workdir.join(&config.compose_path),
            override_file,
            env: stack_env,
        };
```

Without this, `secrets send --apply` would rewrite the override *without* the environment blocks the last deploy put there, silently stripping `RPI_*` from every container until the next deploy. Add `m.overrides.expect_path()` and `m.runtime.expect_services()` to the mock sets in `secrets.rs`'s test module, matching Task 9's pattern.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p pi-application 2>&1 | tail -20` → PASS.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/application
git commit -m "feat(runtime): carry RPI_* through command, lifecycle and teardown"
```

---

### Task 11: `runtime-vars` capability gate

**Files:**
- Modify: `crates/bin/src/compat.rs`
- Modify: `crates/bin/src/cli/commands.rs` (`deploy`)

**Interfaces:**
- Consumes: `CompatSession::gate` (existing).
- Produces: `Feature::RuntimeVars`.

- [ ] **Step 1: Write the failing test**

Add to `crates/bin/src/compat.rs`'s test module:

```rust
    #[test]
    fn runtime_vars_warns_once_on_an_old_agent_without_failing() {
        let messages = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let messages = std::sync::Arc::clone(&messages);
            Box::new(move |m: &str| messages.lock().unwrap().push(m.to_string()))
        };
        let info = VersionInfo {
            version: "0.26.1".into(),
            api: "v1".into(),
            features: Some(vec!["environments".into()]),
        };
        let session = CompatSession::with_sink("0.27.0", &info, sink);

        assert_eq!(session.gate(Feature::RuntimeVars).unwrap(), false);
        assert_eq!(session.gate(Feature::RuntimeVars).unwrap(), false);

        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 1, "banner must print once: {messages:?}");
        assert!(messages[0].contains("0.27.0"), "got: {messages:?}");
    }

    #[test]
    fn runtime_vars_is_available_on_a_current_agent() {
        let info = VersionInfo {
            version: "0.27.0".into(),
            api: "v1".into(),
            features: Some(Feature::advertised()),
        };
        let session = CompatSession::with_sink("0.27.0", &info, Box::new(|_| {}));
        assert!(session.gate(Feature::RuntimeVars).unwrap());
    }
```

If `VersionInfo` has more fields than the three above, copy the construction from an existing test in the same module rather than guessing.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pi-bin compat 2>&1 | tail -20`
Expected: FAIL — `Feature::RuntimeVars` does not exist.

- [ ] **Step 3: Add the variant**

In `crates/bin/src/compat.rs`, add `RuntimeVars` to the `Feature` enum and to `Feature::ALL`, then one arm to each of the four match blocks:

```rust
            Feature::RuntimeVars => "runtime-vars",      // capability()
            Feature::RuntimeVars => "runtime variables", // label()
            Feature::RuntimeVars => Policy::Degradable,  // policy()
            Feature::RuntimeVars => "0.27.0",            // since()
```

Delete the `#[allow(dead_code)]` and the "no current feature declares Degradable yet" comment above `Policy::Degradable` — it now has a consumer.

Do **not** add `runtime-vars` to `LEGACY_MATRIX`: that table is frozen and only describes agents that predate the handshake.

- [ ] **Step 4: Gate the deploy path**

In `crates/bin/src/cli/commands.rs`'s `deploy`, next to the existing `compat.gate(...)` calls, add:

```rust
    // Degradable: an old agent still deploys, it just injects no RPI_*.
    // A hard gate would break working deploys over a dependency the
    // configuration can no longer even express, since RPI_* are runtime-only.
    let _ = compat.gate(crate::compat::Feature::RuntimeVars)?;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p pi-bin compat 2>&1 | tail -20` → PASS.
Run: `cargo test --locked 2>&1 | tail -20` → PASS.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/src/compat.rs crates/bin/src/cli/commands.rs
git commit -m "feat(compat): advertise runtime-vars as the first Degradable feature"
```

---

### Task 12: End-to-end coverage through the real binary

**Files:**
- Create: `crates/bin/tests/config_vars.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

**Scope note — read this before starting.** This repository has no docker-based
end-to-end harness: `crates/bin/tests/success_stream.rs` is 42 lines that run
`CARGO_BIN_EXE_rpi` in a temporary directory and assert on stdout/stderr. So
this task covers everything reachable without an agent — the entire resolver,
every error message, and the `[runtime]` preview — by driving the real binary.
The container-injection half (a running container actually seeing `RPI_*`) has
**no automated coverage** and is verified manually; the checklist is in Step 3.
Do not invent an agent harness here.

- [ ] **Step 1: Write the tests**

Create `crates/bin/tests/config_vars.rs`:

```rust
//! End-to-end coverage of the configuration variable system, driven through
//! the real `rpi` binary. Everything here is local-only: `rpi config show`
//! never contacts an agent, and `rpi env destroy --key` validates the key
//! before it tries to connect.

use std::path::Path;
use std::process::{Command, Output};

const BASE: &str = r#"schema = 1

[project]
name = "myapp"

[source]
repo = "git@github.com:acme/myapp.git"
branch = "main"

[ingress]
hostname = "app.example.com"
service = "web"
port = 3000
"#;

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

fn rpi(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rpi"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn stdout_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    assert!(!out.status.success(), "command unexpectedly succeeded");
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn parameterized_overlay_resolves_key_branch_hostname_and_runtime_block() {
    let dir = project(&[
        ("rpi.toml", BASE),
        (
            "rpi.branch.toml",
            "[source]\nbranch = \"${BRANCH_NAME}\"\n\n[ingress]\nhostname = \"${env.slug}.preview.example.com\"\n",
        ),
    ]);
    let out = stdout_of(&rpi(
        dir.path(),
        &[
            "config",
            "show",
            "--env",
            "branch",
            "--vars",
            "BRANCH_NAME=feature/login",
        ],
    ));

    assert!(out.contains("name = \"myapp--branch--feature-login\""), "{out}");
    assert!(out.contains("branch = \"feature/login\""), "{out}");
    assert!(
        out.contains("hostname = \"feature-login.preview.example.com\""),
        "{out}"
    );
    assert!(out.contains("[runtime]"), "{out}");
    assert!(out.contains("RPI_BRANCH_NAME = \"feature/login\""), "{out}");
    assert!(out.contains("RPI_ENV = \"branch\""), "{out}");
    assert!(out.contains("RPI_ENV_SLUG = \"feature-login\""), "{out}");
    assert!(out.contains("RPI_HOST_PORT = \"<assigned by agent>\""), "{out}");
}

#[test]
fn vars_work_in_the_base_file_without_an_env() {
    let dir = project(&[(
        "rpi.toml",
        &BASE.replace("branch = \"main\"", "branch = \"${BRANCH_NAME}\""),
    )]);
    let out = stdout_of(&rpi(
        dir.path(),
        &["config", "show", "--vars", "BRANCH_NAME=develop"],
    ));
    assert!(out.contains("branch = \"develop\""), "{out}");
    assert!(out.contains("name = \"myapp\""), "no env, no derived key: {out}");
}

#[test]
fn substitution_reaches_commands_and_the_escape_survives() {
    let dir = project(&[(
        "rpi.toml",
        &format!("{BASE}\n[commands]\nseed = \"node seed.js --env ${{STAGE}}\"\nbackup = \"sh -c 'tar -C $${{HOME}} .'\"\n"),
    )]);
    let out = stdout_of(&rpi(dir.path(), &["config", "show", "--vars", "STAGE=qa"]));
    assert!(out.contains("node seed.js --env qa"), "{out}");
    assert!(
        out.contains("tar -C ${HOME} ."),
        "$${{ must render as a literal ${{: {out}"
    );
}

#[test]
fn an_unreferenced_variable_is_reported_by_name() {
    let dir = project(&[("rpi.toml", BASE)]);
    let err = stderr_of(&rpi(dir.path(), &["config", "show", "--vars", "TYPO=1"]));
    assert!(err.contains("TYPO"), "{err}");
    assert!(err.contains("never referenced"), "{err}");
}

#[test]
fn an_rpi_reference_in_toml_points_at_the_replacement() {
    let dir = project(&[
        ("rpi.toml", BASE),
        (
            "rpi.branch.toml",
            "[ingress]\nhostname = \"${RPI_ENV_SLUG}.preview.example.com\"\n",
        ),
    ]);
    let err = stderr_of(&rpi(dir.path(), &["config", "show", "--env", "branch"]));
    assert!(err.contains("runtime"), "{err}");
    assert!(err.contains("${env.slug}"), "{err}");
}

#[test]
fn interpolating_the_project_name_is_refused() {
    let dir = project(&[(
        "rpi.toml",
        &BASE.replace("name = \"myapp\"", "name = \"app-${STAGE}\""),
    )]);
    let err = stderr_of(&rpi(dir.path(), &["config", "show", "--vars", "STAGE=qa"]));
    assert!(err.contains("[project].name"), "{err}");
}

#[test]
fn git_branch_resolves_from_the_surrounding_repository() {
    let dir = project(&[(
        "rpi.toml",
        &BASE.replace("branch = \"main\"", "branch = \"${git.branch}\""),
    )]);
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--quiet", "--initial-branch", "release"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "T"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "init"]);

    let out = stdout_of(&rpi(dir.path(), &["config", "show"]));
    assert!(out.contains("branch = \"release\""), "{out}");
    assert!(out.contains("RPI_BRANCH_NAME = \"release\""), "{out}");
}

#[test]
fn git_branch_outside_a_repository_says_so() {
    let dir = project(&[(
        "rpi.toml",
        &BASE.replace("branch = \"main\"", "branch = \"${git.branch}\""),
    )]);
    let err = stderr_of(&rpi(dir.path(), &["config", "show"]));
    assert!(err.contains("not a git repository"), "{err}");
}

#[test]
fn a_malformed_key_is_refused_before_any_connection_attempt() {
    let dir = project(&[("rpi.toml", BASE)]);
    let err = stderr_of(&rpi(
        dir.path(),
        &["env", "destroy", "--key", "not-a-key", "--yes"],
    ));
    assert!(err.contains("--key"), "{err}");
    assert!(
        !err.contains("ssh") && !err.contains("connect"),
        "validation must precede any connection: {err}"
    );
}
```

- [ ] **Step 2: Run them**

Run: `cargo test --locked --test config_vars 2>&1 | tail -30`
Expected: PASS, 9 tests.

`git_branch_resolves_from_the_surrounding_repository` needs `git` on `PATH`, which the repository's own test suite already assumes elsewhere.

- [ ] **Step 3: Verify container injection by hand and record the result**

No automated coverage exists for this half. Run it against a real agent once, and paste the outcome into the pull-request description:

```bash
# In a project whose docker-compose.yml has a public `web` and a `worker`,
# where web's compose entry sets:  environment: { SEEN: "${RPI_BRANCH_NAME}" }
rpi deploy --env branch --vars BRANCH_NAME=feature/login

rpi command --env branch --vars BRANCH_NAME=feature/login -- printenv RPI_BRANCH_NAME
#   expect: feature/login
rpi command --env branch --vars BRANCH_NAME=feature/login -- printenv RPI_ENV_SLUG
#   expect: feature-login
rpi command --env branch --vars BRANCH_NAME=feature/login -- printenv RPI_COMMIT_SHA
#   expect: the sha the deploy reported
rpi command --env branch --vars BRANCH_NAME=feature/login -- printenv SEEN
#   expect: feature/login  (proves compose-file interpolation, not just the override)

# A [commands] entry pinned to `service = "worker"` must show the same values,
# proving non-public services are covered.

# On a plain (non-environment) deploy of the same project:
rpi command -- printenv RPI_ENV
#   expect: exit code 1, no output — absent, not empty
rpi command -- printenv RPI_PROJECT
#   expect: the project name
```

- [ ] **Step 3: Verify and commit**

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git add crates/bin/tests/runtime_vars.rs
git commit -m "test(runtime): end-to-end coverage for config variables and RPI_*"
```

---

### Task 13: Documentation

**Files:**
- Modify: `docs/architecture/flows/environments.md`
- Modify: `docs/architecture/flows/deploy.md`
- Modify: `.claude/skills/rpi-toml/SKILL.md`, `.claude/skills/rpi-cli/SKILL.md`

Read the `architecture-diagrams` skill first for this repository's conventions (Source anchors, walkthrough numbering, the Mermaid style).

- [ ] **Step 1: Update `flows/environments.md`**

- Sequence diagram: replace `interpolate ${BRANCH_NAME}/${RPI_ENV_SLUG} (source.branch, ingress.hostname only)` with the two-phase description: `phase 1: source.branch; derive env.slug; phase 2: every other field`.
- Walkthrough step 2: rewrite for the three namespaces, the `$${` escape, the `[project].name` ban, the unreferenced-variable error, and the detached-`HEAD` rule.
- Walkthrough step 4: the slug suffix now depends on `${env.slug}` being referenced, not on any variable being used.
- Walkthrough step 13: `destroy`/`reset-data` now resolve the overlay on the `<env>` path, and `--key` is the no-config-file escape hatch. Correct the old claim that they read only `./rpi.toml`.
- Source anchors: add `crates/bin/src/cli/vars.rs`, `crates/bin/src/cli/gitctx.rs`, `crates/domain/src/runtimevars.rs`; update the `overlay.rs` and `envcmds.rs` entries.

- [ ] **Step 2: Update `flows/deploy.md`**

Insert the service-discovery and override-write steps between secrets injection and build, note that the override now carries `environment` for every service, and add `crates/infrastructure/src/overrides.rs` plus the `services` method of `docker.rs` to the Source anchors.

- [ ] **Step 3: Update the two skills**

- `rpi-toml`: document the three namespaces, where substitution is allowed and where it is not, `$${` escaping, and the `${env.slug}` rule for the key suffix. Remove every mention of `${RPI_ENV_SLUG}` in a TOML file.
- `rpi-cli`: `--vars` no longer requires `--env` and takes arbitrary names; add `rpi env destroy --key` / `reset-data --key`; document the `[runtime]` block of `rpi config show`.

- [ ] **Step 4: Verify and commit**

```bash
git add docs .claude/skills
git commit -m "docs: config variables and runtime RPI_* namespace"
```

---

## Self-Review

**Spec coverage.** Every spec section maps to a task: variable model → 1; resolver inputs and detached `HEAD` → 2; substitution scope, escaping, pipeline, removed and added rules, key derivation → 3; `--key` → 4; secrets guard and `[runtime]` → 5; catalog, no-protocol-change and registry → 6; process env, exec and discovery → 7; override emission → 8; pipeline order → 9; the non-deploy paths → 10; old-agent behavior → 11; testing → 12; documentation → 13.

**Two spec items deliberately not given their own task.** The `${RPI_ENV_SLUG}` hard break is implemented by Task 1's `classify` and asserted in Tasks 1 and 3. The silent-key-change warning is implemented and asserted in Task 3.

**One deviation from the spec, applied consistently.** The spec says the override emits `environment` for every service; Task 8 skips services that would carry an empty map, so a project with no runtime variables gets the same single-service file it gets today. This only narrows the output and is asserted by `empty_environment_emits_no_environment_key`.

**Sequencing.** Tasks 6, 7 and 8 each change a signature that Task 9 consumes; each keeps the workspace green by passing a behavior-neutral value at the call site, which Task 9 replaces. Task 1 through 5 are CLI-only and independent of 6 through 11.

**Baseline.** `cargo test --locked` in this worktree at the spec commit: 714 passed across 8 suites. Any task that ends below that number has broken something.

**Three corrections made during review, worth knowing about while implementing:**

1. `crates/application/src/secrets.rs` also writes the override (the `--apply`
   path). Left at Task 8's placeholder arguments, `rpi secrets send --apply`
   would rewrite the override *without* the environment blocks the last deploy
   put there, silently stripping `RPI_*` from every container until the next
   deploy. Task 10 gives that site the real values.
2. `DeployProject` calls `OverrideStore::path` for the first time in Task 9, so
   every deploy test needs a new `expect_path` — mockall panics on an
   unexpected call rather than returning a default.
3. The container-injection half has no automated coverage, because this
   repository has no docker-based test harness — `success_stream.rs` is 42
   lines driving the CLI binary in a tempdir. Task 12 covers everything
   reachable without an agent and specifies a manual checklist for the rest.
   Building an agent-plus-docker harness is worth doing, but it is its own
   piece of work, not a step inside this one.

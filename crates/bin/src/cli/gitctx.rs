//! Resolver inputs read from the local git repository (`${git.*}`).
//!
//! Every function shells out to `git` in an explicit directory rather than
//! the process working directory, so tests run against a throwaway
//! repository instead of whichever checkout happens to be current.
//!
//! These are computed lazily by the resolver — only when a configuration
//! actually references one — so `rpi config show` keeps working outside a
//! git repository for configurations that use no `${git.*}` variable.
//!
//! Nothing calls these yet; Task 3 wires them into the config resolver,
//! which is when this allow becomes unnecessary.
#![allow(dead_code)]

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
    anyhow::bail!("${{git.branch}}: HEAD is detached; pass the branch explicitly via --vars")
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

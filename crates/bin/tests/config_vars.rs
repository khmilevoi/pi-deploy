//! End-to-end coverage of the configuration variable system, driven through
//! the real `rpi` binary. Everything here is local-only: `rpi config show`
//! never contacts an agent, and `rpi env destroy --full-key` validates the
//! key before it tries to connect.

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

    assert!(
        out.contains("name = \"myapp--branch--feature-login\""),
        "{out}"
    );
    assert!(out.contains("branch = \"feature/login\""), "{out}");
    assert!(
        out.contains("hostname = \"feature-login.preview.example.com\""),
        "{out}"
    );
    assert!(out.contains("[runtime]"), "{out}");
    assert!(out.contains("RPI_BRANCH_NAME = \"feature/login\""), "{out}");
    assert!(out.contains("RPI_ENV = \"branch\""), "{out}");
    assert!(out.contains("RPI_ENV_SLUG = \"feature-login\""), "{out}");
    assert!(
        out.contains("RPI_HOST_PORT = \"<assigned by agent>\""),
        "{out}"
    );
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
    assert!(
        out.contains("name = \"myapp\""),
        "no env, no derived key: {out}"
    );
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
        &["env", "destroy", "--full-key", "not-a-key", "--yes"],
    ));
    assert!(err.contains("--full-key"), "{err}");
    assert!(
        !err.contains("ssh") && !err.contains("connect"),
        "validation must precede any connection: {err}"
    );
}

use crate::cli::config::ConnectOpts;
use crate::cli::connect::AgentConn;
use crate::cli::overlay;
use crate::output;

/// One part of a derived key (`base`, `env`, or `slug`): lowercase, no `--`,
/// no leading or trailing `-`. Mirrors the agent's own `is_valid_env_part`.
fn is_valid_key_part(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

/// Appends the two escape hatches to a `<env>` form that failed to resolve
/// locally. Mirrors `base_filter`'s hint for `rpi env ls`: the resolution
/// failure keeps its own message (a deleted overlay, a `${NAME}` this
/// invocation was not given a `--vars` for), and the reader is told how to
/// reach the environment anyway — otherwise a project directory that no
/// longer resolves is a dead end, and `--full-key` exists precisely for it.
fn with_escape_hatches(e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "{e}\n  hint: pass the variables it needs with `--vars KEY=VALUE`, or name the environment outright with `--full-key <key>` (from `rpi env ls`), which reads no configuration file"
    )
}

/// The environment key `destroy`/`reset-data` should act on.
///
/// `--full-key` names it outright and reads no configuration file at all —
/// not the overlay, and not `rpi.toml` either. It is the escape hatch for a
/// project directory that no longer resolves, so depending on any local file
/// would defeat it. `rpi env ls` prints the exact string to pass.
///
/// The `<env>` form resolves the overlay the same way `rpi deploy` does,
/// because the slug now derives from the merged `source.branch`.
fn target_key(env: Option<String>, key: Option<String>, vars: &[String]) -> anyhow::Result<String> {
    match (env, key) {
        (Some(_), Some(_)) => {
            anyhow::bail!("--full-key and <env> are mutually exclusive: pass one or the other")
        }
        (None, None) => anyhow::bail!("pass an environment name, or --full-key <key>"),
        (None, Some(key)) => {
            let parts: Vec<&str> = key.split("--").collect();
            let shaped = matches!(parts.len(), 2 | 3) && parts.iter().all(|p| is_valid_key_part(p));
            if !shaped {
                anyhow::bail!(
                    "--full-key '{key}' is not an environment key (expected base--env or base--env--slug); run `rpi env ls` to see the exact key"
                );
            }
            Ok(key)
        }
        (Some(env), None) => {
            let resolved = overlay::resolve(Some(&env), vars).map_err(with_escape_hatches)?;
            Ok(resolved.rpitoml.project.name)
        }
    }
}

fn confirm_key(action: &str, key: &str, yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    output::warn(format!("this will {action} environment '{key}'"));
    eprint!("type the environment key to confirm: ");
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if input.trim() != key {
        anyhow::bail!("confirmation failed: expected '{key}'");
    }
    Ok(())
}

/// The `base` the agent filters the listing by: `None` for `--all` (every
/// environment on the agent), otherwise this project's name resolved out of
/// `./rpi.toml` — `base_text`, or `None` when there is no readable one here.
///
/// Two distinct failures, never folded into one message: "no rpi.toml here"
/// keeps its friendly `--all` hint, and any other resolution failure keeps
/// its own message (a malformed file, or — since variables reach the base
/// file too — a `${NAME}` this invocation was not given a `--vars` for) with
/// the escape hatches appended. `--all` reads no configuration file at all,
/// so it is the way out of a base file that cannot be resolved here.
fn base_filter(
    all: bool,
    base_text: Option<&str>,
    vars: &[String],
) -> anyhow::Result<Option<String>> {
    if all {
        return Ok(None);
    }
    let Some(text) = base_text else {
        anyhow::bail!("no rpi.toml in the current directory - use `rpi env ls --all`");
    };
    let resolved = overlay::resolve_from(text, None, vars).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  hint: pass it with `rpi env ls --vars KEY=VALUE`, or run `rpi env ls --all`, which reads no configuration file"
        )
    })?;
    Ok(Some(resolved.rpitoml.project.name))
}

pub async fn env_ls(all: bool, vars: Vec<String>, connect: ConnectOpts) -> anyhow::Result<()> {
    // Read here rather than inside `base_filter` so the resolution itself
    // stays a pure function of (text, vars) and is unit-testable.
    let base_text = if all {
        None
    } else {
        std::fs::read_to_string("rpi.toml").ok()
    };
    let base = base_filter(all, base_text.as_deref(), &vars)?;
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Environments)?;
    let envs = api.list_environments(base.as_deref()).await?;
    if envs.is_empty() {
        output::info("no environments registered");
        return Ok(());
    }
    let mut table = output::table();
    table.set_header(output::header([
        "KEY",
        "BASE",
        "ENV",
        "SLUG",
        "LAST DEPLOY",
        "TTL",
    ]));
    for e in envs {
        table.add_row(vec![
            output::cell(e.key),
            output::cell(e.base),
            output::cell(e.env),
            output::cell(e.slug.unwrap_or_else(|| "-".into())),
            output::cell(
                e.last_success_at
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
            ),
            output::cell(
                e.ttl_secs
                    .map(|t| format!("{t}s"))
                    .unwrap_or_else(|| "-".into()),
            ),
        ]);
    }
    println!("{table}");
    Ok(())
}

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
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Environments)?;
    let resp = api.destroy_environment(&key).await?;
    if resp.already_absent {
        output::info(format!(
            "environment '{key}' does not exist - nothing to destroy"
        ));
    } else {
        output::success(format!("environment '{key}' destroyed"));
    }
    Ok(())
}

pub async fn env_reset_data(
    env: Option<String>,
    key: Option<String>,
    vars: Vec<String>,
    yes: bool,
    connect: ConnectOpts,
) -> anyhow::Result<()> {
    let key = target_key(env, key, &vars)?;
    confirm_key("REMOVE ALL DATA (volumes) of", &key, yes)?;
    let AgentConn {
        tunnel: _tunnel,
        api,
        compat,
    } = crate::cli::connect::connect_agent(connect).await?;
    compat.gate(crate::compat::Feature::Environments)?;
    api.reset_environment(&key).await?;
    output::success(format!(
        "environment '{key}' data removed - the next deploy of it re-runs on_create"
    ));
    Ok(())
}

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
            "myapp",                    // no env part at all
            "myapp--",                  // empty env part
            "--test",                   // empty base
            "myapp--Test",              // uppercase
            "myapp--test--slug--extra", // too many parts
            "myapp--test--",            // empty slug
            "myapp---test",             // leading '-' in the env part
        ] {
            let err = target_key(None, Some(bad.into()), &[])
                .unwrap_err()
                .to_string();
            assert!(err.contains("--full-key"), "{bad}: {err}");
        }
    }

    #[test]
    fn key_and_env_are_mutually_exclusive_and_one_is_required() {
        let err = target_key(Some("test".into()), Some("myapp--test".into()), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--full-key"), "got: {err}");

        let err = target_key(None, None, &[]).unwrap_err().to_string();
        assert!(err.contains("--full-key"), "got: {err}");
    }

    /// A base file that needs a user variable — what README recommends and
    /// what `rpi env ls` used to be unable to read at all.
    const BASE_WITH_VAR: &str = r#"
schema = 1

[project]
name = "myapp"

[source]
repo = "git@github.com:acme/myapp.git"
branch = "${BRANCH_NAME}"

[ingress]
service = "web"
port = 3000
"#;

    #[test]
    fn all_needs_no_configuration_file_at_all() {
        assert_eq!(base_filter(true, None, &[]).unwrap(), None);
        let err = base_filter(false, None, &[]).unwrap_err().to_string();
        assert!(err.contains("--all"), "got: {err}");
    }

    #[test]
    fn the_base_name_resolves_once_the_variable_is_passed() {
        assert_eq!(
            base_filter(
                false,
                Some(BASE_WITH_VAR),
                &["BRANCH_NAME=feature/login".into()]
            )
            .unwrap(),
            Some("myapp".to_string()),
            "`rpi env ls --vars` must reach the base file's own variables"
        );
    }

    #[test]
    fn an_unresolvable_base_names_both_escape_hatches() {
        let err = base_filter(false, Some(BASE_WITH_VAR), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("BRANCH_NAME"), "got: {err}");
        assert!(err.contains("--vars"), "got: {err}");
        assert!(err.contains("rpi env ls --all"), "got: {err}");
    }

    #[test]
    fn an_unresolvable_env_form_names_both_escape_hatches() {
        // `rpi env destroy branch` in a directory whose rpi.branch.toml was
        // deleted: the key cannot be derived any more, and the message has to
        // point at --full-key or the user is stuck with an environment they
        // cannot tear down.
        let err = with_escape_hatches(anyhow::anyhow!(
            "cannot read rpi.branch.toml: No such file or directory (os error 2)"
        ))
        .to_string();
        assert!(err.contains("rpi.branch.toml"), "got: {err}");
        assert!(err.contains("--vars"), "got: {err}");
        assert!(err.contains("--full-key"), "got: {err}");
    }

    #[test]
    fn key_path_ignores_vars_entirely() {
        // --full-key exists for a directory that no longer resolves, so it
        // must not consult --vars, rpi.toml, or the overlay.
        assert_eq!(
            target_key(None, Some("myapp--test".into()), &["TYPO=1".into()]).unwrap(),
            "myapp--test"
        );
    }
}

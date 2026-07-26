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
            let resolved = overlay::resolve(Some(&env), vars)?;
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

pub async fn env_ls(all: bool, connect: ConnectOpts) -> anyhow::Result<()> {
    // Distinguish "no rpi.toml here" (friendly hint to use --all) from any
    // other resolution failure (e.g. a malformed rpi.toml), which must
    // propagate instead of being swallowed into the same generic message.
    let base = if all {
        None
    } else if !std::path::Path::new("rpi.toml").exists() {
        anyhow::bail!("no rpi.toml in the current directory - use `rpi env ls --all`")
    } else {
        Some(
            crate::cli::overlay::resolve(None, &[])?
                .rpitoml
                .project
                .name,
        )
    };
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

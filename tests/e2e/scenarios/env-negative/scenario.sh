#!/usr/bin/env bash
set -euo pipefail

# Environment-overlays spec, negative paths. Local overlay resolution must
# fail before any agent contact for: a missing overlay file (with a hint
# listing the overlays that do exist), an invalid or reserved --env name,
# a --vars key nothing references (with and without --env), an
# on_create command absent from the merged [commands], an overlay silently
# inheriting the base [ingress].hostname (production-route hijack), and a
# base rpi.toml whose project name contains the reserved '--'. Agent-side:
# `rpi env reset-data` of a nonexistent environment is a genuine error
# (unlike destroy, which is idempotent), and a deploy whose on_create exits
# nonzero fails at the on_create stage while keeping on_create_done false —
# the registered key survives and the next deploy retries the command.

source /opt/e2e/lib.sh
e2e_bootstrap

# --- Local resolution failures (`rpi config show` never contacts the agent) ---

expect_fail missing-overlay.log rpi config show --env missing
assert_log missing-overlay.log 'cannot read rpi.missing.toml'
assert_log missing-overlay.log 'found overlays: rpi.badcmd.toml, rpi.fail.toml'

expect_fail bad-env-name.log rpi config show --env BAD_NAME
assert_log bad-env-name.log "environment name 'BAD_NAME' must match"

expect_fail reserved-env-name.log rpi config show --env destroy
assert_log reserved-env-name.log "environment name 'destroy' is reserved"

# --vars no longer requires --env; what is still an error either way is a
# key no field references, named so a typo cannot pass silently.
expect_fail vars-unreferenced.log rpi config show --vars BRANCH_NAME=main
assert_log vars-unreferenced.log "variable 'BRANCH_NAME' is never referenced in rpi.toml"

expect_fail vars-unreferenced-env.log rpi config show --env fail --vars BRANCH_NAME=main
assert_log vars-unreferenced-env.log 'never referenced in rpi.toml or rpi.fail.toml'

expect_fail on-create-undeclared.log rpi config show --env badcmd
assert_log on-create-undeclared.log "on_create: command 'ghost' is not declared"

# Hostname hijack: an overlay that inherits the base hostname unchanged must
# be rejected at resolve time (crafted pair in a temp dir; config show is
# local, so the fixture never has to be deployable).
hijack_dir=$(mktemp -d)
cat >"$hijack_dir/rpi.toml" <<'EOF'
schema = 1

[project]
name = "hijack-fixture"

[source]
repo = "git://git-fixture/fixture.git"
branch = "main"

[build]
compose = "compose.yaml"

[ingress]
service = "web"
port = 8080
hostname = "app.example.invalid"

[healthcheck]
path = "/health"
expect = "200"
timeout = "30s"
EOF
cat >"$hijack_dir/rpi.prod.toml" <<'EOF'
# Deliberately empty: inherits everything from the base file, including,
# illegally, its [ingress].hostname.
EOF
(cd "$hijack_dir" && expect_fail hostname-hijack.log rpi config show --env prod)
assert_log hostname-hijack.log "equals the base hostname 'app.example.invalid'"

# '--' in a base project name is reserved for derived environment keys.
dd_dir=$(mktemp -d)
sed 's/^name = .*/name = "bad--name"/' rpi.toml >"$dd_dir/rpi.toml"
(cd "$dd_dir" && expect_fail double-dash-name.log rpi config show)
assert_log double-dash-name.log "must not contain '--' (reserved for environment keys"

# --- Agent-side failures ---

# reset-data of an environment that was never deployed: a genuine error, in
# contrast to `env destroy`, which is idempotent (covered by env-overlay).
expect_fail reset-nosuch.log rpi env reset-data nosuch --yes "${CONNECT[@]}"
assert_log reset-nosuch.log 'environment e2e-fixture--nosuch'

# on_create exits 1: the deploy passes health, then fails at the on_create
# stage; the key stays registered with on_create_done still false.
expect_fail deploy-fail.log rpi deploy --env fail "${CONNECT[@]}"
assert_log deploy-fail.log 'healthcheck: passed'
assert_log deploy-fail.log "on_create 'boom' exited with code 1"

run_capture env-ls.log rpi env ls "${CONNECT[@]}"
assert_log env-ls.log 'e2e-fixture--fail'

# Fix the command locally and redeploy the same key: on_create must run again
# (the flag never flipped) and complete this time.
sed -i 's/^boom = .*/boom = "true"/' rpi.toml
run_capture deploy-fixed.log rpi deploy --env fail "${CONNECT[@]}"
assert_deploy_log deploy-fixed.log
assert_log deploy-fixed.log "on_create 'boom' completed"

echo 'rpi e2e: PASS'

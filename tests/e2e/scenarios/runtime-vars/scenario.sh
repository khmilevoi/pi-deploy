#!/usr/bin/env bash
set -euo pipefail

# Runtime-variables spec: a deploy exports an RPI_* map derived from registry
# state, and delivers it three ways -- as the environment of every
# `docker compose` invocation that reads the project's compose file (so
# `${RPI_*}` interpolates there), as an `environment:` block per service in the
# generated override, and as `-e KEY=VALUE` on `docker compose exec`. Unit
# tests already pin the map and the three call sites; what only a running
# container can settle is whether the values actually arrive, and whether a
# variable with no value is *omitted* rather than set to an empty string --
# the distinction that makes `${RPI_ENV:-prod}` work inside a container.
#
# So this walks a plain deploy (RPI_ENV must not exist at all) and a
# parameterized environment deploy of the same project (`--env branch --vars
# BRANCH_NAME=feature/login`, key e2e-fixture--branch--feature-login), reading
# every value back out of the containers themselves.

source /opt/e2e/lib.sh
e2e_bootstrap

# assert_log is a substring match, and the two project keys here are prefixes
# of one another (e2e-fixture / e2e-fixture--branch--feature-login), so the
# identity checks need a whole-line match to mean anything.
assert_line() {
  grep -Fxq -- "$2" "$ARTIFACTS/$1" || fail "$1 has no line exactly: $2"
}

deny_log() {
  if grep -Fq -- "$2" "$ARTIFACTS/$1"; then
    fail "$1 unexpectedly contains: $2"
  fi
}

VARS=(--env branch --vars BRANCH_NAME=feature/login)

run_capture deploy-base.log rpi deploy "${CONNECT[@]}"
assert_deploy_log deploy-base.log

# Each deploy's own fetch line is the only trustworthy expectation for
# RPI_COMMIT_SHA: asserting "non-empty" would pass on a stale sha, and
# asserting a literal would pin the fixture's history.
base_sha=$(awk '/^fetched /{print $2; exit}' "$ARTIFACTS/deploy-base.log")
[[ -n $base_sha ]] || fail 'deploy-base.log has no "fetched <sha>" line'

# Plain deploy, no environment: the three variables an environment would add
# have no value, so they must be missing from the container's environment
# outright. SEEN comes from the project's own compose.yaml, so it is already
# proof of the process-environment path before any environment exists.
run_capture base-vars.log rpi command web-vars "${CONNECT[@]}"
assert_line base-vars.log 'RPI_PROJECT=e2e-fixture'
assert_line base-vars.log 'RPI_PROJECT_BASE=e2e-fixture'
assert_line base-vars.log 'RPI_BRANCH_NAME=main'
assert_line base-vars.log "RPI_COMMIT_SHA=$base_sha"
assert_line base-vars.log 'SEEN=branch=main'
deny_log base-vars.log 'RPI_ENV'
deny_log base-vars.log 'RPI_HOSTNAME'

# The same, said by the exit code: `printenv RPI_ENV` fails and prints nothing
# only because the variable is absent -- RPI_ENV="" would have exited 0.
expect_fail base-env-name.log rpi command env-name "${CONNECT[@]}"
assert_log base-env-name.log "command 'env-name' exited with code 1"
deny_log base-env-name.log 'RPI_ENV='

run_capture deploy-env.log rpi deploy "${VARS[@]}" "${CONNECT[@]}"
assert_deploy_log deploy-env.log

sha=$(awk '/^fetched /{print $2; exit}' "$ARTIFACTS/deploy-env.log")
[[ -n $sha ]] || fail 'deploy-env.log has no "fetched <sha>" line'
# The fixture gives feature/login main's tree on its own commit precisely so
# this holds; if the two branches ever collapse onto one sha, every
# RPI_COMMIT_SHA assertion below goes blind and this says so out loud.
[[ $sha != "$base_sha" ]] || fail "both deploys fetched $sha - RPI_COMMIT_SHA proves nothing"

run_capture ls.log rpi ls "${CONNECT[@]}"
assert_log ls.log 'e2e-fixture--branch--feature-login'

# Ingress service. Every variable the environment adds is here, and SEEN now
# reads feature/login: the project's compose file was interpolated from the
# environment rpi put on the `docker compose up` process, which is the one
# delivery path the generated override cannot account for.
run_capture env-web-vars.log rpi command web-vars "${VARS[@]}" "${CONNECT[@]}"
assert_line env-web-vars.log 'RPI_PROJECT=e2e-fixture--branch--feature-login'
assert_line env-web-vars.log 'RPI_PROJECT_BASE=e2e-fixture'
assert_line env-web-vars.log 'RPI_ENV=branch'
assert_line env-web-vars.log 'RPI_ENV_SLUG=feature-login'
assert_line env-web-vars.log 'RPI_BRANCH_NAME=feature/login'
assert_line env-web-vars.log 'RPI_HOSTNAME=feature-login.runtime-vars.e2e.invalid'
assert_line env-web-vars.log "RPI_COMMIT_SHA=$sha"
assert_line env-web-vars.log 'SEEN=branch=feature/login'

port=$(sed -n 's/^RPI_HOST_PORT=//p' "$ARTIFACTS/env-web-vars.log")
[[ $port =~ ^[0-9]+$ ]] || fail "RPI_HOST_PORT is not a port number: '$port'"

# Non-public service. It carries no SEEN of its own, so the values below can
# only have come from the per-service `environment:` blocks of the generated
# override -- i.e. the override reaches services that are not the ingress one.
run_capture env-worker-vars.log rpi command worker-vars "${VARS[@]}" "${CONNECT[@]}"
assert_line env-worker-vars.log 'RPI_PROJECT=e2e-fixture--branch--feature-login'
assert_line env-worker-vars.log 'RPI_PROJECT_BASE=e2e-fixture'
assert_line env-worker-vars.log 'RPI_ENV=branch'
assert_line env-worker-vars.log 'RPI_ENV_SLUG=feature-login'
assert_line env-worker-vars.log 'RPI_BRANCH_NAME=feature/login'
assert_line env-worker-vars.log "RPI_COMMIT_SHA=$sha"
deny_log env-worker-vars.log 'SEEN='

# Same command, same container shape, opposite verdict to the plain deploy.
run_capture env-name.log rpi command env-name "${VARS[@]}" "${CONNECT[@]}"
assert_line env-name.log 'branch'

# The environment's whole lifecycle left the base project's own runtime
# identity alone.
run_capture base-project-name.log rpi command project-name "${CONNECT[@]}"
assert_line base-project-name.log 'e2e-fixture'

run_capture destroy.log rpi env destroy branch --vars BRANCH_NAME=feature/login --yes "${CONNECT[@]}"
assert_log destroy.log 'destroyed'

run_capture rm.log rpi rm e2e-fixture --yes "${CONNECT[@]}"
assert_log rm.log "project 'e2e-fixture' removed"

echo 'rpi e2e: PASS'

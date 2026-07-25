#!/usr/bin/env bash
set -euo pipefail

source /opt/e2e/lib.sh
e2e_bootstrap

rpi --version
run_capture deploy-1.log rpi deploy "${CONNECT[@]}"
assert_deploy_log deploy-1.log

run_capture ls-1.log rpi ls "${CONNECT[@]}"
assert_log ls-1.log 'e2e-fixture'
assert_log ls-1.log '18080'
assert_log ls-1.log 'web:running'
health=$("${SSH[@]}" curl -fsS http://127.0.0.1:18080/health)
[[ $health == 'ok' ]] || fail "unexpected first health body: $health"

# `rpi command`: the listing reports what the agent actually has deployed, and
# a run streams the container's whole output — every line, each one complete,
# with no pane window and no truncation to the terminal width.
run_capture command-list.log rpi command "${CONNECT[@]}"
assert_log command-list.log 'echo-lines'
assert_log command-list.log 'fail-lines'

run_capture command-run.log rpi command echo-lines "${CONNECT[@]}"
assert_log command-run.log 'line-1'
assert_log command-run.log 'line-30'
long=$(printf 'x%.0s' $(seq 1 200))
assert_log command-run.log "tail-$long"
assert_log command-run.log "command 'echo-lines' finished (exit 0)"

# A failing run still shows what the command printed, and the in-container exit
# code becomes the CLI's own.
expect_fail command-fail.log rpi command fail-lines "${CONNECT[@]}"
assert_log command-fail.log 'before-failure'
assert_log command-fail.log "command 'fail-lines' exited with code 7"

run_capture deploy-2.log rpi deploy "${CONNECT[@]}"
assert_deploy_log deploy-2.log

run_capture ls-2.log rpi ls "${CONNECT[@]}"
assert_log ls-2.log 'e2e-fixture'
assert_log ls-2.log '18080'
assert_log ls-2.log 'web:running'
health=$("${SSH[@]}" curl -fsS http://127.0.0.1:18080/health)
[[ $health == 'ok' ]] || fail "unexpected second health body: $health"

run_capture rm.log rpi rm e2e-fixture --yes "${CONNECT[@]}"
assert_log rm.log "project 'e2e-fixture' removed"

run_capture ls-after-rm.log rpi ls "${CONNECT[@]}"
assert_log ls-after-rm.log 'no projects deployed yet'
if "${SSH[@]}" curl -fsS http://127.0.0.1:18080/health >/dev/null 2>&1; then
  fail 'health endpoint still reachable after rpi rm'
fi
leftovers=$("${SSH[@]}" env DOCKER_HOST=tcp://127.0.0.1:2375 docker ps -aq \
  --filter label=com.docker.compose.project=e2e-fixture)
[[ -z $leftovers ]] || fail "fixture containers remain after rpi rm: $leftovers"

echo 'rpi e2e: PASS'

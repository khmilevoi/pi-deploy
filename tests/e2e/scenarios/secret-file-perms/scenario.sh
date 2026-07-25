#!/usr/bin/env bash
set -euo pipefail

# Secret-file-modes spec: a container running as a uid other than the agent's
# must be able to read a bind-mounted compose secret. rpi materializes
# [secrets].files itself, so the mode it picks is the only thing that decides
# this — compose silently ignores mode/uid/gid for file-sourced secrets
# outside Swarm.

source /opt/e2e/lib.sh
e2e_bootstrap

# Created after the git fixture was built, so this is a local secret the CLI
# uploads, not a file committed to the repository.
printf 'top-secret-value\n' > app_secret

run_capture send.log rpi secrets send "${CONNECT[@]}"
assert_log send.log 'saved 0 key(s) and 1 file(s)'

run_capture deploy.log rpi deploy "${CONNECT[@]}"
assert_deploy_log deploy.log
assert_log deploy.log 'mode 0644'

# The container reads it as uid 1000. Before the fix this failed with EACCES.
run_capture read.log rpi command read-secret "${CONNECT[@]}"
assert_log read.log 'top-secret-value'

run_capture ls.log rpi secrets ls "${CONNECT[@]}"
assert_log ls.log 'file mode: 0644'

echo 'rpi e2e: PASS'

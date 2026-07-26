#!/usr/bin/env bash
set -euo pipefail

# Secret-groups spec: a group is pushed once for the base project, and every
# branch environment that declares it gets those secrets without a second
# upload. Before groups, each new slug produced a new deploy key with an
# empty bundle, so this scenario is the regression test for the whole point
# of the feature.

source /opt/e2e/lib.sh
e2e_bootstrap

printf 'SHARED_TOKEN=group-token-value\n' > .env.shared

# One push, addressed to the base project's group.
run_capture push.log rpi secrets push --group shared "${CONNECT[@]}"
assert_log push.log "group 'e2e-fixture/shared' now at revision 1"

run_capture groups.log rpi secrets group ls "${CONNECT[@]}"
assert_log groups.log 'shared'

# Two different branches, no secrets sent for either of them.
run_capture a.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_deploy_log a.log
assert_log a.log 'groups: shared@r1, key@r0'

run_capture b.log rpi deploy --env branch --vars BRANCH_NAME=feature/two "${CONNECT[@]}"
assert_deploy_log b.log

for slug in feature-one feature-two; do
  run_capture "read-$slug.log" rpi command print-token \
    --env branch --vars "BRANCH_NAME=feature/${slug#feature-}" "${CONNECT[@]}"
  assert_log "read-$slug.log" 'group-token-value'
done

# Rotation: one push, both environments pick it up on their next deploy.
printf 'SHARED_TOKEN=rotated-token-value\n' > .env.shared
run_capture rotate.log rpi secrets push --group shared "${CONNECT[@]}"
assert_log rotate.log 'revision 2'
assert_log rotate.log '~SHARED_TOKEN'

run_capture redeploy.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_log redeploy.log 'groups: shared@r2, key@r0'
run_capture read-rotated.log rpi command print-token \
  --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_log read-rotated.log 'rotated-token-value'

# A forced push always overwrites, bumping the revision regardless of a
# concurrent change (the guard that --force bypasses is covered by the
# unforced pushes above, which is why this one goes straight to --force).
run_capture conflict.log rpi secrets push --group shared --force "${CONNECT[@]}"
assert_log conflict.log 'revision 3'

# Destroying one environment must not take the shared group with it.
run_capture destroy.log rpi env destroy branch --vars BRANCH_NAME=feature/two --yes "${CONNECT[@]}"
run_capture groups-after.log rpi secrets group ls "${CONNECT[@]}"
assert_log groups-after.log 'shared'

run_capture recreate.log rpi deploy --env branch --vars BRANCH_NAME=feature/two "${CONNECT[@]}"
assert_deploy_log recreate.log
assert_log recreate.log 'groups: shared@r3, key@r0'

# A declared group that does not exist fails loudly at the secrets stage.
run_capture rm.log rpi secrets group rm shared --force "${CONNECT[@]}"
expect_fail missing.log rpi deploy --env branch --vars BRANCH_NAME=feature/one "${CONNECT[@]}"
assert_log missing.log "secret group 'shared'"

echo 'rpi e2e: PASS'

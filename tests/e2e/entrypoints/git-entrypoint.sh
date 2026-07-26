#!/usr/bin/env bash
set -euo pipefail

SCENARIO=${RPI_E2E_SCENARIO:?RPI_E2E_SCENARIO must be set}
APP=/opt/e2e/scenarios/$SCENARIO/app

rm -rf /srv/git/fixture.git /tmp/fixture-work
mkdir -p /srv/git
cp -a "$APP" /tmp/fixture-work
cd /tmp/fixture-work
git init --initial-branch=main
git config user.name 'rpi e2e'
git config user.email 'rpi-e2e@example.invalid'
git add .
git commit -m "fixture: $SCENARIO app"

# Refs beyond `main` that a scenario needs to actually resolve, one name per
# line in its optional `branches` file (kept out of app/, so it is not part of
# the fixture tree). Each forks from the commit above and adds an empty commit:
# the tree stays identical on every branch, so a scenario can deploy
# `feature/login` without maintaining a second copy of the app, while the sha
# still differs from main's — which is what lets a scenario tell "the container
# reports this deploy's RPI_COMMIT_SHA" apart from "it reports some sha".
# Whitespace is stripped the way target-entrypoint.sh reads `agent-bin`, so a
# CRLF checkout cannot smuggle a \r into a ref name.
BRANCHES=/opt/e2e/scenarios/$SCENARIO/branches
if [[ -f $BRANCHES ]]; then
  while IFS= read -r line || [[ -n $line ]]; do
    name=$(printf '%s' "$line" | tr -d '[:space:]')
    [[ -n $name ]] || continue
    git checkout -q -b "$name" main
    git commit -q --allow-empty -m "fixture: $SCENARIO on $name"
  done <"$BRANCHES"
  git checkout -q main
fi

# A scenario that deploys a parameterized overlay against more than one real
# branch (e.g. a per-branch preview) lists the extra names here, one per
# line; each is created pointing at the same commit as main, since only the
# ref needs to differ, not the content. Absent for every other scenario, so
# this is a no-op everywhere else.
EXTRA_BRANCHES=/opt/e2e/scenarios/$SCENARIO/extra-branches
if [[ -f $EXTRA_BRANCHES ]]; then
  while IFS= read -r branch; do
    # Belt-and-braces: .gitattributes pins this file to LF, but strip a
    # trailing CR anyway in case a clone somehow still normalized it -- a
    # ref name containing \r is a hard git error, not a soft one.
    branch=${branch%$'\r'}
    [[ -z $branch ]] && continue
    git branch "$branch"
  done <"$EXTRA_BRANCHES"
fi

git clone --bare . /srv/git/fixture.git
exec git daemon --reuseaddr --verbose --export-all --base-path=/srv/git /srv/git

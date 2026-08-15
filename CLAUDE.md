# Project rules

## Before finishing any task

Run these before considering a change complete (matches what CI checks on Linux — a mismatch here is a guaranteed CI failure):

```bash
rtk cargo fmt --all -- --check
rtk cargo clippy --all-targets --locked -- -D warnings
rtk cargo test --locked
```

If `cargo fmt --all -- --check` reports a diff, run `rtk cargo fmt --all` and commit the result — do not hand-edit formatting.

If the change alters behavior or structure covered by `docs/architecture/`,
update the affected documents before finishing — see the
`architecture-diagrams` skill for the code-area→doc map and conventions.

## Scheduled security review

A scheduled routine reviews new commits on `master` and records the last
reviewed commit in `.security-review/last-reviewed-sha.txt`. When it finds no
critical/high issues it bumps that marker and commits the bump to `master` —
which means the routine's own commit becomes the next run's "new" change.

**This rule overrides the routine's stored prompt where they conflict.** Before
reviewing, list the real changes since the marker, ignoring the routine's own
bookkeeping:

```bash
git diff --name-only "$(cat .security-review/last-reviewed-sha.txt)"..origin/master -- . ':(exclude).security-review/'
```

If that prints nothing, there is no new code to review: stop, and change
nothing. Do not review, do not bump the marker, do not commit, do not push, do
not open a PR. Leaving the marker on an older SHA is correct — bumping it is
exactly what makes the routine re-trigger on its own commits forever. A run that
ends with the repository untouched is a success.

Only when that command lists real files does a review run, and only then may the
marker be bumped to the reviewed SHA.

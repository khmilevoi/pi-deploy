# Security review marker

`last-reviewed-sha.txt` holds the SHA of the last `master` commit that the
scheduled security-review routine examined.

The marker is deliberately allowed to lag behind `master`. It points at the last
commit that contained **reviewable code**, not at whatever `master` happens to
be right now.

That distinction matters because the routine commits its own marker bumps to
`master`. If a run treated "marker != HEAD" as "there is new code", every bump
would manufacture the delta that triggers the next run, and the routine would
review its own bookkeeping forever — which is what happened across
`de3df3b`, `fe8e4c7`, `326058e` and `217d819`.

So the check is "has any file outside this directory changed", not "has the SHA
moved":

```bash
git diff --name-only "$(cat .security-review/last-reviewed-sha.txt)"..origin/master -- . ':(exclude).security-review/'
```

Empty output means there is nothing to review, and the run must leave the
repository untouched. See the "Scheduled security review" section in the
repo-root `CLAUDE.md`.

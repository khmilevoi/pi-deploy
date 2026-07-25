# Migration: `rpi command --full` Removed (v0.25)

`rpi command NAME` used to render the container's output into a bordered live
pane: only the last 10 lines stayed visible, and each line was cut to the
terminal width. `--full` existed to dump the complete captured output *after*
the run had finished.

v0.25 prints every line straight to stdout as it arrives — complete, in order,
never truncated — so the flag has nothing left to do and is removed.

## What To Do

Drop `--full` from any script or CI job that passes it. It is now an unknown
argument, so a call that keeps it fails to parse:

```bash
# before
rpi command migrate --full

# after — same complete output, streamed live instead of dumped at the end
rpi command migrate
```

Nothing else changes: the command name, trailing `-- <args>`, `--env`/`--vars`,
and the exit code (still the in-container exit code) all behave as before.

## What The Output Looks Like Now

- Every line the command printed goes to **stdout**, unabridged and
  untruncated — long lines survive whole, and `rpi command migrate >
  migrate.log` captures exactly what ran.
- Only the closing verdict (`command 'migrate' finished (exit 0)`, or the
  failing exit code) goes to **stderr**, so redirecting stdout leaves a clean
  capture of the command's own output.
- Colour from the command survives; terminal-corrupting control sequences
  (cursor movement, line/screen erase) are still stripped.

`rpi deploy` is unaffected — its staged live pane is unchanged.

## Rollback

Pin the previous release if a script depends on the old rendering:

```bash
npm install -g rpi-deploy@0.24.0
```

The agent protocol is unchanged, so a v0.24 CLI works against a v0.25 agent and
vice versa (the CLI warns about the version difference, as always).

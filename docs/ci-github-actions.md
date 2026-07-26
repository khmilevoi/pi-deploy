# CI: Deploy From GitHub Actions

`rpi deploy` is CI-ready: the latest-wins queue handles two
consecutive pushes without retries, staged timeouts prevent jobs from hanging,
and the `--host/--user/--key` flags avoid requiring client config on the runner.

## This repository's Docker e2e merge gate

The `e2e` job in `.github/workflows/ci.yml` runs after the Linux format, clippy,
and unit-test job on every pull request and push to `master`. It prebuilds the
shared runtime with Docker Buildx, uses the GitHub Actions cache backend v2, and
runs `npm run test:e2e` on `ubuntu-latest` with a 30-minute job timeout.

The job intentionally has only `contents: read`, consumes no repository or
deployment secrets, and uploads `${{ runner.temp }}/rpi-e2e` only when the job
fails. The DinD service is privileged, so this job must remain on a disposable
GitHub-hosted runner; do not copy it to `self-hosted` without a separate threat
review.

The cache is an optimization, not a correctness dependency. Cache export uses
`ignore-error=true`, and a cold runner must still build and pass.

## Repository Secrets (Settings -> Secrets -> Actions)

| Secret | Description |
|---|---|
| `PI_HOST` | Pi host/IP reachable from the runner over SSH |
| `PI_USER` | Pi login user, not the `pi-agent` service user |
| `PI_SSH_KEY` | Private key; its public key must be in this user's `authorized_keys` |

## .github/workflows/deploy.yml

```yaml
name: deploy

on:
  push:
    branches: [main]

concurrency:
  group: deploy-production
  cancel-in-progress: true # saves minutes; the agent queue is still latest-wins

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      # ... project tests ...

  deploy:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install rpi CLI
        # postinstall downloads the prebuilt x86_64 binary from GitHub Releases.
        run: npm install -g rpi-deploy

      - name: Prepare SSH
        run: |
          mkdir -p ~/.ssh
          ssh-keyscan -H "${{ secrets.PI_HOST }}" >> ~/.ssh/known_hosts
          install -m 600 /dev/null ~/.ssh/deploy_key
          printf '%s\n' "${{ secrets.PI_SSH_KEY }}" > ~/.ssh/deploy_key

      - name: Deploy
        run: |
          rpi deploy \
            --ref "$GITHUB_SHA" \
            --host "${{ secrets.PI_HOST }}" \
            --user "${{ secrets.PI_USER }}" \
            --key ~/.ssh/deploy_key
```

## Why This Shape

- **`--ref "$GITHUB_SHA"`** deploys the exact tested commit, not "main at deploy time".
- **`ssh-keyscan` is required**: the SSH tunnel runs with `BatchMode=yes`, so
  without known_hosts the connection fails on host key verification.
- **Two consecutive pushes**: the first deploy can finish as `superseded`; the
  CLI exits with code 0 and the job stays green (latest wins, §8.1). Red
  statuses are `failed`, `canceled`, and `interrupted`.
- **Project secrets** are not sent from CI on every deploy: the bundle is
  already stored on the Pi (`rpi secrets push` is run manually when values change, §10).
- **A stuck build** is killed by the agent's staged timeout (`timeout: build`,
  default 30 minutes), so the job fails with a clear reason instead of the
  runner timeout.
- **CI cancellation is unnecessary**: a new push supersedes the queued deploy by
  itself; use `rpi deploy --cancel` from a workstation for manual cancellation.

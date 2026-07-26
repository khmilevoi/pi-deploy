# Deploy pipeline and queue

Running `rpi deploy` moves a project's code from its git repository to a
running, publicly reachable service on the agent host. The agent runs a fixed
sequence of stages — fetch the code, build the images, start the containers,
wait for the service to become healthy, publish its public hostname if one is
configured, and clean up old build artifacts — and streams progress back to
the CLI in real time. Only one deploy runs at a time per project; anything
else submitted while it's running waits in a one-slot queue, and if a second
request arrives while a third is still waiting, the newest request simply
replaces the waiting one ("latest wins") instead of piling up a backlog.

```mermaid
sequenceDiagram
    participant CLI as rpi deploy (CLI)
    participant API as Agent HTTP API
    participant Q as Deploy queue
    participant P as Pipeline
    participant D as Docker
    participant CF as Cloudflare

    CLI->>API: submit deploy (project, ref)
    API->>Q: enqueue (latest wins per project)
    alt no deploy running for this project
        API-->>CLI: accepted, started
        Q->>P: run now
    else a deploy is already running for this project
        API-->>CLI: accepted, queued behind the active deploy
        Note over Q: at most one pending deploy per project — a newer submit replaces it (superseded)
        Q->>P: run once the active deploy finishes
    end
    CLI->>API: open log stream (SSE)
    P->>P: fetch — clone or update checkout, inject secrets
    P->>D: list the stack's services
    D-->>P: service names (or a config error, which fails the deploy here)
    P->>P: write override — public port and bind, RPI_* variables on every service
    P->>D: build compose stack
    D-->>P: images built (or build error)
    P->>D: start containers
    D-->>P: containers up (or start error)
    P->>P: health — probe (docker healthcheck, HTTP, or TCP) until ok or timeout
    opt hostname configured in rpi.toml
        P->>CF: route — publish hostname to the stable host port
        CF-->>P: applied / skipped / error
    end
    opt environment deploy, on_create declared and not yet run
        P->>D: on_create — exec the declared command once
        D-->>P: exit code (0 required)
    end
    P->>P: gc — best-effort prune of dangling images
    P-->>API: stage events as they happen, then the final status
    API-->>CLI: SSE stream, rendered as a staged pipeline
```

```mermaid
stateDiagram-v2
    [*] --> Queued: rpi deploy
    Queued --> Running: slot is free, or the previous run for this project just finished
    Queued --> Superseded: a newer rpi deploy for the same project arrives while still queued
    Queued --> Canceled: rpi deploy --cancel
    Queued --> Interrupted: agent restarts
    Running --> Canceled: rpi deploy --cancel
    Running --> Failed: a stage errors out
    Running --> Success: every stage completes (a skipped route still counts as success)
    Running --> Interrupted: agent restarts
    Superseded --> [*]
    Canceled --> [*]
    Failed --> [*]
    Success --> [*]
    Interrupted --> [*]
```

## Walkthrough

1. `rpi deploy` reads `rpi.toml` in the current directory, connects to the
   agent, and — when the agent supports it — checks that it can reach the git
   remote before submitting anything.
2. The CLI submits the deploy request over the agent's HTTP API. The agent
   answers immediately with a deployment id and whether the deploy started
   right away or was queued behind another one. A deploy for an environment
   overlay (`rpi deploy --env`) carries an extra `environment` block that the
   agent validates — name shape, and that the target key isn't already
   registered as the other kind (base project vs. environment) — before it
   ever reaches the queue; see `flows/environments.md` for that guard and
   for the overlay resolution that produces the request in the first place.
3. Queuing is per project and latest-wins: at most one deploy waits behind
   the one that's currently running. If a second request arrives while the
   first is running, it waits. If a third request arrives while the second
   is still waiting, the third replaces the second outright — the second is
   marked superseded and never runs at all; the CLI watching it sees its
   stream end immediately with a "superseded" result. The deploy that is
   already running is never affected by newer submissions — only the one
   still waiting can be replaced.
4. Once a deploy starts running, the CLI opens a log stream and renders every
   stage as it happens: a staged pipeline where each stage shows started,
   then succeeded / failed / skipped, with timing. The agent buffers each
   deployment's recent log lines and stage events — an in-memory backlog
   while it's still running, or the tail saved with its history record once
   it's finished — and replays that backlog to any viewer who opens or
   reopens the deploy's log stream, with live events (or just the final
   status, for an already-finished deploy) following right after.
5. **Fetch** — clones the repository on the very first deploy of a project
   (generating a dedicated SSH deploy key first if the repo needs one), then
   always fetches from the remote and hard-resets the checkout to the
   requested branch or commit. Secrets are then decrypted and written into
   the checkout. The agent then derives the deploy's `RPI_*` runtime
   variables — `RPI_PROJECT` and `RPI_PROJECT_BASE`, `RPI_ENV` and
   `RPI_ENV_SLUG` for an environment deploy, `RPI_BRANCH_NAME`,
   `RPI_HOSTNAME`, `RPI_HOST_PORT`, and `RPI_COMMIT_SHA` for the commit just
   fetched — entirely from what it already knows about the project, so the
   CLI never sends them and the two sides cannot disagree about them. One
   that does not apply (no hostname configured, a plain project with no
   environment, or a stored value that is the empty string — `--vars B=` is
   legal and `branch = "${B}"` reaches the registry empty) is left out
   rather than exported empty, so a container's own `${RPI_ENV:-prod}`
   default still works. To learn which services should
   receive those variables, it asks
   Docker to list the services declared by the project's own compose file,
   then generates rpi's compose override: the fixed host port, bind address
   and restart policy for the public service, plus the `RPI_*` variables on
   every service of the stack. The same variables are also exported into the
   environment of every `docker compose` call that reads this project's
   compose file — this deploy's own service listing, build and start, and
   later `rpi command`, `rpi start`/`stop`/`restart`,
   `rpi secrets push --apply`, `rpi rm` and `rpi env destroy`/`reset-data` —
   so a `${RPI_*}` reference inside the project's own compose file resolves
   there too. (The label-only calls behind `rpi logs`, `rpi status` and
   `rpi stats` never read the compose file at all, so they need nothing.)
   `rpi command` goes one step further and repeats the map as `-e` flags on
   the exec itself, because the container it enters was started by an earlier
   deploy and still carries that deploy's values; see `flows/commands.md`.
   - *Old agent*: injecting `RPI_*` is a capability the agent advertises
     (`runtime-vars`, agent `>= 0.27.0`). It is deliberately not required —
     against an older agent the CLI prints one warning and deploys anyway,
     just with no `RPI_*` reaching any container, because the alternative is
     refusing a working deploy over something the configuration cannot even
     express (`flows/connect.md`).
   - *Failure*: a git error, or the fetch stage exceeding its timeout, fails
     the deploy right here. A compose file that cannot be parsed fails the
     service listing and therefore the deploy, before anything is built —
     silently dropping a variable would be worse than an explicit error, and
     the same unparsable file would fail `build` moments later anyway.
     Either way nothing has been built or started, so whatever was already
     running from a previous deploy is untouched.
6. **Build** — runs `docker compose build` against the fetched checkout.
   Builds across different projects share a limited, agent-wide pool of
   build slots, so the Pi never builds many projects' images at once.
   - *Failure*: a bad Dockerfile, a failed dependency fetch, or a build
     timeout fails the deploy here. Because `start` never runs, whatever was
     already running from the previous successful deploy of this project
     keeps running untouched.
7. **Start** — runs `docker compose up -d --remove-orphans` to bring up (or
   replace) the running stack, using the fetched compose file plus rpi's
   override.
   - *Failure*: a compose error or timeout fails the deploy here. Health and
     route never run; the containers are left exactly as Docker's own `up`
     left them — there is no automatic rollback.
8. **Health** — polls the public service until it passes: a Docker-declared
   healthcheck if the service has one, otherwise an HTTP GET on the
   configured health path, otherwise a plain TCP connect — up to the
   project's health timeout.
   - *Failure*: the gate times out, or Docker reports the container
     unhealthy. The deploy fails here, but the stack `start` just brought up
     is deliberately left running rather than torn down, so it stays
     reachable and debuggable instead of disappearing.
9. The agent also queries the running service count for the CLI's final
   summary (silently skipped if that query fails), and, for projects exposed
   on the LAN, logs a best-effort reachable `http://<lan-ip>:<port>` line
   (with a warning if the detected address turns out to be public rather
   than a private LAN address).
10. **Route** — only runs when `rpi.toml` declares a hostname. Publishes (or
    updates) the Cloudflare route pointing at the project's stable host
    port.
    - *Failure branches*: if ingress is disabled on the agent, the stage is
      marked **skipped**, not failed — the deploy still succeeds, and the
      CLI prints a warning telling the operator how to enable ingress. If
      the route call itself errors, the deploy **fails** here; the
      application stack is already up and healthy at this point, just not
      reachable at its public hostname.
11. **On_create** — runs only for an environment deploy (`rpi deploy --env`)
    whose overlay declared `[environment].on_create` and whose registry row
    hasn't run it yet. Right after health (and the optional route above),
    the agent execs that one declared command, once per environment key, and
    flips a persisted flag so later redeploys of the same key skip it. Full
    detail — including the flag, and `rpi env reset-data`'s way of clearing
    it — is in `flows/environments.md`.
    - *Failure*: a nonzero exit fails the deploy here; the flag stays unset,
      so the next deploy of the same key retries it.
12. **Gc** — a best-effort prune of dangling Docker images to reclaim disk
    space. A gc error, or its own timeout, is always downgraded to
    "skipped" — it never fails the deploy.
13. The agent records the final status (success, failed, or canceled) and
    closes the stream. The CLI prints a final stamp (success / superseded /
    failed) with elapsed time, service count, and hostname if any, and
    re-prints any warnings (like the ingress-disabled one) next to it so
    they can't scroll away unseen. A failed deploy makes the CLI exit
    non-zero.
14. **Canceling** (`rpi deploy --cancel`) cancels every active deployment for
    the project at once, not just one: a deploy still waiting in the queue
    is removed and marked canceled before it ever runs; a deploy that's
    already running is signaled to stop wherever it currently is (killing
    any in-flight git/docker command it was waiting on) and is then marked
    canceled. Either way, canceling never tears down or rolls back
    containers that `start` already brought up — it only stops the pipeline
    from proceeding any further. The CLI has no distinct stamp for this
    outcome, though: whichever process is still following that deploy's log
    stream sees a status other than success or superseded, so an explicit
    `--cancel` still prints the same failed-style stamp and exits non-zero,
    just like a genuine failure. A cancel that arrives for a deployment whose
    final status is already recorded is refused as a conflict ("already
    finished") rather than reported as canceling — the recorded status, not the
    queue's own bookkeeping, decides whether a deployment is still cancellable,
    because the queue only releases a deployment's slot after its run has
    returned, which is a moment later than the status write inside it. Since
    `--cancel` cancels the whole active list, one such refusal is reported
    against that deployment and the rest are still cancelled; the command exits
    non-zero at the end.
15. When a running deploy ends — success, failure, or cancellation — the
    agent immediately promotes the one deploy waiting behind it (if any) to
    running. If nothing is queued, the project goes idle until the next
    `rpi deploy`.
16. If the agent process itself restarts while a deployment is still queued
    or running, that row is not resumed: every such deployment is marked
    interrupted at startup, and the project simply waits for the next
    `rpi deploy` to start fresh.

## Source anchors

- `crates/application/src/deploy.rs` — the deploy use case: runs fetch →
  build → start → health → route → on_create (environment deploys only) →
  gc in order, emits the stage/log events the CLI renders, and records the
  final status. It also assembles the `RPI_*` variables and the service list
  for the override, between injecting secrets and building. The `on_create`
  stage itself, and the pre-queue `environment`-block guards in
  `crates/bin/src/agent/http.rs`'s `create_deployment`, are covered in full
  in `flows/environments.md`.
- `crates/domain/src/runtimevars.rs` — derives the `RPI_*` variables a
  deploy exports from the project's registry row and the commit just
  fetched; a value that does not apply is left out rather than exported
  empty.
- `crates/application/src/scheduler.rs` — the per-project deploy queue:
  starts a deploy immediately if the project is idle, otherwise queues it; a
  newer queued request supersedes the one waiting; drives cancel — checking the
  recorded status first, so a deployment that already finished is not treated as
  cancellable while its slot is still being released — and promotes the next
  queued deploy once the running one finishes.
- `crates/infrastructure/src/git.rs` — the fetch stage: clones or updates
  the checkout, generates a per-project SSH deploy key when the repo needs
  one, and checks repo access ahead of a deploy.
- `crates/infrastructure/src/repo.rs` — stores each project's configuration
  and allocates its host port on first deploy; the port stays stable across
  every later redeploy of that project.
- `crates/infrastructure/src/docker.rs` — runs `docker compose build` and
  `docker compose up -d` for the build and start stages, and its `services`
  method lists the stack's services beforehand (`compose config --services`)
  — deliberately over a file chain without rpi's own override, so a service
  left over in it from a previous deploy is not counted and then re-created
  as a phantom. Every compose invocation for a stack is spawned with that
  stack's `RPI_*` map in its process environment, which is what makes
  `${RPI_*}` interpolate inside the project's own compose file; `exec`
  additionally repeats the map as `-e` flags. (It also backs other,
  non-deploy commands not covered by this document.)
- `crates/infrastructure/src/health.rs` — the health stage: tries a
  Docker-declared healthcheck first, then an HTTP probe, then a plain TCP
  connect, polling until one passes or the timeout expires.
- `crates/infrastructure/src/probe.rs` — the agent's diagnostics (`rpi
  doctor`): checks Docker/compose availability and ingress configuration.
  Read for contrast — it does not participate in the deploy pipeline
  itself.
- `crates/infrastructure/src/hostnet.rs` — best-effort local network
  address detection, used only to log a reachable LAN URL for
  LAN-exposed projects; it does not allocate or manage ports (that's
  `repo.rs`).
- `crates/infrastructure/src/overrides.rs` — writes rpi's own compose
  override (bind address, stable host port and restart policy for the public
  service, plus the `RPI_*` variables on every discovered service) that the
  build/start stages layer on top of the project's own compose file.
  `environment` is emitted as a mapping, never a sequence, so compose merges
  it key-wise and the generated override — last in the file chain — wins over
  a same-named key in the project's own file. The public service is written
  unconditionally, because its port mapping is the one thing a deploy cannot
  work without even if discovery came back empty; a service with an empty
  variable map is left out of the file entirely rather than emitted with a
  bare `environment: {}`.
- `crates/infrastructure/src/history.rs` (`sweep_interrupted` only) — marks
  any deployment left `queued` or `running` as `interrupted` when the agent
  restarts; its other role, deployment-history retention, is covered in
  `flows/gc.md`.
- `crates/infrastructure/src/events.rs` — the in-memory backlog and live
  broadcast behind the deploy log stream: buffers each running deployment's
  recent events and replays them to a viewer that opens or reopens the
  stream mid-run.
- `crates/application/src/tail.rs` — keeps the last N lines of a
  deployment's log as its history record's tail, which is what a viewer
  gets instead once that deployment has finished.
- `crates/bin/src/cli/commands.rs` — the CLI side of `rpi deploy` and `rpi
  deploy --cancel`: resolves the local configuration, gates `runtime-vars`
  degradably (the return value is deliberately discarded — the deploy runs
  either way, the gate exists only to emit the one-shot warning), submits the
  request, follows the SSE stream, renders the staged pipeline, and prints
  the final result.
- `crates/bin/src/cli/sourcekey.rs` — the deploy-key preflight run before
  submitting a deploy for an SSH-remote repo: asks the agent whether it can
  read the repo, and on denial either auto-registers a deploy key via the
  local `gh` CLI or shows the public key with instructions and polls until
  access works.

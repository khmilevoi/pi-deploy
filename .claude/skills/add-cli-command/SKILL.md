---
name: add-cli-command
description: Use when adding a new rpi subcommand or changing an existing command's flags, output rendering, or agent API surface in this repo (crates/bin, crates/application, proto.rs, agent/http.rs).
---

# Adding an rpi CLI Subcommand

End-to-end recipe for a new `rpi <cmd>`. The chain is: clap enum → `cli/commands.rs` → `ApiClient` → HTTP `/v1/...` → axum handler → use case → domain contract. Work through the checklist in order; every REQUIRED item exists for each shipped command today.

For the *why* behind this shape — the CLI↔agent boundary, command nesting, help conventions, output split, safety gates, version skew — see [`docs/cli-philosophy.md`](../../../docs/cli-philosophy.md).

## File checklist (in order)

**Agent side (skip for local-only commands like `init`/`setup`):**

1. `crates/domain/src/contracts.rs` — new trait method only if a new infra capability is needed. Keep `#[cfg_attr(feature = "mocks", automock)]`; mockall regenerates `Mock*` automatically.
2. `crates/infrastructure/src/<area>.rs` — implement the contract. SQL follows existing queries in the same file (`row_to_*` mappers).
3. `crates/application/src/<verb>.rs` — use case struct: `Arc<dyn Contract>` fields, `pub fn new(...) -> Arc<Self>`, `pub async fn execute(...) -> Result<_, DomainError>`. **Decision rule:** a read that is a single repository call with no orchestration may skip this layer and use the `AppState` field directly in the handler (precedent: `active_deployments`); anything combining contracts, timeouts, or log sinks gets a use case.
4. `crates/bin/src/proto.rs` — DTO `#[derive(Debug, Clone, Serialize, Deserialize)]` + `impl From<DomainType>`. Reuse an existing DTO when the fields match (`DeploymentDto` covers most deployment views).
5. `crates/bin/src/agent/http.rs` — route in `router()` + handler. Handler shape: validate `is_valid_name(&name)` → `DomainError::Invalid`; call use case; wrap errors in `ApiError` (it maps Conflict→409, NotFound→404, Invalid→400). Long operations: `tokio::time::timeout` (see `run_gc`). Streaming: SSE with `event: log` / terminal event (see `run_command`).
6. **If you added a use case: wire it in BOTH `AppState` construction sites** — production in `crates/bin/src/agent/state.rs` AND the test fixture near the bottom of `agent/http.rs` (`state_with...`). Missing the second one fails only at test-compile time.

**CLI side:**

7. `crates/bin/src/cli/api.rs` — `ApiClient` method: `extract_error(resp).await?.json().await?`. **New route ⇒ old-agent compat:** a bare 404 (no `{"error"}` JSON body) means the agent predates the feature — bail with `"agent does not support X; update the agent on the Pi"` (pattern: `commands_not_found`, `extract_secrets_error`).
8. `crates/bin/src/cli/commands.rs` — `pub async fn <name>(..., connect: ConnectOpts) -> anyhow::Result<()>`. Skeleton: `RpiToml::load(Path::new("rpi.toml"))?` only if the command needs project context; then `connect.resolve()?` → `SshTunnel::open(&profile).await?` → `ApiClient::new(tunnel.base_url.clone())`. Extract rendering into a pure `pub(crate) fn render_*` for tests.
9. `crates/bin/src/main.rs` — variant in `enum Cmd` with `///` doc comment (it IS the help text), `#[command(flatten)] connect: cli::config::ConnectOpts` for remote commands, match arm in `run()` delegating to `cli::commands::<fn>`.

## Output conventions

- stdout = data, stderr = status/progress (`success`/`warn`/`note`/`error` print to stderr; `heading` and tables to stdout).
- Tabular data: `output::table()`, UPPERCASE headers, `-` for absent cells, `println!("{table}")`. No ANSI styling inside cells (comfy-table width math breaks).
- Sectioned lists: `output::heading("label:")` + indented `println!("  {item}")` (see `secrets_ls`).
- One-shot confirmations: `output::success(...)`; hints: `output::note(...)`.
- Streamed long-running output, two shapes: progress a user watches (deploy) → `output::LogPane::new(label, 10)` + `push_line` + `finish_ok`/`finish_neutral`/`finish_err`; remote output that IS the payload (`rpi command`) → `output::log_line` per line, unframed and untruncated to stdout, verdict via `success`/`error`. Either way, on failure `drop(tunnel)` then `std::process::exit(code)`.
- Machine-readable views get a `--json` flag: `serde_json::to_string_pretty` + early return before any table code.
- Empty state: single plain `println!("no X ...")`, exit 0.
- Durations: `human_duration`; PASS/FAIL strings: `styled_ok`/`styled_err` (pure, testable).

## Required tests

- [ ] clap parse test in `main.rs` `mod tests` — every subcommand has one (`stats`, `agent_logs_flags_parse`, ...). Do not skip.
- [ ] Pure render helper test in `commands.rs` (pattern: `render_doctor`, `expose_cell`).
- [ ] `ApiClient` test against a local axum app via `spawn_app` in `api.rs` — required for streaming or non-trivial error mapping (404 compat); plain `extract_error` + JSON GET/POST wrappers are already covered by the http.rs integration test.
- [ ] Handler integration test in `agent/http.rs` `mod tests` (`state_with` + `oneshot`), including the invalid-name 400 case.
- [ ] Use case test with mockall mocks in the application crate (if a use case was added).

## Finish

Run the CI gate (also enforced by the Stop hook): `rtk cargo fmt --all -- --check && rtk cargo clippy --all-targets --locked -- -D warnings && rtk cargo test --locked`.

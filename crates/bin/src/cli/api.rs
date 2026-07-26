use futures::StreamExt;

use std::collections::BTreeMap;

use crate::cli::sse::SseParser;
use crate::proto::{
    AgentOverviewDto, CommandRunRequest, CommandsResponse, DeployAccepted, DeployRequest,
    DeploymentDto, DiagnosticReportDto, EnvironmentActionResponse, EnvironmentViewDto, GcResponse,
    LifecycleResponse, ProjectViewDto, RemoveResponse, SecretsListResponse, SecretsSendRequest,
    SecretsSendResponse, SourceCheckRequest, SourceCheckResponse, StatsReportDto, VersionInfo,
};

#[derive(Debug, serde::Deserialize)]
pub struct StageEventDto {
    pub stage: String,
    pub status: String,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

pub enum DeployStreamEvent<'a> {
    Line(&'a str),
    Stage(StageEventDto),
    Summary { services: usize },
}

fn parse_stage(data: &str) -> Option<StageEventDto> {
    serde_json::from_str(data).ok()
}

fn parse_summary(data: &str) -> Option<usize> {
    #[derive(serde::Deserialize)]
    struct SummaryDto {
        services: usize,
    }
    serde_json::from_str::<SummaryDto>(data)
        .ok()
        .map(|s| s.services)
}

async fn extract_error(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let msg = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| status.to_string());
    anyhow::bail!("{msg}")
}

/// The {"error"} message of a JSON 404 body, or `None` for a bare 404.
/// Every rpi-agent error carries a JSON body; axum's bare 404 (route not
/// registered) does not — that is how an old agent reveals itself.
async fn json_404_error(resp: reqwest::Response) -> Option<String> {
    let bytes = resp.bytes().await.unwrap_or_default();
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
}

/// Race safety net (spec 2026-07-12): the agent may have been swapped
/// between the handshake and this call. A bare 404 means the running agent
/// does not serve `feature`; a JSON 404 is a domain not-found and passes
/// through verbatim.
async fn expect_feature(
    resp: reqwest::Response,
    feature: crate::compat::Feature,
) -> anyhow::Result<reqwest::Response> {
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return match json_404_error(resp).await {
            Some(msg) => Err(anyhow::anyhow!("{msg}")),
            None => Err(crate::compat::feature_unavailable_error(feature)),
        };
    }
    extract_error(resp).await
}

pub struct ApiClient {
    http: reqwest::Client,
    base: String,
}

impl ApiClient {
    pub fn new(base: String) -> ApiClient {
        ApiClient {
            http: reqwest::Client::new(),
            base,
        }
    }

    pub async fn version(&self) -> anyhow::Result<VersionInfo> {
        let resp = self
            .http
            .get(format!("{}/v1/version", self.base))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("agent does not expose /v1 — incompatible agent; update it on the Pi");
        }
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn deploy(&self, req: &DeployRequest) -> anyhow::Result<DeployAccepted> {
        let resp = self
            .http
            .post(format!("{}/v1/deployments", self.base))
            .json(req)
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    /// Deploy-key preflight. `Ok(None)` — the agent predates the route (bare
    /// 404): callers skip the preflight and deploy as before. Deliberate
    /// deviation from the usual bail-on-404 pattern (spec 2026-07-10): old
    /// agents still print the key hint inside the fetch stage, so degrading
    /// silently keeps first deploys working instead of blocking them. The
    /// blanket `Ok(None)`-on-any-404 stays as the Silent policy's
    /// transport-level fallback (spec 2026-07-12) — unlike `expect_feature`,
    /// this method never distinguishes bare vs. JSON 404.
    pub async fn source_check(
        &self,
        project: &str,
        repo: &str,
    ) -> anyhow::Result<Option<SourceCheckResponse>> {
        let resp = self
            .http
            .post(format!("{}/v1/projects/{project}/source/check", self.base))
            .json(&SourceCheckRequest {
                repo: repo.to_string(),
            })
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(extract_error(resp).await?.json().await?))
    }

    pub async fn active_deployments(&self, project: &str) -> anyhow::Result<Vec<DeploymentDto>> {
        let resp = self
            .http
            .get(format!(
                "{}/v1/projects/{project}/deployments/active",
                self.base
            ))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn cancel_deployment(&self, id: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .delete(format!("{}/v1/deployments/{id}", self.base))
            .send()
            .await?;
        let json: serde_json::Value = extract_error(resp).await?.json().await?;
        Ok(json["status"].as_str().unwrap_or("unknown").to_string())
    }

    pub async fn gc(&self) -> anyhow::Result<GcResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/gc", self.base))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn stats(&self, project: Option<&str>) -> anyhow::Result<StatsReportDto> {
        let url = match project {
            Some(project) => format!("{}/v1/stats?project={project}", self.base),
            None => format!("{}/v1/stats", self.base),
        };
        let resp = self.http.get(url).send().await?;
        Ok(expect_feature(resp, crate::compat::Feature::Stats)
            .await?
            .json()
            .await?)
    }

    pub async fn agent_status(&self) -> anyhow::Result<AgentOverviewDto> {
        let resp = self
            .http
            .get(format!("{}/v1/status", self.base))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn doctor(&self) -> anyhow::Result<DiagnosticReportDto> {
        let resp = self
            .http
            .get(format!("{}/v1/doctor", self.base))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn lifecycle(
        &self,
        project: &str,
        action: &str,
    ) -> anyhow::Result<LifecycleResponse> {
        let resp = self
            .http
            .post(format!(
                "{}/v1/projects/{project}/lifecycle/{action}",
                self.base
            ))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn remove_project(
        &self,
        project: &str,
        volumes: bool,
    ) -> anyhow::Result<RemoveResponse> {
        let resp = self
            .http
            .delete(format!(
                "{}/v1/projects/{project}?volumes={volumes}",
                self.base
            ))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn follow_logs(
        &self,
        id: &str,
        mut on_event: impl FnMut(DeployStreamEvent<'_>),
    ) -> anyhow::Result<String> {
        let resp = self
            .http
            .get(format!("{}/v1/deployments/{id}/logs", self.base))
            .send()
            .await?;
        let resp = extract_error(resp).await?;
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            let valid_up_to = match std::str::from_utf8(&buf) {
                Ok(_) => buf.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => buf.len(),
            };
            if valid_up_to == 0 {
                continue;
            }
            let text = String::from_utf8_lossy(&buf[..valid_up_to]).into_owned();
            buf.drain(..valid_up_to);
            for ev in parser.push(&text) {
                match ev.event.as_str() {
                    "log" => on_event(DeployStreamEvent::Line(&ev.data)),
                    "stage" => {
                        if let Some(dto) = parse_stage(&ev.data) {
                            on_event(DeployStreamEvent::Stage(dto));
                        }
                    }
                    "summary" => {
                        if let Some(services) = parse_summary(&ev.data) {
                            on_event(DeployStreamEvent::Summary { services });
                        }
                    }
                    "finished" => return Ok(ev.data),
                    _ => {}
                }
            }
        }
        anyhow::bail!("log stream ended without a final status (agent restarted?)")
    }

    pub async fn stream_sse(
        &self,
        query: &str,
        mut on_line: impl FnMut(&str),
    ) -> anyhow::Result<()> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, query))
            .send()
            .await?;
        let resp = extract_error(resp).await?;
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            let valid_up_to = match std::str::from_utf8(&buf) {
                Ok(_) => buf.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => buf.len(),
            };
            if valid_up_to == 0 {
                continue;
            }
            let text = String::from_utf8_lossy(&buf[..valid_up_to]).into_owned();
            buf.drain(..valid_up_to);
            for ev in parser.push(&text) {
                if ev.event == "log" {
                    on_line(&ev.data);
                }
            }
        }
        Ok(())
    }

    pub async fn projects(&self) -> anyhow::Result<Vec<ProjectViewDto>> {
        let resp = self
            .http
            .get(format!("{}/v1/projects", self.base))
            .send()
            .await?;
        Ok(extract_error(resp).await?.json().await?)
    }

    pub async fn send_secrets(
        &self,
        project: &str,
        vars: BTreeMap<String, String>,
        files: BTreeMap<String, String>,
        file_mode: Option<u32>,
        expected_revision: Option<u64>,
        apply: bool,
    ) -> anyhow::Result<SecretsSendResponse> {
        let req = SecretsSendRequest {
            vars,
            files,
            file_mode,
            expected_revision,
            apply,
        };
        let resp = self
            .http
            .put(format!("{}/v1/projects/{project}/secrets", self.base))
            .json(&req)
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::Secrets)
            .await?
            .json()
            .await?)
    }

    pub async fn list_secrets(&self, project: &str) -> anyhow::Result<SecretsListResponse> {
        let resp = self
            .http
            .get(format!("{}/v1/projects/{project}/secrets", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::Secrets)
            .await?
            .json()
            .await?)
    }

    pub async fn list_commands(&self, project: &str) -> anyhow::Result<CommandsResponse> {
        let resp = self
            .http
            .get(format!("{}/v1/projects/{project}/commands", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::Commands)
            .await?
            .json()
            .await?)
    }

    /// Streams command output; returns the in-container exit code.
    pub async fn run_command(
        &self,
        project: &str,
        command: &str,
        args: &[String],
        mut on_line: impl FnMut(&str),
    ) -> anyhow::Result<i32> {
        let resp = self
            .http
            .post(format!(
                "{}/v1/projects/{project}/commands/{command}",
                self.base
            ))
            .json(&CommandRunRequest {
                args: args.to_vec(),
            })
            .send()
            .await?;
        let resp = expect_feature(resp, crate::compat::Feature::Commands).await?;
        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            let valid_up_to = match std::str::from_utf8(&buf) {
                Ok(_) => buf.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => buf.len(),
            };
            if valid_up_to == 0 {
                continue;
            }
            let text = String::from_utf8_lossy(&buf[..valid_up_to]).into_owned();
            buf.drain(..valid_up_to);
            for ev in parser.push(&text) {
                match ev.event.as_str() {
                    "log" => on_line(&ev.data),
                    "exit" => {
                        return ev.data.trim().parse::<i32>().map_err(|_| {
                            anyhow::anyhow!("agent sent invalid exit code '{}'", ev.data)
                        })
                    }
                    _ => {}
                }
            }
        }
        anyhow::bail!("command stream ended without an exit status (agent restarted?)")
    }

    pub async fn list_environments(
        &self,
        base: Option<&str>,
    ) -> anyhow::Result<Vec<EnvironmentViewDto>> {
        let url = match base {
            Some(base) => format!("{}/v1/environments?base={base}", self.base),
            None => format!("{}/v1/environments", self.base),
        };
        let resp = self.http.get(url).send().await?;
        Ok(expect_feature(resp, crate::compat::Feature::Environments)
            .await?
            .json()
            .await?)
    }

    pub async fn destroy_environment(
        &self,
        key: &str,
    ) -> anyhow::Result<EnvironmentActionResponse> {
        let resp = self
            .http
            .delete(format!("{}/v1/environments/{key}", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::Environments)
            .await?
            .json()
            .await?)
    }

    pub async fn reset_environment(&self, key: &str) -> anyhow::Result<EnvironmentActionResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/environments/{key}/reset-data", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::Environments)
            .await?
            .json()
            .await?)
    }

    pub async fn push_secret_group(
        &self,
        base: &str,
        group: &str,
        req: &crate::proto::SecretGroupPushRequest,
    ) -> anyhow::Result<crate::proto::SecretGroupPushResponse> {
        let resp = self
            .http
            .put(format!(
                "{}/v1/projects/{base}/secret-groups/{group}",
                self.base
            ))
            .json(req)
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn head_secret_group(
        &self,
        base: &str,
        group: &str,
    ) -> anyhow::Result<crate::proto::SecretGroupHeadResponse> {
        let resp = self
            .http
            .get(format!(
                "{}/v1/projects/{base}/secret-groups/{group}",
                self.base
            ))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn list_secret_groups(
        &self,
        base: &str,
    ) -> anyhow::Result<crate::proto::SecretGroupsListResponse> {
        let resp = self
            .http
            .get(format!("{}/v1/projects/{base}/secret-groups", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn delete_secret_group(
        &self,
        base: &str,
        group: &str,
        force: bool,
    ) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(format!(
                "{}/v1/projects/{base}/secret-groups/{group}?force={force}",
                self.base
            ))
            .send()
            .await?;
        expect_feature(resp, crate::compat::Feature::SecretGroups).await?;
        Ok(())
    }

    pub async fn head_key_secrets(
        &self,
        project: &str,
    ) -> anyhow::Result<crate::proto::SecretGroupHeadResponse> {
        let resp = self
            .http
            .get(format!("{}/v1/projects/{project}/secrets/head", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }

    pub async fn apply_key_secrets(
        &self,
        project: &str,
    ) -> anyhow::Result<crate::proto::SecretsApplyResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/projects/{project}/secrets/apply", self.base))
            .send()
            .await?;
        Ok(expect_feature(resp, crate::compat::Feature::SecretGroups)
            .await?
            .json()
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{delete, get, post, put};
    use axum::Router;
    use std::sync::{Arc, Mutex};

    /// Binds an ephemeral port, serves `app` in the background, and returns
    /// the base URL to point an `ApiClient` at.
    async fn spawn_app(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn sse_log_and_exit() -> impl IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "event: log\ndata: hello\n\nevent: exit\ndata: 7\n\n",
        )
    }

    async fn sse_no_exit() -> impl IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "event: log\ndata: hello\n\n",
        )
    }

    async fn sse_bad_exit() -> impl IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "event: exit\ndata: not-a-number\n\n",
        )
    }

    /// Splits the two-byte UTF-8 encoding of 'é' (0xC3 0xA9) across two
    /// separately-flushed chunks, exercising the `error_len().is_none()`
    /// (incomplete-sequence-at-end) branch in the client's chunk decoder.
    async fn sse_multibyte_split() -> Response {
        let stream = async_stream::stream! {
            yield Ok::<Vec<u8>, std::convert::Infallible>(b"event: log\ndata: h\xC3".to_vec());
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            yield Ok::<Vec<u8>, std::convert::Infallible>(
                b"\xA9llo\n\nevent: exit\ndata: 0\n\n".to_vec(),
            );
        };
        Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from_stream(stream))
            .unwrap()
    }

    async fn not_found_plain() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    async fn not_found_json() -> impl IntoResponse {
        (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "command 'nope' not found; available: create-invite"
            })),
        )
    }

    async fn source_check_denied() -> impl IntoResponse {
        axum::Json(serde_json::json!({
            "ok": false,
            "pubkey": "ssh-ed25519 AAAA pi-deploy-demo",
            "error": "Permission denied (publickey)"
        }))
    }

    async fn stats_extended() -> impl IntoResponse {
        axum::Json(serde_json::json!({
            "host": {
                "cpu_percent": 12.5,
                "mem_used_bytes": 1024,
                "mem_total_bytes": 4096,
                "disk_used_percent": 40,
                "uptime_secs": 3600,
                "temp_celsius": 47.5
            },
            "projects": [],
            "host_history": [
                {"at_ms": 1, "cpu_percent": 10.0, "mem_used_bytes": 1000, "mem_total_bytes": 4096, "temp_celsius": 45.0},
                {"at_ms": 2, "cpu_percent": 12.5, "mem_used_bytes": 1024, "mem_total_bytes": 4096, "temp_celsius": 47.5}
            ]
        }))
    }

    #[tokio::test]
    async fn stats_decodes_temp_and_history() {
        let app = Router::new().route("/v1/stats", get(stats_extended));
        let client = ApiClient::new(spawn_app(app).await);
        let resp = client.stats(None).await.unwrap();
        assert_eq!(resp.host.temp_celsius, Some(47.5));
        assert_eq!(resp.host_history.len(), 2);
        assert_eq!(resp.host_history[1].cpu_percent, 12.5);
    }

    #[tokio::test]
    async fn stats_bare_404_means_old_agent_and_prompts_update() {
        let app = Router::new(); // no /v1/stats route -> bare 404
        let api = ApiClient::new(spawn_app(app).await);
        let err = api.stats(None).await.unwrap_err().to_string();
        assert!(err.contains("stats"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[tokio::test]
    async fn source_check_returns_typed_payload() {
        let app = Router::new().route("/v1/projects/demo/source/check", post(source_check_denied));
        let client = ApiClient::new(spawn_app(app).await);

        let resp = client
            .source_check("demo", "git@github.com:x/y.git")
            .await
            .unwrap()
            .expect("new agent answers");

        assert!(!resp.ok);
        assert_eq!(
            resp.pubkey.as_deref(),
            Some("ssh-ed25519 AAAA pi-deploy-demo")
        );
        assert_eq!(resp.error.as_deref(), Some("Permission denied (publickey)"));
    }

    #[tokio::test]
    async fn source_check_404_means_old_agent_and_returns_none() {
        // Deliberate deviation from the bail-on-404 pattern (spec 2026-07-10):
        // old agents keep working, the fetch stage still prints the key hint.
        let app = Router::new().route("/v1/projects/demo/source/check", post(not_found_plain));
        let client = ApiClient::new(spawn_app(app).await);

        let resp = client
            .source_check("demo", "git@github.com:x/y.git")
            .await
            .unwrap();

        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn run_command_streams_log_and_returns_exit_code() {
        let app = Router::new().route("/v1/projects/demo/commands/build", post(sse_log_and_exit));
        let client = ApiClient::new(spawn_app(app).await);

        let mut lines = Vec::new();
        let code = client
            .run_command("demo", "build", &[], |l| lines.push(l.to_string()))
            .await
            .unwrap();

        assert_eq!(code, 7);
        assert_eq!(lines, vec!["hello".to_string()]);
    }

    #[tokio::test]
    async fn run_command_decodes_multibyte_utf8_split_across_chunks() {
        let app = Router::new().route(
            "/v1/projects/demo/commands/build",
            post(sse_multibyte_split),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let mut lines = Vec::new();
        let code = client
            .run_command("demo", "build", &[], |l| lines.push(l.to_string()))
            .await
            .unwrap();

        assert_eq!(code, 0);
        assert_eq!(lines, vec!["héllo".to_string()]);
    }

    #[tokio::test]
    async fn run_command_errors_if_stream_ends_without_exit() {
        let app = Router::new().route("/v1/projects/demo/commands/build", post(sse_no_exit));
        let client = ApiClient::new(spawn_app(app).await);

        let err = client
            .run_command("demo", "build", &[], |_| {})
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("without an exit status"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn run_command_errors_on_unparseable_exit_code() {
        let app = Router::new().route("/v1/projects/demo/commands/build", post(sse_bad_exit));
        let client = ApiClient::new(spawn_app(app).await);

        let err = client
            .run_command("demo", "build", &[], |_| {})
            .await
            .unwrap_err();

        assert!(err.to_string().contains("invalid exit code"), "got: {err}");
    }

    #[tokio::test]
    async fn list_commands_404_without_body_prompts_agent_update() {
        let app = Router::new().route("/v1/projects/demo/commands", get(not_found_plain));
        let api = ApiClient::new(spawn_app(app).await);
        let err = api.list_commands("demo").await.unwrap_err().to_string();
        assert!(err.contains("container commands"), "{err}");
        assert!(err.contains(">= 0.9.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[tokio::test]
    async fn list_commands_404_with_json_error_uses_message_verbatim() {
        let app = Router::new().route("/v1/projects/demo/commands", get(not_found_json));
        let client = ApiClient::new(spawn_app(app).await);

        let err = client.list_commands("demo").await.unwrap_err();

        assert_eq!(
            err.to_string(),
            "command 'nope' not found; available: create-invite"
        );
    }

    #[tokio::test]
    async fn list_secrets_404_without_body_prompts_agent_update() {
        let app = Router::new().route("/v1/projects/demo/secrets", get(not_found_plain));
        let api = ApiClient::new(spawn_app(app).await);
        let err = api.list_secrets("demo").await.unwrap_err().to_string();
        assert!(err.contains("secrets"), "{err}");
        assert!(err.contains(">= 0.9.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[tokio::test]
    async fn list_secrets_404_with_json_error_uses_message_verbatim() {
        let app = Router::new().route("/v1/projects/demo/secrets", get(not_found_json));
        let api = ApiClient::new(spawn_app(app).await);
        let err = api.list_secrets("demo").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "command 'nope' not found; available: create-invite"
        );
    }

    #[tokio::test]
    async fn send_secrets_404_without_body_prompts_agent_update() {
        let app = Router::new().route("/v1/projects/demo/secrets", put(not_found_plain));
        let api = ApiClient::new(spawn_app(app).await);
        let err = api
            .send_secrets("demo", BTreeMap::new(), BTreeMap::new(), None, None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("secrets"), "{err}");
        assert!(err.contains(">= 0.9.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[tokio::test]
    async fn push_secret_group_round_trips_request_and_response() {
        // Captures what the server actually received, so assertions run in
        // the test's own task rather than inside the (spawned) handler.
        let captured: Arc<Mutex<Option<(String, String, crate::proto::SecretGroupPushRequest)>>> =
            Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{base}/secret-groups/{group}",
            put(
                move |Path((base, group)): Path<(String, String)>,
                      axum::Json(req): axum::Json<crate::proto::SecretGroupPushRequest>| {
                    let captured2 = captured2.clone();
                    async move {
                        *captured2.lock().unwrap() = Some((base, group, req));
                        axum::Json(serde_json::json!({ "revision": 4, "keys": 1, "files": 1 }))
                    }
                },
            ),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let mut vars = BTreeMap::new();
        vars.insert("KEY".to_string(), "value".to_string());
        let mut files = BTreeMap::new();
        files.insert("cert.pem".to_string(), "UEVN".to_string());
        let req = crate::proto::SecretGroupPushRequest {
            vars,
            files,
            file_mode: Some(0o640),
            expected_revision: Some(3),
            merge: true,
        };
        let resp = client
            .push_secret_group("myapp", "preview", &req)
            .await
            .unwrap();
        assert_eq!(resp.revision, 4);
        assert_eq!(resp.keys, 1);
        assert_eq!(resp.files, 1);

        let (base, group, received) = captured.lock().unwrap().take().unwrap();
        assert_eq!(base, "myapp");
        assert_eq!(group, "preview");
        assert_eq!(received.vars.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(
            received.files.get("cert.pem").map(String::as_str),
            Some("UEVN")
        );
        assert_eq!(received.file_mode, Some(0o640));
        assert_eq!(received.expected_revision, Some(3));
        assert!(received.merge);
    }

    #[tokio::test]
    async fn head_secret_group_round_trips_response() {
        let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{base}/secret-groups/{group}",
            get(move |Path((base, group)): Path<(String, String)>| {
                let captured2 = captured2.clone();
                async move {
                    *captured2.lock().unwrap() = Some((base, group));
                    axum::Json(serde_json::json!({
                        "revision": 7,
                        "vars": {"KEY": "deadbeefcafebabe"},
                        "files": {"cert.pem": {"size": 3, "digest": "0123456789abcdef"}},
                        "file_mode": 416
                    }))
                }
            }),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let head = client.head_secret_group("myapp", "preview").await.unwrap();
        assert_eq!(head.revision, 7);
        assert_eq!(
            head.vars.get("KEY").map(String::as_str),
            Some("deadbeefcafebabe")
        );
        let file = head.files.get("cert.pem").unwrap();
        assert_eq!(file.size, 3);
        assert_eq!(file.digest, "0123456789abcdef");
        assert_eq!(head.file_mode, Some(0o640));

        assert_eq!(
            captured.lock().unwrap().take().unwrap(),
            ("myapp".to_string(), "preview".to_string())
        );
    }

    #[tokio::test]
    async fn head_secret_group_bare_404_means_old_agent_and_prompts_update() {
        let app = Router::new(); // no secret-groups route at all -> bare 404
        let api = ApiClient::new(spawn_app(app).await);
        let err = api
            .head_secret_group("myapp", "preview")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("secret groups"), "{err}");
        assert!(err.contains(">= 0.27.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[tokio::test]
    async fn list_secret_groups_round_trips_response() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{base}/secret-groups",
            get(move |Path(base): Path<String>| {
                let captured2 = captured2.clone();
                async move {
                    *captured2.lock().unwrap() = Some(base);
                    axum::Json(serde_json::json!({
                        "groups": [{
                            "name": "preview",
                            "revision": 2,
                            "keys": 1,
                            "files": 0,
                            "bytes": 0,
                            "updated_at": 1234,
                            "attached_by": ["myapp--branch"]
                        }]
                    }))
                }
            }),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let listed = client.list_secret_groups("myapp").await.unwrap();
        assert_eq!(listed.groups.len(), 1);
        let g = &listed.groups[0];
        assert_eq!(g.name, "preview");
        assert_eq!(g.revision, 2);
        assert_eq!(g.attached_by, vec!["myapp--branch".to_string()]);

        assert_eq!(captured.lock().unwrap().take().unwrap(), "myapp");
    }

    #[tokio::test]
    async fn delete_secret_group_sends_the_force_flag_and_succeeds() {
        #[derive(serde::Deserialize)]
        struct ForceQuery {
            #[serde(default)]
            force: bool,
        }

        let captured: Arc<Mutex<Option<(String, String, bool)>>> = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{base}/secret-groups/{group}",
            delete(
                move |Path((base, group)): Path<(String, String)>, Query(q): Query<ForceQuery>| {
                    let captured2 = captured2.clone();
                    async move {
                        *captured2.lock().unwrap() = Some((base, group, q.force));
                        axum::Json(serde_json::json!({ "removed": "preview" }))
                    }
                },
            ),
        );
        let client = ApiClient::new(spawn_app(app).await);

        client
            .delete_secret_group("myapp", "preview", true)
            .await
            .unwrap();

        assert_eq!(
            captured.lock().unwrap().take().unwrap(),
            ("myapp".to_string(), "preview".to_string(), true)
        );
    }

    #[tokio::test]
    async fn head_key_secrets_round_trips_response() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{name}/secrets/head",
            get(move |Path(name): Path<String>| {
                let captured2 = captured2.clone();
                async move {
                    *captured2.lock().unwrap() = Some(name);
                    axum::Json(serde_json::json!({
                        "revision": 5,
                        "vars": {"A": "aaaaaaaaaaaaaaaa"},
                        "files": {},
                        "file_mode": null
                    }))
                }
            }),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let head = client.head_key_secrets("myapp--branch").await.unwrap();
        assert_eq!(head.revision, 5);
        assert_eq!(
            head.vars.get("A").map(String::as_str),
            Some("aaaaaaaaaaaaaaaa")
        );
        assert!(head.files.is_empty());
        assert_eq!(head.file_mode, None);

        assert_eq!(
            captured.lock().unwrap().take().unwrap(),
            "myapp--branch".to_string()
        );
    }

    #[tokio::test]
    async fn apply_key_secrets_round_trips_response() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured2 = captured.clone();
        let app = Router::new().route(
            "/v1/projects/{name}/secrets/apply",
            post(move |Path(name): Path<String>| {
                let captured2 = captured2.clone();
                async move {
                    *captured2.lock().unwrap() = Some(name);
                    axum::Json(serde_json::json!({ "keys": 2, "files": 1 }))
                }
            }),
        );
        let client = ApiClient::new(spawn_app(app).await);

        let resp = client.apply_key_secrets("myapp--branch").await.unwrap();
        assert_eq!(resp.keys, 2);
        assert_eq!(resp.files, 1);

        assert_eq!(
            captured.lock().unwrap().take().unwrap(),
            "myapp--branch".to_string()
        );
    }

    #[tokio::test]
    async fn apply_key_secrets_bare_404_means_old_agent_and_prompts_update() {
        let app = Router::new(); // no secrets/apply route at all -> bare 404
        let api = ApiClient::new(spawn_app(app).await);
        let err = api
            .apply_key_secrets("myapp--branch")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("secret groups"), "{err}");
        assert!(err.contains(">= 0.27.0"), "{err}");
        assert!(err.contains("update the agent on the Pi"), "{err}");
    }

    #[test]
    fn parse_stage_accepts_valid_and_rejects_malformed_payloads() {
        let ev = parse_stage(r#"{"stage":"build","status":"ok","elapsed_ms":48231}"#).unwrap();
        assert_eq!(ev.stage, "build");
        assert_eq!(ev.status, "ok");
        assert_eq!(ev.elapsed_ms, Some(48231));

        let started = parse_stage(r#"{"stage":"fetch","status":"started"}"#).unwrap();
        assert_eq!(started.elapsed_ms, None);

        assert!(parse_stage("not json").is_none());
        assert!(
            parse_stage(r#"{"status":"ok"}"#).is_none(),
            "missing stage field"
        );
    }

    #[test]
    fn parse_summary_accepts_valid_and_rejects_malformed_payloads() {
        assert_eq!(parse_summary(r#"{"services":2}"#), Some(2));
        assert_eq!(parse_summary("nope"), None);
    }
}

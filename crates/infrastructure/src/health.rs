use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_domain::contracts::{ContainerRuntime, HealthGate, LogSink};
use pi_domain::entities::ProjectConfig;
use pi_domain::error::DomainError;

/// "2xx" | "3xx" | exact code; None => 2xx/3xx ([healthcheck].expect, §12).
pub(crate) fn status_matches(expect: Option<&str>, status: u16) -> bool {
    match expect {
        None => (200..400).contains(&status),
        Some("2xx") => (200..300).contains(&status),
        Some("3xx") => (300..400).contains(&status),
        Some(code) => code.parse::<u16>().map(|c| c == status).unwrap_or(false),
    }
}

enum Probe {
    Pass,
    Wait(String),
}

/// Hybrid deploy gate (§8): docker healthcheck when declared on the public
/// service, else HTTP GET on the host port when [healthcheck].path is set,
/// else plain TCP connect. Polls until pass or timeout.
pub struct HybridHealthGate {
    runtime: Arc<dyn ContainerRuntime>,
    http: reqwest::Client,
    interval: Duration,
}

impl HybridHealthGate {
    pub fn new(runtime: Arc<dyn ContainerRuntime>) -> Arc<HybridHealthGate> {
        HybridHealthGate::with_interval(runtime, Duration::from_secs(2))
    }

    /// Tests use a short interval.
    pub fn with_interval(
        runtime: Arc<dyn ContainerRuntime>,
        interval: Duration,
    ) -> Arc<HybridHealthGate> {
        Arc::new(HybridHealthGate {
            runtime,
            http: reqwest::Client::new(),
            interval,
        })
    }

    async fn probe(&self, config: &ProjectConfig, host_port: u16) -> Result<Probe, DomainError> {
        // 1. docker healthcheck on the public service, when declared
        let services = self.runtime.ps(&config.name).await?;
        let health = services
            .iter()
            .find(|s| s.service == config.service)
            .and_then(|s| s.health.clone());
        match health.as_deref() {
            Some("healthy") => return Ok(Probe::Pass),
            Some("unhealthy") => {
                return Err(DomainError::HealthCheck(
                    "docker reports the public service unhealthy".into(),
                ))
            }
            Some(other) => return Ok(Probe::Wait(format!("docker health: {other}"))),
            None => {}
        }
        // 2. HTTP probe when a path is configured, else 3. TCP connect
        match &config.healthcheck.path {
            Some(path) => {
                let url = format!("http://127.0.0.1:{host_port}{path}");
                match self.http.get(&url).send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        if status_matches(config.healthcheck.expect.as_deref(), status) {
                            Ok(Probe::Pass)
                        } else {
                            Ok(Probe::Wait(format!("GET {path} -> {status}")))
                        }
                    }
                    Err(e) => Ok(Probe::Wait(format!("GET {path}: {e}"))),
                }
            }
            None => match tokio::net::TcpStream::connect(("127.0.0.1", host_port)).await {
                Ok(_) => Ok(Probe::Pass),
                Err(e) => Ok(Probe::Wait(format!(
                    "tcp connect 127.0.0.1:{host_port}: {e}"
                ))),
            },
        }
    }
}

#[async_trait]
impl HealthGate for HybridHealthGate {
    async fn check(
        &self,
        config: &ProjectConfig,
        host_port: u16,
        log: Arc<dyn LogSink>,
    ) -> Result<(), DomainError> {
        let budget = Duration::from_secs(config.healthcheck.timeout_secs);
        let deadline = tokio::time::Instant::now() + budget;
        log.line(&format!(
            "healthcheck: waiting up to {}s ...",
            budget.as_secs()
        ));
        loop {
            let wait_reason = match self.probe(config, host_port).await? {
                Probe::Pass => {
                    log.line("healthcheck: passed");
                    return Ok(());
                }
                Probe::Wait(reason) => reason,
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(DomainError::HealthCheck(format!(
                    "timed out after {}s (last probe: {wait_reason})",
                    budget.as_secs()
                )));
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::CollectSink;
    use pi_domain::contracts::MockContainerRuntime;
    use pi_domain::entities::{ExposeMode, HealthcheckConfig, ServiceState, StageTimeoutOverrides};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sink() -> Arc<dyn LogSink> {
        CollectSink::new()
    }

    fn config(healthcheck: HealthcheckConfig) -> ProjectConfig {
        ProjectConfig {
            name: "rateme".into(),
            repo: "https://github.com/x/y.git".into(),
            branch: "main".into(),
            compose_path: "docker-compose.yml".into(),
            service: "web".into(),
            container_port: 3000,
            hostname: None,
            expose: ExposeMode::default(),
            healthcheck,
            timeouts: StageTimeoutOverrides::default(),
            commands: Default::default(),
            command_timeout_secs: None,
            environment: None,
            secret_groups: Vec::new(),
        }
    }

    fn web(health: Option<&str>) -> Vec<ServiceState> {
        vec![ServiceState {
            service: "web".into(),
            state: "running".into(),
            health: health.map(str::to_string),
        }]
    }

    fn gate(runtime: MockContainerRuntime) -> Arc<HybridHealthGate> {
        HybridHealthGate::with_interval(Arc::new(runtime), Duration::from_millis(10))
    }

    #[test]
    fn status_matches_classes_and_exact_codes() {
        assert!(status_matches(None, 200) && status_matches(None, 302));
        assert!(!status_matches(None, 404));
        assert!(status_matches(Some("2xx"), 204) && !status_matches(Some("2xx"), 301));
        assert!(status_matches(Some("3xx"), 301) && !status_matches(Some("3xx"), 200));
        assert!(status_matches(Some("418"), 418) && !status_matches(Some("418"), 200));
        assert!(!status_matches(Some("bogus"), 200));
    }

    #[tokio::test]
    async fn docker_healthy_passes() {
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(|_| Ok(web(Some("healthy"))));
        gate(runtime)
            .check(&config(HealthcheckConfig::default()), 1, sink())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn docker_unhealthy_fails_without_waiting_for_timeout() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_ps()
            .times(1)
            .returning(|_| Ok(web(Some("unhealthy"))));
        let err = gate(runtime)
            .check(&config(HealthcheckConfig::default()), 1, sink())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::HealthCheck(_)), "got: {err}");
    }

    #[tokio::test]
    async fn docker_starting_then_healthy_passes() {
        let calls = AtomicUsize::new(0);
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Ok(web(Some("starting")))
            } else {
                Ok(web(Some("healthy")))
            }
        });
        gate(runtime)
            .check(&config(HealthcheckConfig::default()), 1, sink())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tcp_probe_passes_when_port_listens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(|_| Ok(web(None)));
        gate(runtime)
            .check(&config(HealthcheckConfig::default()), port, sink())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn http_probe_checks_expected_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut conn, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = conn.read(&mut buf).await;
                    let _ = conn
                        .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                        .await;
                }
            }
        });
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(|_| Ok(web(None)));
        let hc = HealthcheckConfig {
            path: Some("/health".into()),
            expect: Some("204".into()),
            timeout_secs: 5,
        };
        gate(runtime)
            .check(&config(hc), port, sink())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn times_out_with_last_probe_reason() {
        // port from a dropped listener: nothing listens there
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(|_| Ok(web(None)));
        let hc = HealthcheckConfig {
            timeout_secs: 0,
            ..HealthcheckConfig::default()
        };
        let err = gate(runtime)
            .check(&config(hc), port, sink())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DomainError::HealthCheck(m) if m.contains("timed out")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn ps_failure_propagates_as_error() {
        let mut runtime = MockContainerRuntime::new();
        runtime
            .expect_ps()
            .returning(|_| Err(DomainError::Runtime("docker daemon down".into())));
        let err = gate(runtime)
            .check(&config(HealthcheckConfig::default()), 1, sink())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Runtime(_)), "got: {err}");
    }
}

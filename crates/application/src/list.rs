use std::net::IpAddr;
use std::sync::Arc;

use pi_domain::contracts::{ContainerRuntime, HostNetwork, ProjectRepository};
use pi_domain::entities::{ExposeMode, ServiceState};
use pi_domain::error::DomainError;

/// Line for `rpi ls`: project + state of its services (§7 ListProjects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectView {
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub hostname: Option<String>,
    pub host_port: u16,
    pub expose: ExposeMode,
    /// Detected LAN ip for expose=lan projects (None for private or when
    /// undetectable). Used by `rpi ls` to render `lan http://<ip>:<port>`.
    pub lan_ip: Option<IpAddr>,
    pub services: Vec<ServiceState>,
}

pub struct ListProjects {
    projects: Arc<dyn ProjectRepository>,
    runtime: Arc<dyn ContainerRuntime>,
    host_network: Arc<dyn HostNetwork>,
}

impl ListProjects {
    pub fn new(
        projects: Arc<dyn ProjectRepository>,
        runtime: Arc<dyn ContainerRuntime>,
        host_network: Arc<dyn HostNetwork>,
    ) -> Arc<ListProjects> {
        Arc::new(ListProjects {
            projects,
            runtime,
            host_network,
        })
    }

    pub async fn execute(&self) -> Result<Vec<ProjectView>, DomainError> {
        let projects = self.projects.list().await?;

        // Detect the host LAN ip only when at least one project needs it
        // (UDP-connect syscall offloaded to a blocking thread like deploy.rs).
        // Avoids the syscall for empty lists, private-only stacks, and — by
        // returning early above on a repo error — repository failures.
        let needs_lan_ip = projects.iter().any(|p| p.config.expose == ExposeMode::Lan);
        let lan_ip = if needs_lan_ip {
            let hn = Arc::clone(&self.host_network);
            tokio::task::spawn_blocking(move || hn.primary_ipv4())
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let mut views = Vec::new();
        for project in projects {
            // ps error (stack not yet up / docker unavailable) is not a reason
            // to drop the entire list: we show the project without services.
            let services = self
                .runtime
                .ps(&project.config.name)
                .await
                .unwrap_or_default();
            let expose = project.config.expose;
            views.push(ProjectView {
                name: project.config.name,
                repo: project.config.repo,
                branch: project.config.branch,
                hostname: project.config.hostname,
                host_port: project.host_port,
                expose,
                lan_ip: if expose == ExposeMode::Lan {
                    lan_ip
                } else {
                    None
                },
                services,
            });
        }
        Ok(views)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_domain::contracts::{MockContainerRuntime, MockHostNetwork, MockProjectRepository};
    use pi_domain::entities::{
        ExposeMode, HealthcheckConfig, Project, ProjectConfig, ServiceState, StageTimeoutOverrides,
    };
    use pi_domain::error::DomainError;

    fn project(name: &str, host_port: u16) -> Project {
        Project {
            config: ProjectConfig {
                name: name.into(),
                repo: format!("git@github.com:x/{name}.git"),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
                service: "web".into(),
                container_port: 3000,
                hostname: None,
                expose: ExposeMode::default(),
                healthcheck: HealthcheckConfig::default(),
                timeouts: StageTimeoutOverrides::default(),
                commands: Default::default(),
                command_timeout_secs: None,
                environment: None,
            },
            host_port,
            created_at: 1,
            on_create_done: false,
            last_success_at: None,
            last_commit_sha: None,
        }
    }

    /// HostNetwork mock that asserts `primary_ipv4` is never called — used by
    /// tests whose projects are all private (or whose `list()` fails), so the
    /// agent must skip the UDP-connect syscall entirely.
    fn unused_net() -> MockHostNetwork {
        let mut net = MockHostNetwork::new();
        net.expect_primary_ipv4().times(0);
        net
    }

    #[tokio::test]
    async fn lists_projects_with_service_states() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Ok(vec![project("a", 8000), project("b", 8001)]));

        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().withf(|n| n == "a").returning(|_| {
            Ok(vec![ServiceState {
                service: "web".into(),
                state: "running".into(),
                health: None,
            }])
        });
        // ps for "b" fails (stack never been up) - show empty list of services.
        runtime
            .expect_ps()
            .withf(|n| n == "b")
            .returning(|_| Err(DomainError::Runtime("no such project".into())));

        let list = ListProjects::new(
            Arc::new(projects),
            Arc::new(runtime),
            Arc::new(unused_net()),
        );
        let views = list.execute().await.unwrap();

        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, "a");
        assert_eq!(views[0].host_port, 8000);
        assert_eq!(views[0].expose, ExposeMode::default());
        assert_eq!(views[0].lan_ip, None);
        assert_eq!(
            views[0].services,
            vec![ServiceState {
                service: "web".into(),
                state: "running".into(),
                health: None,
            }]
        );
        assert_eq!(views[1].name, "b");
        assert!(views[1].services.is_empty());
    }

    #[tokio::test]
    async fn propagates_project_repository_errors_without_querying_runtime() {
        let mut projects = MockProjectRepository::new();
        projects
            .expect_list()
            .returning(|| Err(DomainError::Storage("db unavailable".into())));

        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().times(0);

        let mut net = MockHostNetwork::new();
        net.expect_primary_ipv4().times(0);

        let list = ListProjects::new(Arc::new(projects), Arc::new(runtime), Arc::new(net));
        let err = list.execute().await.unwrap_err();

        assert!(matches!(err, DomainError::Storage(message) if message == "db unavailable"));
    }

    #[tokio::test]
    async fn lan_projects_get_ip_private_projects_do_not() {
        let mut projects = MockProjectRepository::new();
        projects.expect_list().returning(|| {
            let mut lan = project("lan-app", 8000);
            lan.config.expose = ExposeMode::Lan;
            Ok(vec![lan, project("priv-app", 8001)])
        });
        let mut runtime = MockContainerRuntime::new();
        runtime.expect_ps().returning(|_| Ok(vec![]));
        let mut net = MockHostNetwork::new();
        net.expect_primary_ipv4()
            .returning(|| Some("192.168.1.50".parse().unwrap()));

        let list = ListProjects::new(Arc::new(projects), Arc::new(runtime), Arc::new(net));
        let views = list.execute().await.unwrap();

        let lan = views.iter().find(|v| v.name == "lan-app").unwrap();
        let priv_ = views.iter().find(|v| v.name == "priv-app").unwrap();
        assert_eq!(lan.expose, ExposeMode::Lan);
        assert_eq!(lan.lan_ip, Some("192.168.1.50".parse().unwrap()));
        assert_eq!(priv_.expose, ExposeMode::Private);
        assert_eq!(priv_.lan_ip, None);
    }
}

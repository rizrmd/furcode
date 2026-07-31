use std::collections::BTreeMap;

use furcode_app::McpServerInfra;
use furcode_domain::{Environment, McpServerConfig};

use crate::mcp_client::FurcodeMcpClient;

#[derive(Clone)]
pub struct FurcodeMcpServer;

#[async_trait::async_trait]
impl McpServerInfra for FurcodeMcpServer {
    type Client = FurcodeMcpClient;

    async fn connect(
        &self,
        config: McpServerConfig,
        env_vars: &BTreeMap<String, String>,
        environment: &Environment,
    ) -> anyhow::Result<Self::Client> {
        Ok(FurcodeMcpClient::new(config, env_vars, environment.clone()))
    }
}

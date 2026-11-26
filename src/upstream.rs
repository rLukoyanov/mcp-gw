use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceExt,
    model::{ClientCapabilities, ClientConfig, Implementation},
    service::{NotificationContext, RunningServiceCancellationToken},
    transport::{
        IntoTransport, StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use crate::config::{ServerConfig, Transport};
use crate::registry::GatewayState;

/// MCP client handler for one upstream server.
///
/// It only needs to react to `notifications/tools/list_changed`: when the
/// upstream server changes its tool set, we re-fetch and re-index, keeping the
/// host-side catalog fresh without polling.
#[derive(Clone)]
pub struct UpstreamClient {
    server: String,
    state: Arc<GatewayState>,
}

impl ClientHandler for UpstreamClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("tokenmaxing", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        match context.peer.list_all_tools().await {
            Ok(tools) => {
                let count = tools.len();
                self.state.clear_tool_cache_server(&self.server);
                self.state.replace_server_tools(&self.server, tools);
                eprintln!("server '{}': refreshed {count} tools", self.server);
            }
            Err(err) => eprintln!(
                "server '{}': failed to refresh tool list: {err}",
                self.server
            ),
        }
    }
}

/// Keep a served client alive in the background, returning a handle and a
/// cancellation token that tears the connection down.
async fn start<T, E, A>(
    client: UpstreamClient,
    transport: T,
) -> anyhow::Result<(Peer<RoleClient>, RunningServiceCancellationToken)>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let running = client.serve(transport).await?;
    let peer = running.peer().clone();
    let token = running.cancellation_token();
    tokio::spawn(async move {
        if let Err(err) = running.waiting().await {
            eprintln!("upstream connection closed: {err}");
        }
    });
    Ok((peer, token))
}

async fn connect(
    config: &ServerConfig,
    state: Arc<GatewayState>,
) -> anyhow::Result<(Peer<RoleClient>, RunningServiceCancellationToken)> {
    let client = UpstreamClient {
        server: config.name.clone(),
        state,
    };

    match &config.transport {
        Transport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            cmd.envs(env);
            let transport = TokioChildProcess::new(cmd)
                .with_context(|| format!("spawning upstream command '{command}'"))?;
            start(client, transport).await
        }
        Transport::Http { url, headers } => {
            let mut custom_headers = HashMap::new();
            for (key, value) in headers {
                let name = http::HeaderName::from_bytes(key.as_bytes())
                    .with_context(|| format!("invalid header name '{key}'"))?;
                let value = http::HeaderValue::from_str(value)
                    .with_context(|| format!("invalid header value for '{key}'"))?;
                custom_headers.insert(name, value);
            }
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(url.clone())
                    .custom_headers(custom_headers),
            );
            start(client, transport).await
        }
    }
}

/// Connect to a configured server, register it, and load its tool list.
/// Returns the number of tools discovered.
pub async fn enable(config: &ServerConfig, state: Arc<GatewayState>) -> anyhow::Result<usize> {
    let (peer, token) = connect(config, state.clone()).await?;
    state.set_connected(&config.name, peer.clone(), token);

    match peer.list_all_tools().await {
        Ok(tools) => {
            let count = tools.len();
            state.replace_server_tools(&config.name, tools);
            eprintln!("server '{}': connected, {count} tools", config.name);
            Ok(count)
        }
        Err(err) => {
            state.disconnect(&config.name);
            Err(anyhow::anyhow!("tools/list failed: {err}"))
        }
    }
}

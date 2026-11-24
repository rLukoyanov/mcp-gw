// A tiny upstream MCP server used to exercise the gateway end-to-end.
// Run it directly (`cargo run --bin demo_upstream`) or point mcp.json at the
// compiled binary with a `command` entry.

use rmcp::{
    ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio,
};
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// Text to echo back.
    text: String,
}

#[derive(Clone)]
struct DemoServer;

#[tool_router(server_handler)]
impl DemoServer {
    #[tool(name = "echo", description = "Echo the provided text back.")]
    fn echo(&self, Parameters(EchoParams { text }): Parameters<EchoParams>) -> String {
        text
    }

    #[tool(name = "ping", description = "Return the string 'pong'.")]
    fn ping(&self) -> String {
        "pong".to_string()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = DemoServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

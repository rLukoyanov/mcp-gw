mod config;
mod registry;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ServerCapabilities, ServerConfig,
    },
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use registry::{CatalogEntry, GatewayState};

/// Meta-tools implement the "progressive discovery" pattern: the model first
/// searches a compact catalog, inspects only the tool it needs, then executes
/// it. Upstream tool definitions never all enter the context at once.
#[derive(Clone)]
struct GatewayServer {
    state: Arc<GatewayState>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DetailLevel {
    /// Only the tool name and its server.
    Name,
    /// Name, title and one-line description (default).
    #[default]
    Summary,
    /// Everything, including the input schema.
    Full,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchToolsParams {
    /// Free-text query matched against tool name, title, description and server.
    query: String,
    /// Maximum number of results to return (defaults to 10).
    limit: Option<usize>,
    /// How much detail to include per result.
    detail: Option<DetailLevel>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ToolSummary {
    name: String,
    server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_schema: Option<Value>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SearchToolsResult {
    query: String,
    total: usize,
    tools: Vec<ToolSummary>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetToolDetailsParams {
    /// Exact tool name, as returned by `search_tools`.
    name: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ToolDetails {
    name: String,
    server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_schema: Option<Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CallToolParams {
    /// Exact tool name, as returned by `search_tools`.
    name: String,
    /// Arguments matching the tool's input schema (a JSON object).
    #[serde(default)]
    arguments: Option<Value>,
}

#[tool_router]
impl GatewayServer {
    /// Layer 1 — catalog: search available tools by keyword.
    #[tool(
        name = "search_tools",
        description = "Search the catalog of tools available through the gateway. Returns matching tool names and the upstream server that provides them, with optional descriptions and schemas."
    )]
    fn search_tools(
        &self,
        Parameters(SearchToolsParams {
            query,
            limit,
            detail,
        }): Parameters<SearchToolsParams>,
    ) -> Json<SearchToolsResult> {
        let detail = detail.unwrap_or_default();
        let limit = limit.unwrap_or(10).clamp(1, 50);
        let matches = self.state.search(&query, limit);
        Json(SearchToolsResult {
            query,
            total: matches.len(),
            tools: matches
                .into_iter()
                .map(|entry| summarize(&entry, &detail))
                .collect(),
        })
    }

    /// Layer 2 — inspect: fetch the full definition for a single tool.
    #[tool(
        name = "get_tool_details",
        description = "Get the full definition of one tool by its exact name, including its input and output JSON schemas."
    )]
    fn get_tool_details(
        &self,
        Parameters(GetToolDetailsParams { name }): Parameters<GetToolDetailsParams>,
    ) -> Result<Json<ToolDetails>, ErrorData> {
        let entry = self.state.get(&name).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!("tool '{name}' not found; use search_tools first"),
                None,
            )
        })?;

        let tool = &entry.tool;
        Ok(Json(ToolDetails {
            name: entry.name,
            server: entry.server,
            title: tool.title.clone(),
            description: tool.description.as_ref().map(|d| d.to_string()),
            input_schema: Value::Object((*tool.input_schema).clone()),
            output_schema: tool
                .output_schema
                .as_ref()
                .map(|schema| Value::Object((**schema).clone())),
        }))
    }

    /// Layer 3 — execute: call a tool on its owning upstream server.
    #[tool(
        name = "call_tool",
        description = "Invoke a tool by its exact name with arguments matching its input schema. Routes the call to the upstream server that owns the tool."
    )]
    async fn call_tool(
        &self,
        Parameters(CallToolParams { name, arguments }): Parameters<CallToolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let entry = self.state.get(&name).ok_or_else(|| {
            ErrorData::resource_not_found(
                format!("tool '{name}' not found; use search_tools first"),
                None,
            )
        })?;

        let peer = self.state.peer(&entry.server).ok_or_else(|| {
            ErrorData::internal_error(format!("server '{}' is not connected", entry.server), None)
        })?;

        let mut request = CallToolRequestParams::new(entry.tool.name.clone());
        match arguments {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => request = request.with_arguments(map),
            Some(_) => {
                return Err(ErrorData::invalid_params(
                    "arguments must be a JSON object",
                    None,
                ));
            }
        }

        peer.call_tool(request)
            .await
            .map_err(|err| ErrorData::internal_error(format!("upstream call failed: {err}"), None))
    }
}

#[tool_handler]
impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-gw", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Progressive tool discovery gateway. Use search_tools to find capabilities, \
             get_tool_details to inspect a tool's schema, then call_tool to invoke it."
                    .to_string(),
            )
    }
}

fn summarize(entry: &CatalogEntry, detail: &DetailLevel) -> ToolSummary {
    let tool = &entry.tool;
    let (title, description, input_schema) = match detail {
        DetailLevel::Name => (None, None, None),
        DetailLevel::Summary => (
            tool.title.clone(),
            tool.description.as_ref().map(|d| d.to_string()),
            None,
        ),
        DetailLevel::Full => (
            tool.title.clone(),
            tool.description.as_ref().map(|d| d.to_string()),
            Some(Value::Object((*tool.input_schema).clone())),
        ),
    };
    ToolSummary {
        name: entry.name.clone(),
        server: entry.server.clone(),
        title,
        description,
        input_schema,
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("MCP_GW_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("mcp.json"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(GatewayState::new());

    let path = config_path();
    match config::McpConfig::load(&path) {
        Ok(config) => {
            let servers = config.into_servers();
            let mut tasks = tokio::task::JoinSet::new();
            for server in servers {
                let state = state.clone();
                tasks.spawn(async move {
                    let name = server.name.clone();
                    match upstream::connect_and_register(&server, state).await {
                        Ok(()) => None,
                        Err(err) => Some(format!("server '{name}': {err:#}")),
                    }
                });
            }
            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Some(message)) => eprintln!("failed to connect {message}"),
                    Ok(None) => {}
                    Err(err) => eprintln!("connection task failed: {err}"),
                }
            }
            eprintln!("gateway ready with {} tools", state.len());
        }
        Err(err) => {
            eprintln!("warning: {err:#}; starting with an empty tool catalog");
        }
    }

    let server = GatewayServer { state };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

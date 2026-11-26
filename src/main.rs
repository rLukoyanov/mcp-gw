mod code_mode;
mod compression;
mod config;
mod metrics;
mod registry;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// discovers servers, searches a compact catalog, inspects only the tool it
/// needs, then executes it. Upstream tool definitions never all enter the
/// context at once, and servers can be connected or dropped on demand.
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
struct SearchServersParams {
    /// Free-text query matched against server name and description. Omit to list all.
    query: Option<String>,
    /// Maximum number of results to return (defaults to 20).
    limit: Option<usize>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ServerSummary {
    name: String,
    description: String,
    connected: bool,
    tool_count: usize,
}

#[derive(Serialize, schemars::JsonSchema)]
struct SearchServersResult {
    query: String,
    total: usize,
    servers: Vec<ServerSummary>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ServerActionParams {
    /// Exact server name, as returned by `search_servers`.
    name: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct ServerStatus {
    name: String,
    connected: bool,
    tool_count: usize,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExecuteCodeParams {
    /// JavaScript to run. It has access to `await callTool(name, args)` and `console.*`.
    code: String,
    /// Execution timeout in milliseconds (default 10000, max 60000).
    timeout_ms: Option<u64>,
}

#[tool_router]
impl GatewayServer {
    /// Dynamic server management: list configured servers and whether they are connected.
    #[tool(
        name = "search_servers",
        description = "List or search configured upstream MCP servers with their connection status and tool counts. Use enable_server to connect a server that is not connected."
    )]
    fn search_servers(
        &self,
        Parameters(SearchServersParams { query, limit }): Parameters<SearchServersParams>,
    ) -> Json<SearchServersResult> {
        let query = query.unwrap_or_default();
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let servers: Vec<ServerSummary> = self
            .state
            .servers(&query, limit)
            .into_iter()
            .map(|server| ServerSummary {
                name: server.name,
                description: server.description,
                connected: server.connected,
                tool_count: server.tool_count,
            })
            .collect();
        metrics::record_meta_tool("search_servers", "ok");
        Json(SearchServersResult {
            query,
            total: servers.len(),
            servers,
        })
    }

    /// Dynamic server management: connect an upstream server and load its tools.
    #[tool(
        name = "enable_server",
        description = "Connect to a configured upstream MCP server and add its tools to the catalog. Idempotent: enabling an already-connected server is a no-op."
    )]
    async fn enable_server(
        &self,
        Parameters(ServerActionParams { name }): Parameters<ServerActionParams>,
    ) -> Result<Json<ServerStatus>, ErrorData> {
        let Some(config) = self.state.config(&name) else {
            metrics::record_meta_tool("enable_server", "error");
            return Err(ErrorData::resource_not_found(
                format!("server '{name}' is not configured"),
                None,
            ));
        };

        let _guard = self.state.lifecycle().lock().await;
        if self.state.is_connected(&name) {
            metrics::record_meta_tool("enable_server", "ok");
            return Ok(Json(ServerStatus {
                name,
                connected: true,
                tool_count: self.state.tool_count_for(&config.name),
            }));
        }

        let tool_count = match upstream::enable(&config, self.state.clone()).await {
            Ok(count) => count,
            Err(err) => {
                metrics::record_meta_tool("enable_server", "error");
                return Err(ErrorData::internal_error(
                    format!("failed to enable server '{name}': {err:#}"),
                    None,
                ));
            }
        };

        metrics::record_meta_tool("enable_server", "ok");
        Ok(Json(ServerStatus {
            name,
            connected: true,
            tool_count,
        }))
    }

    /// Dynamic server management: disconnect a server and drop its tools.
    #[tool(
        name = "disable_server",
        description = "Disconnect an upstream MCP server and remove its tools from the catalog, freeing context. Use enable_server to reconnect it later."
    )]
    async fn disable_server(
        &self,
        Parameters(ServerActionParams { name }): Parameters<ServerActionParams>,
    ) -> Result<Json<ServerStatus>, ErrorData> {
        if self.state.config(&name).is_none() {
            metrics::record_meta_tool("disable_server", "error");
            return Err(ErrorData::resource_not_found(
                format!("server '{name}' is not configured"),
                None,
            ));
        }

        let _guard = self.state.lifecycle().lock().await;
        self.state.disconnect(&name);
        metrics::record_meta_tool("disable_server", "ok");
        Ok(Json(ServerStatus {
            name,
            connected: false,
            tool_count: 0,
        }))
    }

    /// Layer 1 — catalog: search available tools by keyword.
    #[tool(
        name = "search_tools",
        description = "Search the catalog of tools available through connected servers. Returns matching tool names and the upstream server that provides them, with optional descriptions and schemas."
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
        metrics::record_meta_tool("search_tools", "ok");
        let result = SearchToolsResult {
            query,
            total: matches.len(),
            tools: matches
                .into_iter()
                .map(|entry| summarize(&entry, &detail))
                .collect(),
        };
        metrics::record_inspection_tokens(metrics::count_tokens(
            &serde_json::to_string(&result).unwrap_or_default(),
        ));
        Json(result)
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
            metrics::record_meta_tool("get_tool_details", "error");
            ErrorData::resource_not_found(
                format!("tool '{name}' not found; use search_tools first"),
                None,
            )
        })?;

        let tool = &entry.tool;
        metrics::record_meta_tool("get_tool_details", "ok");
        let details = ToolDetails {
            name: entry.name,
            server: entry.server,
            title: tool.title.clone(),
            description: tool.description.as_ref().map(|d| d.to_string()),
            input_schema: Value::Object((*tool.input_schema).clone()),
            output_schema: tool
                .output_schema
                .as_ref()
                .map(|schema| Value::Object((**schema).clone())),
        };
        metrics::record_inspection_tokens(metrics::count_tokens(
            &serde_json::to_string(&details).unwrap_or_default(),
        ));
        Ok(Json(details))
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
        let started = Instant::now();

        let Some(entry) = self.state.get(&name) else {
            metrics::record_meta_tool("call_tool", "error");
            return Err(ErrorData::resource_not_found(
                format!("tool '{name}' not found; use search_tools first"),
                None,
            ));
        };

        let Some(peer) = self.state.peer(&entry.server) else {
            metrics::record_meta_tool("call_tool", "error");
            return Err(ErrorData::internal_error(
                format!("server '{}' is not connected", entry.server),
                None,
            ));
        };

        let mut request = CallToolRequestParams::new(entry.tool.name.clone());
        match arguments {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => request = request.with_arguments(map),
            Some(_) => {
                metrics::record_meta_tool("call_tool", "error");
                return Err(ErrorData::invalid_params(
                    "arguments must be a JSON object",
                    None,
                ));
            }
        }

        let result = match peer.call_tool(request).await {
            Ok(result) => result,
            Err(err) => {
                metrics::record_tool_call(
                    &name,
                    &entry.server,
                    "error",
                    started.elapsed().as_secs_f64(),
                );
                metrics::record_meta_tool("call_tool", "error");
                return Err(ErrorData::internal_error(
                    format!("upstream call failed: {err}"),
                    None,
                ));
            }
        };

        let status = if result.is_error == Some(true) {
            "error"
        } else {
            "ok"
        };
        metrics::record_tool_call(
            &name,
            &entry.server,
            status,
            started.elapsed().as_secs_f64(),
        );
        metrics::record_meta_tool("call_tool", status);

        Ok(compression::compress(&self.state, &entry.server, result).await)
    }

    /// Programmatic tool calling: run model-written JavaScript in a sandbox that
    /// composes multiple tool calls without streaming intermediate results
    /// through the model's context.
    #[tool(
        name = "execute_code",
        description = "Run JavaScript in an isolated sandbox (no filesystem or network). The code may call `await callTool(name, args)` to invoke gateway tools and `console.log(...)` for output; only the logs and the final value are returned. Use this to chain or filter tool results without passing them through context."
    )]
    async fn execute_code(
        &self,
        Parameters(ExecuteCodeParams { code, timeout_ms }): Parameters<ExecuteCodeParams>,
    ) -> CallToolResult {
        let started = Instant::now();
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(10_000).clamp(100, 60_000));
        let result = code_mode::execute(&code, self.state.clone(), timeout).await;
        let status = if result.is_error == Some(true) {
            "error"
        } else {
            "ok"
        };
        metrics::record_code_mode(status, started.elapsed().as_secs_f64());
        metrics::record_meta_tool("execute_code", status);
        result
    }
}

#[tool_handler]
impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-gw", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Progressive tool discovery gateway. Use search_servers/enable_server to control \
                 which upstream servers are connected, search_tools to find capabilities, \
                 get_tool_details to inspect a tool's schema, call_tool to invoke one tool, or \
                 execute_code to compose several calls in a sandbox."
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
    metrics::init();

    if let Some(addr) = metrics::configured_addr() {
        tokio::spawn(async move {
            if let Err(err) = metrics::serve_http(addr).await {
                eprintln!("metrics: failed to serve on {addr}: {err}");
            }
        });
    }

    let state = Arc::new(GatewayState::new());

    let path = config_path();
    match config::McpConfig::load(&path) {
        Ok(config) => {
            let (servers, compression) = config.into_parts();
            for server in &servers {
                state.register_config(server.clone());
                metrics::set_server_connected(&server.name, false);
                metrics::set_catalog_tools(&server.name, 0);
            }

            if let Some(settings) = compression {
                if servers.iter().any(|server| server.name == settings.server) {
                    state.set_compressor(Some(settings));
                } else {
                    eprintln!(
                        "compression: server '{}' is not configured; auto-compression disabled",
                        settings.server
                    );
                }
            }

            let compressor_server = state.compressor().map(|settings| settings.server);
            let auto_connect: Vec<_> = servers
                .into_iter()
                .filter(|server| {
                    server.auto_connect || Some(&server.name) == compressor_server.as_ref()
                })
                .collect();

            let mut tasks = tokio::task::JoinSet::new();
            for server in auto_connect {
                let state = state.clone();
                tasks.spawn(async move {
                    let name = server.name.clone();
                    match upstream::enable(&server, state).await {
                        Ok(_) => None,
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
    let meta_tokens: u64 = GatewayServer::tool_router()
        .list_all()
        .iter()
        .map(|tool| metrics::count_tokens(&serde_json::to_string(tool).unwrap_or_default()))
        .sum();
    metrics::set_meta_definition_tokens(meta_tokens);

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

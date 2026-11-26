mod cache;
mod code_mode;
mod compression;
mod config;
mod metrics;
mod registry;
mod tool_cache;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        tool::ToolCallContext,
        wrapper::{Json, Parameters},
    },
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ResultType, ServerCapabilities,
        ServerConfig, Tool,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use registry::{CatalogEntry, GatewayState};

/// How the gateway presents itself to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Expose seven stable meta-tools; upstream definitions are discovered on demand.
    Discovery,
    /// Expose upstream tools directly. Best for small catalogs and warm caches.
    Passthrough,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Discovery => "discovery",
            Mode::Passthrough => "passthrough",
        }
    }
}

/// In discovery mode the meta-tools implement the "progressive discovery"
/// pattern: the model first discovers servers, searches a compact catalog,
/// inspects only the tool it needs, then executes it. In passthrough mode the
/// catalog is exposed directly, matching a client connected without a gateway.
#[derive(Clone)]
struct GatewayServer {
    state: Arc<GatewayState>,
    mode: Mode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DetailLevel {
    /// Compact schema for 1-3 matches, names otherwise (default).
    #[default]
    Auto,
    /// Name and server only.
    Name,
    /// Name, title and one-line description.
    Summary,
    /// Compact input schema.
    Schema,
    /// Full input schema.
    Full,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchServersParams {
    /// Match server name or description; omit for all.
    query: Option<String>,
    /// Max results (default 20).
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
    /// Exact server name.
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
    /// Search text.
    query: String,
    /// Max results (default 10).
    limit: Option<usize>,
    /// Detail level (default auto).
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
    /// Exact tool name.
    name: Option<String>,
    /// Up to 20 exact tool names; returns `{ tools, missing }`.
    names: Option<Vec<String>>,
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
    /// Exact tool name.
    name: String,
    /// JSON object matching the tool schema.
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExecuteCodeParams {
    /// JavaScript to run.
    code: String,
    /// Timeout in ms (default 10000, max 60000).
    timeout_ms: Option<u64>,
}

/// Descriptions in `summary`/`schema` results are collapsed to one line.
const SUMMARY_MAX_CHARS: usize = 200;
/// Upper bound for a single batched `get_tool_details` call.
const MAX_BATCH_DETAILS: usize = 20;
/// Default threshold for automatic passthrough mode.
const DEFAULT_PASSTHROUGH_MAX_TOOLS: usize = 20;
/// `auto` returns compact schemas when at most this many tools match.
const AUTO_SCHEMA_MAX_RESULTS: usize = 3;
/// How many frequently used tools to mention in discovery instructions.
const HOT_TOOL_HINTS: usize = 5;

#[tool_router]
impl GatewayServer {
    /// List configured servers and whether they are connected.
    #[tool(
        name = "search_servers",
        description = "List configured upstream servers with connection status and tool counts."
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

    /// Connect a server and load its tools.
    #[tool(
        name = "enable_server",
        description = "Connect a configured upstream server and load its tools. Idempotent."
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

    /// Disconnect a server and drop its tools.
    #[tool(
        name = "disable_server",
        description = "Disconnect a server and remove its tools from the catalog."
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
        description = "Search connected tools. Default auto: compact schemas for 1-3 matches, names otherwise. detail: auto|name|summary|schema|full."
    )]
    fn search_tools(
        &self,
        Parameters(SearchToolsParams {
            query,
            limit,
            detail,
        }): Parameters<SearchToolsParams>,
    ) -> Json<SearchToolsResult> {
        let requested = detail.unwrap_or_default();
        let limit = limit.unwrap_or(10).clamp(1, 50);
        let matches = self.state.search(&query, limit);
        let detail = effective_detail(requested, matches.len());
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

    /// Layer 2 — inspect: fetch full definitions for one or several tools.
    #[tool(
        name = "get_tool_details",
        description = "Full input/output schemas for one tool (`name`) or up to 20 (`names`)."
    )]
    fn get_tool_details(
        &self,
        Parameters(GetToolDetailsParams { name, names }): Parameters<GetToolDetailsParams>,
    ) -> Result<Json<Value>, ErrorData> {
        match (name, names) {
            (Some(_), Some(_)) => {
                metrics::record_meta_tool("get_tool_details", "error");
                Err(ErrorData::invalid_params(
                    "provide either 'name' or 'names', not both",
                    None,
                ))
            }
            (Some(name), None) => {
                let Some(entry) = self.state.get(&name) else {
                    metrics::record_meta_tool("get_tool_details", "error");
                    return Err(ErrorData::resource_not_found(
                        format!("tool '{name}' not found; use search_tools first"),
                        None,
                    ));
                };
                let value = serde_json::to_value(build_details(&entry)).unwrap_or(Value::Null);
                record_inspection(&value);
                metrics::record_meta_tool("get_tool_details", "ok");
                Ok(Json(value))
            }
            (None, Some(names)) => {
                if names.is_empty() {
                    metrics::record_meta_tool("get_tool_details", "error");
                    return Err(ErrorData::invalid_params("'names' must not be empty", None));
                }
                if names.len() > MAX_BATCH_DETAILS {
                    metrics::record_meta_tool("get_tool_details", "error");
                    return Err(ErrorData::invalid_params(
                        format!("at most {MAX_BATCH_DETAILS} names per call"),
                        None,
                    ));
                }
                let mut tools = Vec::with_capacity(names.len());
                let mut missing = Vec::new();
                for name in names {
                    match self.state.get(&name) {
                        Some(entry) => tools.push(build_details(&entry)),
                        None => missing.push(name),
                    }
                }
                let value = json!({ "tools": tools, "missing": missing });
                record_inspection(&value);
                metrics::record_meta_tool("get_tool_details", "ok");
                Ok(Json(value))
            }
            (None, None) => {
                metrics::record_meta_tool("get_tool_details", "error");
                Err(ErrorData::invalid_params("provide 'name' or 'names'", None))
            }
        }
    }

    /// Layer 3 — execute: call a tool on its owning upstream server.
    #[tool(
        name = "call_tool",
        description = "Call a tool by exact name with arguments matching its schema."
    )]
    async fn call_tool(
        &self,
        Parameters(CallToolParams { name, arguments }): Parameters<CallToolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self.forward_call(&name, arguments).await;
        let status = match &result {
            Ok(result) if result.is_error != Some(true) => "ok",
            _ => "error",
        };
        metrics::record_meta_tool("call_tool", status);
        result
    }

    /// Programmatic tool calling: run model-written JavaScript in a sandbox that
    /// composes multiple tool calls without streaming intermediate results
    /// through the model's context.
    #[tool(
        name = "execute_code",
        description = "Run sandboxed JavaScript with `await callTool(name,args)` and `console.*`; returns logs and result."
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

impl GatewayServer {
    /// The seven meta-tools with compact schemas. Prose is dropped from JSON
    /// Schema while types, required fields and enums stay intact.
    fn discovery_tools() -> Vec<Tool> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(compact_tool)
            .collect()
    }

    /// Route one tool call to its owning upstream server. Shared by the
    /// `call_tool` meta-tool and passthrough mode.
    async fn forward_call(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let Some(entry) = self.state.get(name) else {
            let hint = match self.mode {
                Mode::Discovery => "; use search_tools first",
                Mode::Passthrough => "",
            };
            return Err(ErrorData::resource_not_found(
                format!("tool '{name}' not found{hint}"),
                None,
            ));
        };

        let Some(peer) = self.state.peer(&entry.server) else {
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
                return Err(ErrorData::invalid_params(
                    "arguments must be a JSON object",
                    None,
                ));
            }
        }

        let cache_args = match &request.arguments {
            Some(arguments) => Value::Object(arguments.clone()),
            None => Value::Object(Default::default()),
        };
        let cache = self.state.cache_lookup(&entry, &cache_args).await;
        if let Some(hit) = cache.hit {
            self.state.record_tool_use(&entry);
            return Ok(compression::compress(&self.state, &entry.server, hit.result).await);
        }

        let result = match peer.call_tool(request).await {
            Ok(result) => result,
            Err(err) => {
                metrics::record_tool_call(
                    name,
                    &entry.server,
                    "error",
                    started.elapsed().as_secs_f64(),
                );
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
        metrics::record_tool_call(name, &entry.server, status, started.elapsed().as_secs_f64());
        if status == "ok" {
            self.state.record_tool_use(&entry);
            if let Some(context) = cache.store {
                self.state.cache_store(&entry, context, &result);
            }
        }
        Ok(compression::compress(&self.state, &entry.server, result).await)
    }
}

#[tool_handler]
impl ServerHandler for GatewayServer {
    fn get_info(&self) -> ServerConfig {
        let mut instructions = match self.mode {
            Mode::Discovery => {
                "Progressive tool discovery gateway. Use search_servers/enable_server to control \
                 which upstream servers are connected, search_tools to find capabilities (default \
                 auto: compact schemas for 1-3 matches, names otherwise), get_tool_details to \
                 inspect one or more schemas, call_tool to invoke one tool, or execute_code to \
                 compose several calls in a sandbox."
                    .to_string()
            }
            Mode::Passthrough => {
                "All configured upstream MCP tools are exposed directly. Call them as regular MCP \
                 tools. Large results may be compressed automatically when compression is configured."
                    .to_string()
            }
        };
        if self.mode == Mode::Discovery {
            append_hot_tool_hints(self, &mut instructions);
        }
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "tokenmaxing",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = match self.mode {
            Mode::Discovery => Self::discovery_tools(),
            Mode::Passthrough => {
                self.state.set_client_peer(context.peer.clone());
                self.state.catalog_tools()
            }
        };
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match self.mode {
            Mode::Discovery => Self::discovery_tools()
                .into_iter()
                .find(|tool| tool.name.as_ref() == name),
            Mode::Passthrough => self.state.catalog_tool(name),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match self.mode {
            Mode::Discovery => {
                let tcc = ToolCallContext::new(self, request, context);
                Self::tool_router().call(tcc).await
            }
            Mode::Passthrough => {
                let name = request.name.to_string();
                let arguments = request.arguments.map(Value::Object);
                let result = self.forward_call(&name, arguments).await?;
                Ok(result.into())
            }
        }
    }
}

/// Mention tools the model has called before so repeated tasks can skip search.
fn append_hot_tool_hints(server: &GatewayServer, instructions: &mut String) {
    // Over-fetch so stale cache entries do not shrink the hint list.
    let hot = server.state.hot_tools(HOT_TOOL_HINTS * 4);
    let mut present = Vec::new();
    for hot in hot {
        if present.len() >= HOT_TOOL_HINTS {
            break;
        }
        if let Some(entry) = server.state.get(&hot.name) {
            if entry.server != hot.server {
                continue;
            }
            let description = entry
                .tool
                .description
                .as_ref()
                .map(|description| description.to_string())
                .unwrap_or(hot.description);
            present.push((hot.name, description));
        } else if server.state.config(&hot.server).is_some()
            && !server.state.is_connected(&hot.server)
        {
            present.push((
                hot.name,
                format!("{} (enable server '{}')", hot.description, hot.server),
            ));
        }
    }
    if present.is_empty() {
        return;
    }
    instructions.push_str("\n\nFrequently used tools (call directly):");
    for (name, description) in present {
        instructions.push_str(&format!("\n- {name}: {}", one_line(&description, 120)));
    }
}

fn compact_tool(mut tool: Tool) -> Tool {
    if let Value::Object(compact) = compact_schema(&Value::Object((*tool.input_schema).clone())) {
        tool.input_schema = std::sync::Arc::new(compact);
    }
    tool
}

fn effective_detail(requested: DetailLevel, matches: usize) -> DetailLevel {
    match requested {
        DetailLevel::Auto if matches <= AUTO_SCHEMA_MAX_RESULTS => DetailLevel::Schema,
        DetailLevel::Auto => DetailLevel::Name,
        other => other,
    }
}

fn summarize(entry: &CatalogEntry, detail: &DetailLevel) -> ToolSummary {
    let tool = &entry.tool;
    let description = tool.description.as_ref().map(|d| d.to_string());
    let (title, description, input_schema) = match detail {
        DetailLevel::Auto | DetailLevel::Name => (None, None, None),
        DetailLevel::Summary => (
            tool.title.clone(),
            description.map(|text| one_line(&text, SUMMARY_MAX_CHARS)),
            None,
        ),
        DetailLevel::Schema => (
            tool.title.clone(),
            description.map(|text| one_line(&text, SUMMARY_MAX_CHARS)),
            Some(compact_schema(&Value::Object((*tool.input_schema).clone()))),
        ),
        DetailLevel::Full => (
            tool.title.clone(),
            description,
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

fn build_details(entry: &CatalogEntry) -> ToolDetails {
    let tool = &entry.tool;
    ToolDetails {
        name: entry.name.clone(),
        server: entry.server.clone(),
        title: tool.title.clone(),
        description: tool.description.as_ref().map(|d| d.to_string()),
        input_schema: Value::Object((*tool.input_schema).clone()),
        output_schema: tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object((**schema).clone())),
    }
}

fn record_inspection(value: &Value) {
    metrics::record_inspection_tokens(metrics::count_tokens(&value.to_string()));
}

/// Collapse whitespace and truncate to `max_chars` characters.
fn one_line(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut truncated: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    truncated.push('…');
    truncated
}

/// Keep schema structure and constraints but drop prose that is expensive in
/// tokens: descriptions, examples, defaults and JSON Schema metadata.
fn compact_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut compact = serde_json::Map::new();
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "$schema"
                        | "$id"
                        | "title"
                        | "description"
                        | "examples"
                        | "default"
                        | "deprecated"
                        | "readOnly"
                        | "writeOnly"
                        | "externalDocs"
                ) {
                    continue;
                }
                let value = match key.as_str() {
                    "properties" => match value {
                        Value::Object(properties) => Value::Object(
                            properties
                                .iter()
                                .map(|(name, schema)| (name.clone(), compact_schema(schema)))
                                .collect(),
                        ),
                        other => other.clone(),
                    },
                    _ => compact_schema(value),
                };
                compact.insert(key.clone(), value);
            }
            Value::Object(compact)
        }
        Value::Array(items) => Value::Array(items.iter().map(compact_schema).collect()),
        other => other.clone(),
    }
}

fn config_path() -> PathBuf {
    std::env::var_os("TOKENMAXING_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("mcp.json"))
}

/// `TOKENMAXING_MODE`; `auto` (or unset) lets the gateway choose.
fn mode_from_env() -> Option<Mode> {
    match std::env::var("TOKENMAXING_MODE").ok().as_deref() {
        None | Some("") | Some("auto") => None,
        Some("discovery") => Some(Mode::Discovery),
        Some("passthrough") => Some(Mode::Passthrough),
        Some(other) => {
            eprintln!("mode: unknown TOKENMAXING_MODE '{other}', using auto");
            None
        }
    }
}

fn passthrough_max_tools() -> usize {
    match std::env::var("TOKENMAXING_PASSTHROUGH_MAX_TOOLS") {
        Err(_) => DEFAULT_PASSTHROUGH_MAX_TOOLS,
        Ok(value) => match value.parse::<usize>() {
            Ok(parsed) => parsed.clamp(1, 10_000),
            Err(err) => {
                eprintln!(
                    "mode: invalid TOKENMAXING_PASSTHROUGH_MAX_TOOLS '{value}': {err}; \
                     using {DEFAULT_PASSTHROUGH_MAX_TOOLS}"
                );
                DEFAULT_PASSTHROUGH_MAX_TOOLS
            }
        },
    }
}

/// Automatic mode only chooses passthrough for a small catalog where every
/// configured server is already connected. Otherwise discovery remains safe.
fn choose_mode(
    explicit: Option<Mode>,
    all_connected: bool,
    catalog_len: usize,
    max_tools: usize,
) -> Mode {
    explicit.unwrap_or(
        if all_connected && catalog_len > 0 && catalog_len <= max_tools {
            Mode::Passthrough
        } else {
            Mode::Discovery
        },
    )
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
    state.set_usage(cache::UsageCache::from_env(&path));
    let mode = match config::McpConfig::load(&path) {
        Ok(config) => {
            let (servers, compression, tool_cache) = config.into_parts();
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

            if let Some(settings) = tool_cache
                && settings.enabled
            {
                match tool_cache::ToolCache::from_settings(settings, &path) {
                    Ok(cache) => state.set_tool_cache(Some(cache)),
                    Err(err) => eprintln!("tool cache: {err:#}; disabled"),
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
            let all_connected = state.all_configured_connected();
            let max_tools = passthrough_max_tools();
            let mode = choose_mode(mode_from_env(), all_connected, state.len(), max_tools);
            eprintln!(
                "gateway ready with {} tools (mode: {}, passthrough threshold: {})",
                state.len(),
                mode.as_str(),
                max_tools
            );
            if mode == Mode::Passthrough && !all_connected {
                eprintln!(
                    "mode: passthrough exposes only connected servers; \
                     some configured servers are disconnected"
                );
            }
            mode
        }
        Err(err) => {
            eprintln!("warning: {err:#}; starting with an empty tool catalog");
            Mode::Discovery
        }
    };

    state.set_passthrough(mode == Mode::Passthrough);
    let server = GatewayServer { state, mode };
    if mode == Mode::Discovery {
        let meta_tokens: u64 = GatewayServer::discovery_tools()
            .iter()
            .map(|tool| metrics::count_tokens(&serde_json::to_string(tool).unwrap_or_default()))
            .sum();
        metrics::set_meta_definition_tokens(meta_tokens);
    } else {
        metrics::set_meta_definition_tokens(0);
    }

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("  hello\n\n  world  ", 40), "hello world");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }

    #[test]
    fn compact_schema_keeps_structure_and_drops_prose() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "title": "Example",
            "description": "a long description",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "where to read",
                    "examples": ["/tmp/x"]
                },
                "flag": { "type": "boolean", "default": false }
            },
            "required": ["path"],
            "additionalProperties": false
        });

        let compact = compact_schema(&schema);
        assert!(compact.get("$schema").is_none());
        assert!(compact.get("title").is_none());
        assert!(compact.get("description").is_none());
        assert!(compact["properties"]["path"].get("description").is_none());
        assert!(compact["properties"]["path"].get("examples").is_none());
        assert!(compact["properties"]["flag"].get("default").is_none());
        assert_eq!(compact["properties"]["path"]["type"], "string");
        assert_eq!(compact["required"], serde_json::json!(["path"]));
        assert_eq!(compact["additionalProperties"], false);
    }

    #[test]
    fn auto_detail_uses_schemas_for_few_matches() {
        assert_eq!(effective_detail(DetailLevel::Auto, 1), DetailLevel::Schema);
        assert_eq!(effective_detail(DetailLevel::Auto, 3), DetailLevel::Schema);
        assert_eq!(effective_detail(DetailLevel::Auto, 4), DetailLevel::Name);
        assert_eq!(
            effective_detail(DetailLevel::Summary, 1),
            DetailLevel::Summary
        );
    }

    #[test]
    fn auto_mode_requires_a_small_fully_connected_catalog() {
        assert_eq!(choose_mode(None, true, 10, 20), Mode::Passthrough);
        assert_eq!(choose_mode(None, true, 16, 20), Mode::Passthrough);
        assert_eq!(choose_mode(None, true, 21, 20), Mode::Discovery);
        assert_eq!(choose_mode(None, false, 10, 20), Mode::Discovery);
        assert_eq!(choose_mode(None, true, 0, 20), Mode::Discovery);
        assert_eq!(
            choose_mode(Some(Mode::Discovery), true, 3, 20),
            Mode::Discovery
        );
        assert_eq!(
            choose_mode(Some(Mode::Passthrough), false, 3, 20),
            Mode::Passthrough
        );
    }
}

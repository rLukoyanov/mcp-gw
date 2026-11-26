use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, RwLock};

use rmcp::RoleServer;
use rmcp::model::Tool;
use rmcp::service::RunningServiceCancellationToken;
use rmcp::service::{Peer, RoleClient};
use tokio::sync::Mutex;

use crate::cache::{HotTool, UsageCache};
use crate::config::{CompressionSettings, ServerConfig};
use crate::metrics;

/// One tool aggregated from an upstream MCP server.
#[derive(Clone)]
pub struct CatalogEntry {
    /// Upstream server that owns the tool.
    pub server: String,
    /// Name exposed to the model. Equal to the upstream name unless it would
    /// collide with a tool from another server, in which case it is namespaced.
    pub name: String,
    /// Original tool definition as reported by the upstream server.
    pub tool: Tool,
}

/// A configured server plus its current connection state, as returned by
/// `search_servers`.
#[derive(Clone)]
pub struct ServerEntry {
    pub name: String,
    pub description: String,
    pub connected: bool,
    pub tool_count: usize,
}

/// Shared state: the registry of configured servers, the aggregated catalog,
/// and the live connection to each enabled upstream server. Cached host-side so
/// re-injecting a definition never needs another `tools/list` round trip.
#[derive(Default)]
pub struct GatewayState {
    configs: RwLock<HashMap<String, ServerConfig>>,
    catalog: RwLock<Vec<CatalogEntry>>,
    peers: RwLock<HashMap<String, Peer<RoleClient>>>,
    tokens: RwLock<HashMap<String, RunningServiceCancellationToken>>,
    /// When set, large tool results are compressed through this server.
    compressor: RwLock<Option<CompressionSettings>>,
    /// Serializes `enable_server` / `disable_server` so concurrent requests
    /// cannot open or tear down the same connection twice.
    lifecycle: Mutex<()>,
    /// Passthrough mode exposes catalog tools directly; when enabled, changes
    /// to the catalog must be announced to the connected client.
    passthrough: AtomicBool,
    /// The downstream client, learned from the first `tools/list` request.
    client_peer: RwLock<Option<Peer<RoleServer>>>,
    /// Tools the model has called, persisted across sessions.
    usage: StdMutex<UsageCache>,
}

impl GatewayState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lifecycle(&self) -> &Mutex<()> {
        &self.lifecycle
    }

    pub fn set_compressor(&self, settings: Option<CompressionSettings>) {
        *self.compressor.write().unwrap() = settings;
    }

    pub fn compressor(&self) -> Option<CompressionSettings> {
        self.compressor.read().unwrap().clone()
    }

    pub fn register_config(&self, config: ServerConfig) {
        self.configs
            .write()
            .unwrap()
            .insert(config.name.clone(), config);
    }

    pub fn config(&self, name: &str) -> Option<ServerConfig> {
        self.configs.read().unwrap().get(name).cloned()
    }

    pub fn is_connected(&self, name: &str) -> bool {
        self.peers
            .read()
            .unwrap()
            .get(name)
            .is_some_and(|peer| !peer.is_transport_closed())
    }

    pub fn set_connected(
        &self,
        server: &str,
        peer: Peer<RoleClient>,
        token: RunningServiceCancellationToken,
    ) {
        self.peers.write().unwrap().insert(server.to_string(), peer);
        self.tokens
            .write()
            .unwrap()
            .insert(server.to_string(), token);
        metrics::set_server_connected(server, true);
        metrics::set_catalog_tools(server, 0);
        metrics::set_server_definition_tokens(server, 0);
    }

    pub fn peer(&self, server: &str) -> Option<Peer<RoleClient>> {
        self.peers.read().unwrap().get(server).cloned()
    }

    /// Whether every configured server currently has a live connection.
    pub fn all_configured_connected(&self) -> bool {
        let configs = self.configs.read().unwrap();
        let peers = self.peers.read().unwrap();
        configs.keys().all(|name| {
            peers
                .get(name)
                .is_some_and(|peer| !peer.is_transport_closed())
        })
    }

    pub fn set_passthrough(&self, enabled: bool) {
        self.passthrough.store(enabled, Ordering::Relaxed);
    }

    pub fn set_usage(&self, cache: UsageCache) {
        *self.usage.lock().unwrap() = cache;
    }

    /// Remember a successful tool call for ranking and instruction hints.
    pub fn record_tool_use(&self, entry: &CatalogEntry) {
        self.usage.lock().unwrap().record(entry);
    }

    pub fn hot_tools(&self, limit: usize) -> Vec<HotTool> {
        self.usage.lock().unwrap().hot(limit)
    }

    /// Remember the downstream client so upstream tool changes can be relayed.
    pub fn set_client_peer(&self, peer: Peer<RoleServer>) {
        *self.client_peer.write().unwrap() = Some(peer);
    }

    /// Notify the client that `tools/list` changed. Only meaningful in
    /// passthrough mode, where the exposed tool set follows the catalog.
    fn notify_tools_changed(&self) {
        if !self.passthrough.load(Ordering::Relaxed) {
            return;
        }
        let peer = self.client_peer.read().unwrap().clone();
        if let Some(peer) = peer {
            tokio::spawn(async move {
                if let Err(err) = peer.notify_tool_list_changed().await {
                    eprintln!("failed to notify client about tool list change: {err}");
                }
            });
        }
    }

    /// Tear down a server: cancel its connection and drop its tools.
    /// Returns whether a connection was actually removed.
    pub fn disconnect(&self, server: &str) -> bool {
        self.catalog
            .write()
            .unwrap()
            .retain(|entry| entry.server != server);
        let removed_peer = self.peers.write().unwrap().remove(server).is_some();
        if let Some(token) = self.tokens.write().unwrap().remove(server) {
            token.cancel();
        }
        metrics::set_server_connected(server, false);
        metrics::set_catalog_tools(server, 0);
        metrics::set_server_definition_tokens(server, 0);
        self.notify_tools_changed();
        removed_peer
    }

    /// Replace every tool previously contributed by `server`. Called after
    /// enabling and again on `notifications/tools/list_changed`.
    pub fn replace_server_tools(&self, server: &str, tools: Vec<Tool>) {
        let mut catalog = self.catalog.write().unwrap();
        catalog.retain(|entry| entry.server != server);

        // The compressor is invoked by the gateway automatically, so hide its
        // compress tool from the model to avoid double compression.
        let hidden_tool = self.compressor().and_then(|settings| {
            (settings.enabled && settings.server == server).then_some(settings.compress_tool)
        });

        for tool in tools {
            if hidden_tool.as_deref() == Some(tool.name.as_ref()) {
                continue;
            }
            let base = tool.name.to_string();
            let mut name = base.clone();
            if catalog.iter().any(|entry| entry.name == name) {
                name = format!("{server}__{base}");
            }
            let mut suffix = 2;
            while catalog.iter().any(|entry| entry.name == name) {
                name = format!("{server}__{base}_{suffix}");
                suffix += 1;
            }
            catalog.push(CatalogEntry {
                server: server.to_string(),
                name,
                tool,
            });
        }

        let count = catalog
            .iter()
            .filter(|entry| entry.server == server)
            .count();
        metrics::set_catalog_tools(server, count);

        let tokens: u64 = catalog
            .iter()
            .filter(|entry| entry.server == server)
            .map(|entry| {
                let json = serde_json::to_string(&entry.tool).unwrap_or_default();
                metrics::count_tokens(&json)
            })
            .sum();
        metrics::set_server_definition_tokens(server, tokens);
        drop(catalog);
        self.notify_tools_changed();
    }

    /// Tool definitions as the model should see them, with collision-safe names.
    pub fn catalog_tools(&self) -> Vec<Tool> {
        let catalog = self.catalog.read().unwrap();
        let mut tools: Vec<Tool> = catalog.iter().map(exposed_tool).collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// One catalog tool with its exposed (possibly namespaced) name.
    pub fn catalog_tool(&self, name: &str) -> Option<Tool> {
        self.get(name).as_ref().map(exposed_tool)
    }

    /// Configured servers matching `query` (by name or description), with their
    /// live connection status and tool counts.
    pub fn servers(&self, query: &str, limit: usize) -> Vec<ServerEntry> {
        let query = query.trim().to_lowercase();
        let configs = self.configs.read().unwrap();
        let catalog = self.catalog.read().unwrap();
        let peers = self.peers.read().unwrap();

        let mut entries: Vec<ServerEntry> = configs
            .values()
            .filter(|config| {
                query.is_empty()
                    || config.name.to_lowercase().contains(&query)
                    || config.description.to_lowercase().contains(&query)
            })
            .map(|config| ServerEntry {
                name: config.name.clone(),
                description: config.description.clone(),
                connected: peers
                    .get(&config.name)
                    .is_some_and(|peer| !peer.is_transport_closed()),
                tool_count: catalog
                    .iter()
                    .filter(|entry| entry.server == config.name)
                    .count(),
            })
            .collect();

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.truncate(limit);
        entries
    }

    /// Case-insensitive keyword search over name, title, description and server.
    /// Ordered by relevance, highest first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<CatalogEntry> {
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let catalog = self.catalog.read().unwrap();
        let usage = self.usage.lock().unwrap().counts();
        let mut scored: Vec<(u32, &CatalogEntry)> = catalog
            .iter()
            .filter_map(|entry| {
                let base = score_entry(entry, &tokens);
                if base == 0 {
                    return None;
                }
                // Nudge frequently used tools up without surfacing unrelated ones.
                let boost = usage.get(&entry.name).copied().unwrap_or(0).min(20) as u32;
                Some((base + boost, entry))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<CatalogEntry> {
        self.catalog
            .read()
            .unwrap()
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
    }

    pub fn tool_count_for(&self, server: &str) -> usize {
        self.catalog
            .read()
            .unwrap()
            .iter()
            .filter(|entry| entry.server == server)
            .count()
    }

    pub fn len(&self) -> usize {
        self.catalog.read().unwrap().len()
    }
}

/// A clone of the upstream definition under the name exposed to the model.
fn exposed_tool(entry: &CatalogEntry) -> Tool {
    let mut tool = entry.tool.clone();
    if tool.name != entry.name {
        tool.name = Cow::Owned(entry.name.clone());
    }
    tool
}

fn score_entry(entry: &CatalogEntry, tokens: &[String]) -> u32 {
    let name = entry.name.to_lowercase();
    let title = entry.tool.title.as_deref().unwrap_or("").to_lowercase();
    let description = entry
        .tool
        .description
        .as_deref()
        .unwrap_or("")
        .to_lowercase();
    let server = entry.server.to_lowercase();

    tokens.iter().fold(0, |acc, token| {
        let mut score = 0;
        if name == *token {
            score += 100;
        } else if name.contains(token) {
            score += 40;
        }
        if server.contains(token) {
            score += 25;
        }
        if title.contains(token) {
            score += 10;
        }
        if description.contains(token) {
            score += 5;
        }
        acc + score
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::JsonObject;

    fn tool(name: &'static str, description: &'static str) -> Tool {
        Tool::new(name, description, JsonObject::new())
    }

    #[test]
    fn replaces_only_the_target_server() {
        let state = GatewayState::new();
        state.replace_server_tools("a", vec![tool("one", "first")]);
        state.replace_server_tools("b", vec![tool("two", "second")]);
        state.replace_server_tools("a", vec![tool("three", "third")]);

        assert!(state.get("one").is_none());
        assert_eq!(state.get("two").unwrap().server, "b");
        assert_eq!(state.get("three").unwrap().server, "a");
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn namespaces_colliding_tool_names() {
        let state = GatewayState::new();
        state.replace_server_tools("a", vec![tool("search", "a search")]);
        state.replace_server_tools("b", vec![tool("search", "b search")]);

        assert_eq!(state.get("search").unwrap().server, "a");
        let namespaced = state.get("b__search").unwrap();
        assert_eq!(namespaced.server, "b");
        assert_eq!(namespaced.tool.name, "search");
    }

    #[test]
    fn search_ranks_name_matches_first() {
        let state = GatewayState::new();
        state.replace_server_tools(
            "demo",
            vec![
                tool("fetch_url", "reads a url"),
                tool("other", "fetch a value"),
            ],
        );

        let results = state.search("fetch", 10);
        assert_eq!(results[0].name, "fetch_url");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_limit_is_respected() {
        let state = GatewayState::new();
        state.replace_server_tools(
            "many",
            vec![tool("a1", "x"), tool("a2", "x"), tool("a3", "x")],
        );
        assert_eq!(state.search("a", 2).len(), 2);
    }

    #[test]
    fn servers_are_reported_with_status_and_tool_counts() {
        let state = GatewayState::new();
        let config = ServerConfig {
            name: "demo".into(),
            description: "demo tools".into(),
            auto_connect: true,
            transport: crate::config::Transport::Stdio {
                command: "true".into(),
                args: vec![],
                env: HashMap::new(),
            },
        };
        state.register_config(config);
        state.replace_server_tools("demo", vec![tool("ping", "returns pong")]);

        let servers = state.servers("", 10);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "demo");
        assert!(!servers[0].connected);
        assert_eq!(servers[0].tool_count, 1);

        assert_eq!(state.servers("demo", 10).len(), 1);
        assert!(state.servers("nothing", 10).is_empty());
    }

    #[test]
    fn usage_boosts_search_ranking() {
        let state = GatewayState::new();
        state.replace_server_tools(
            "demo",
            vec![
                tool("alpha", "fetch a value"),
                tool("beta", "fetch a value"),
            ],
        );
        let entry = state.get("beta").unwrap();
        for _ in 0..10 {
            state.record_tool_use(&entry);
        }

        let results = state.search("fetch", 10);
        assert_eq!(results[0].name, "beta");
    }

    #[test]
    fn catalog_tools_use_collision_safe_names() {
        let state = GatewayState::new();
        state.replace_server_tools("a", vec![tool("search", "a search")]);
        state.replace_server_tools("b", vec![tool("search", "b search")]);

        let names: Vec<String> = state
            .catalog_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(names.contains(&"search".to_string()));
        assert!(names.contains(&"b__search".to_string()));
        assert_eq!(
            state
                .catalog_tool("b__search")
                .map(|tool| tool.name.to_string()),
            Some("b__search".to_string())
        );
    }

    #[test]
    fn hides_the_compressor_tool_from_the_catalog() {
        let state = GatewayState::new();
        state.set_compressor(Some(CompressionSettings {
            server: "headroom".into(),
            min_bytes: 512,
            enabled: true,
            compress_tool: "headroom_compress".into(),
            retrieve_tool: "headroom_retrieve".into(),
        }));
        state.replace_server_tools(
            "headroom",
            vec![
                tool("headroom_compress", "compress"),
                tool("headroom_retrieve", "retrieve"),
            ],
        );

        assert!(state.get("headroom_compress").is_none());
        assert!(state.get("headroom_retrieve").is_some());
    }
}

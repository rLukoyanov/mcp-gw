use std::collections::HashMap;
use std::sync::RwLock;

use rmcp::model::Tool;
use rmcp::service::RunningServiceCancellationToken;
use rmcp::service::{Peer, RoleClient};
use tokio::sync::Mutex;

use crate::config::ServerConfig;

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
    /// Serializes `enable_server` / `disable_server` so concurrent requests
    /// cannot open or tear down the same connection twice.
    lifecycle: Mutex<()>,
}

impl GatewayState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lifecycle(&self) -> &Mutex<()> {
        &self.lifecycle
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
    }

    pub fn peer(&self, server: &str) -> Option<Peer<RoleClient>> {
        self.peers.read().unwrap().get(server).cloned()
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
        removed_peer
    }

    /// Replace every tool previously contributed by `server`. Called after
    /// enabling and again on `notifications/tools/list_changed`.
    pub fn replace_server_tools(&self, server: &str, tools: Vec<Tool>) {
        let mut catalog = self.catalog.write().unwrap();
        catalog.retain(|entry| entry.server != server);

        for tool in tools {
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
        let mut scored: Vec<(u32, &CatalogEntry)> = catalog
            .iter()
            .filter_map(|entry| {
                let score = score_entry(entry, &tokens);
                (score > 0).then_some((score, entry))
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
}

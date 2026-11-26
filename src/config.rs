use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

/// Configuration file format (Claude Desktop compatible, with two extensions):
///
/// ```json
/// {
///   "compression": {
///     "server": "headroom",
///     "minBytes": 512
///   },
///   "mcpServers": {
///     "local": {
///       "command": "npx",
///       "args": ["-y", "some-server"],
///       "env": { "KEY": "value" },
///       "description": "What this server provides",
///       "autoConnect": false
///     },
///     "remote": {
///       "url": "https://example.com/mcp",
///       "headers": { "Authorization": "Bearer ..." }
///     }
///   }
/// }
/// ```
///
/// `description` feeds `search_servers`; `autoConnect` (default `true`) controls
/// whether the gateway connects at startup or only when the model calls
/// `enable_server`.
///
/// The optional `compression` block routes every large `call_tool` result
/// through the named upstream server's `compressTool` (Headroom by default)
/// before it reaches the model.
#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: HashMap<String, RawServer>,
    #[serde(default)]
    pub compression: Option<CompressionSettings>,
    #[serde(rename = "toolCache", default)]
    pub tool_cache: Option<ToolCacheSettings>,
}

/// Auto-compression via an upstream MCP server (Headroom).
#[derive(Debug, Clone, Deserialize)]
pub struct CompressionSettings {
    /// Name of the configured server that exposes the compression tools.
    pub server: String,
    /// Only results at least this many bytes are compressed. Defaults to 512.
    #[serde(rename = "minBytes", default = "default_min_bytes")]
    pub min_bytes: usize,
    /// Disable compression without removing the config block.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool that compresses content. Defaults to `headroom_compress`.
    #[serde(rename = "compressTool", default = "default_compress_tool")]
    pub compress_tool: String,
    /// Tool named in the hint so the model can recover the original.
    #[serde(rename = "retrieveTool", default = "default_retrieve_tool")]
    pub retrieve_tool: String,
}

fn default_min_bytes() -> usize {
    512
}

fn default_compress_tool() -> String {
    "headroom_compress".to_string()
}

fn default_retrieve_tool() -> String {
    "headroom_retrieve".to_string()
}

/// Optional cache for MCP tool results. Disabled by default and restricted to
/// an explicit allowlist, because tool calls can have side effects.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCacheSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ToolCacheMode,
    /// Cache entry lifetime in seconds.
    #[serde(rename = "ttlSeconds", default = "default_tool_cache_ttl")]
    pub ttl_seconds: u64,
    /// Cosine similarity required for a semantic hit.
    #[serde(default = "default_tool_cache_threshold")]
    pub threshold: f32,
    #[serde(rename = "maxEntries", default = "default_tool_cache_entries")]
    pub max_entries: usize,
    #[serde(rename = "maxResultBytes", default = "default_tool_cache_result_bytes")]
    pub max_result_bytes: usize,
    /// Exposed tool names to cache. Supports a trailing `*` wildcard.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub embeddings: Option<EmbeddingSettings>,
    /// Cache file path; defaults to the user cache directory.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolCacheMode {
    /// Exact canonical arguments only.
    #[default]
    Exact,
    /// Exact first, then embedding similarity within the same tool.
    Semantic,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingSettings {
    pub provider: EmbeddingProvider,
    /// Model name for OpenAI-compatible and Ollama providers.
    #[serde(default)]
    pub model: String,
    /// Base URL (OpenAI) or Ollama base URL.
    #[serde(default)]
    pub url: Option<String>,
    /// API key; `TOKENMAXING_EMBEDDINGS_API_KEY` is used as a fallback.
    #[serde(rename = "apiKey", default)]
    pub api_key: Option<String>,
    /// Dimensions for the local `hash` provider.
    #[serde(default = "default_hash_dimensions")]
    pub dimensions: usize,
    #[serde(rename = "timeoutMs", default = "default_embedding_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProvider {
    Openai,
    Ollama,
    /// Deterministic token-hash embedding for tests/offline; lexical, not semantic.
    Hash,
}

fn default_tool_cache_ttl() -> u64 {
    300
}

fn default_tool_cache_threshold() -> f32 {
    0.9
}

fn default_tool_cache_entries() -> usize {
    200
}

fn default_tool_cache_result_bytes() -> usize {
    65_536
}

fn default_hash_dimensions() -> usize {
    256
}

fn default_embedding_timeout_ms() -> u64 {
    5_000
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawServer {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Human-readable description used by `search_servers`.
    #[serde(default)]
    pub description: String,
    /// Connect at startup. Defaults to `true`; set `false` for on-demand
    /// servers that the model should `enable_server` explicitly.
    #[serde(rename = "autoConnect", default = "default_true")]
    pub auto_connect: bool,
}

fn default_true() -> bool {
    true
}

impl McpConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Convert raw entries into validated server configs, logging and skipping
    /// any entry that does not describe exactly one transport.
    pub fn into_parts(
        self,
    ) -> (
        Vec<ServerConfig>,
        Option<CompressionSettings>,
        Option<ToolCacheSettings>,
    ) {
        let compression = self.compression;
        let tool_cache = self.tool_cache;
        let servers = self
            .servers
            .into_iter()
            .filter_map(
                |(name, raw)| match ServerConfig::from_raw(name.clone(), raw) {
                    Ok(server) => Some(server),
                    Err(err) => {
                        eprintln!("skipping server '{name}': {err}");
                        None
                    }
                },
            )
            .collect();
        (servers, compression, tool_cache)
    }

    /// Convenience for tests that do not care about compression settings.
    #[cfg(test)]
    pub fn into_servers(self) -> Vec<ServerConfig> {
        self.into_parts().0
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub description: String,
    pub auto_connect: bool,
    pub transport: Transport,
}

#[derive(Debug, Clone)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

impl ServerConfig {
    fn from_raw(name: String, raw: RawServer) -> anyhow::Result<Self> {
        let transport = match (raw.command, raw.url) {
            (Some(command), None) => Transport::Stdio {
                command,
                args: raw.args,
                env: raw.env,
            },
            (None, Some(url)) => Transport::Http {
                url,
                headers: raw.headers,
            },
            (Some(_), Some(_)) => {
                bail!("must specify either 'command' (stdio) or 'url' (http), not both")
            }
            (None, None) => bail!("must specify either 'command' (stdio) or 'url' (http)"),
        };
        Ok(Self {
            name,
            description: raw.description,
            auto_connect: raw.auto_connect,
            transport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> McpConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_stdio_and_http_servers() {
        let config = parse(
            r#"{
                "mcpServers": {
                    "local": { "command": "node", "args": ["server.js"], "env": { "TOKEN": "x" } },
                    "remote": { "url": "https://example.com/mcp", "headers": { "Auth": "y" } }
                }
            }"#,
        );
        let servers = config.into_servers();
        assert_eq!(servers.len(), 2);

        let local = servers.iter().find(|s| s.name == "local").unwrap();
        assert!(matches!(&local.transport, Transport::Stdio { command, .. } if command == "node"));

        let remote = servers.iter().find(|s| s.name == "remote").unwrap();
        assert!(
            matches!(&remote.transport, Transport::Http { url, .. } if url == "https://example.com/mcp")
        );
    }

    #[test]
    fn skips_entries_without_a_transport() {
        let config = parse(r#"{ "mcpServers": { "broken": {} } }"#);
        assert!(config.into_servers().is_empty());
    }

    #[test]
    fn rejects_entries_with_both_transports() {
        let config =
            parse(r#"{ "mcpServers": { "x": { "command": "a", "url": "https://e/mcp" } } }"#);
        assert!(config.into_servers().is_empty());
    }

    #[test]
    fn parses_compression_settings_with_defaults() {
        let config = parse(
            r#"{
                "compression": { "server": "headroom" },
                "mcpServers": {}
            }"#,
        );
        let (_, compression, _) = config.into_parts();
        let compression = compression.expect("compression block");
        assert_eq!(compression.server, "headroom");
        assert_eq!(compression.min_bytes, 512);
        assert!(compression.enabled);
        assert_eq!(compression.compress_tool, "headroom_compress");
        assert_eq!(compression.retrieve_tool, "headroom_retrieve");
    }

    #[test]
    fn parses_compression_overrides() {
        let config = parse(
            r#"{
                "compression": {
                    "server": "hr",
                    "minBytes": 10,
                    "enabled": false,
                    "compressTool": "c",
                    "retrieveTool": "r"
                },
                "mcpServers": {}
            }"#,
        );
        let (_, compression, _) = config.into_parts();
        let compression = compression.expect("compression block");
        assert_eq!(compression.min_bytes, 10);
        assert!(!compression.enabled);
        assert_eq!(compression.compress_tool, "c");
        assert_eq!(compression.retrieve_tool, "r");
    }

    #[test]
    fn parses_tool_cache_defaults() {
        let config = parse(
            r#"{
                "toolCache": {
                    "enabled": true,
                    "tools": ["filesystem_read_text_file"]
                },
                "mcpServers": {}
            }"#,
        );
        let (_, _, cache) = config.into_parts();
        let cache = cache.expect("tool cache block");
        assert!(cache.enabled);
        assert_eq!(cache.mode, ToolCacheMode::Exact);
        assert_eq!(cache.ttl_seconds, 300);
        assert_eq!(cache.max_entries, 200);
        assert_eq!(cache.max_result_bytes, 65_536);
        assert_eq!(cache.tools, vec!["filesystem_read_text_file"]);
        assert!(cache.embeddings.is_none());
    }

    #[test]
    fn parses_semantic_tool_cache() {
        let config = parse(
            r#"{
                "toolCache": {
                    "enabled": true,
                    "mode": "semantic",
                    "threshold": 0.8,
                    "tools": ["*"],
                    "embeddings": {
                        "provider": "ollama",
                        "model": "nomic-embed-text",
                        "url": "http://localhost:11434"
                    }
                },
                "mcpServers": {}
            }"#,
        );
        let (_, _, cache) = config.into_parts();
        let cache = cache.expect("tool cache block");
        assert_eq!(cache.mode, ToolCacheMode::Semantic);
        assert_eq!(cache.threshold, 0.8);
        let embeddings = cache.embeddings.expect("embeddings block");
        assert_eq!(embeddings.provider, EmbeddingProvider::Ollama);
        assert_eq!(embeddings.model, "nomic-embed-text");
        assert_eq!(embeddings.timeout_ms, 5_000);
    }
}

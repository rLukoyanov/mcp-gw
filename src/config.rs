use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

/// Configuration file format (Claude Desktop compatible, with two extensions):
///
/// ```json
/// {
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
#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: HashMap<String, RawServer>,
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
    pub fn into_servers(self) -> Vec<ServerConfig> {
        self.servers
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
            .collect()
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
}

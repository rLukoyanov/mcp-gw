use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::registry::CatalogEntry;

const VERSION: u32 = 1;
const MAX_ENTRIES: usize = 2000;

/// One tool the model has actually called, kept across sessions so search can
/// rank familiar tools first and instructions can hint at them.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedTool {
    server: String,
    calls: u64,
    last_used: u64,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    #[serde(default)]
    tools: HashMap<String, CachedTool>,
}

/// A tool worth mentioning in the gateway instructions.
#[derive(Debug, Clone)]
pub struct HotTool {
    pub name: String,
    pub server: String,
    pub description: String,
}

/// Persistent usage cache. Only tool names, servers, call counts and one-line
/// descriptions are stored; arguments and results never touch the disk.
#[derive(Debug, Default)]
pub struct UsageCache {
    path: Option<PathBuf>,
    tools: HashMap<String, CachedTool>,
}

impl UsageCache {
    /// Load the cache for a config file. `TOKENMAXING_CACHE` overrides the
    /// path; `off` or an empty value disables persistence.
    pub fn from_env(config_path: &Path) -> Self {
        let path = match std::env::var("TOKENMAXING_CACHE").ok().as_deref() {
            Some("off") | Some("") => None,
            Some(value) => Some(PathBuf::from(value)),
            None => default_path(config_path),
        };
        let mut cache = Self {
            path,
            tools: HashMap::new(),
        };
        cache.load();
        cache
    }

    #[cfg(test)]
    fn with_path(path: PathBuf) -> Self {
        let mut cache = Self {
            path: Some(path),
            tools: HashMap::new(),
        };
        cache.load();
        cache
    }

    fn load(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(file) if file.version == VERSION => self.tools = file.tools,
            Ok(_) => eprintln!("cache: ignoring {}: unsupported version", path.display()),
            Err(err) => eprintln!("cache: ignoring {}: {err}", path.display()),
        }
    }

    /// Record one successful call for `entry` and persist the cache.
    pub fn record(&mut self, entry: &CatalogEntry) {
        let now = unix_seconds();
        let item = self
            .tools
            .entry(entry.name.clone())
            .or_insert_with(|| CachedTool {
                server: entry.server.clone(),
                calls: 0,
                last_used: now,
                description: String::new(),
            });
        item.server = entry.server.clone();
        item.calls = item.calls.saturating_add(1);
        item.last_used = now;
        item.description = entry
            .tool
            .description
            .as_ref()
            .map(|description| description.to_string())
            .unwrap_or_default();
        if self.tools.len() > MAX_ENTRIES {
            self.prune();
        }
        self.persist();
    }

    /// Call counts for search ranking.
    pub fn counts(&self) -> HashMap<String, u64> {
        self.tools
            .iter()
            .map(|(name, item)| (name.clone(), item.calls))
            .collect()
    }

    /// Most-called tools, most recent first on ties.
    pub fn hot(&self, limit: usize) -> Vec<HotTool> {
        let mut items: Vec<_> = self.tools.iter().collect();
        sort_usage(&mut items);
        items
            .into_iter()
            .take(limit)
            .map(|(name, item)| HotTool {
                name: name.clone(),
                server: item.server.clone(),
                description: item.description.clone(),
            })
            .collect()
    }

    fn prune(&mut self) {
        let mut items: Vec<(String, CachedTool)> = self.tools.drain().collect();
        items.sort_by(|a, b| {
            b.1.calls
                .cmp(&a.1.calls)
                .then_with(|| b.1.last_used.cmp(&a.1.last_used))
                .then_with(|| a.0.cmp(&b.0))
        });
        items.truncate(MAX_ENTRIES);
        self.tools = items.into_iter().collect();
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!("cache: cannot create {}: {err}", parent.display());
            return;
        }
        let file = CacheFile {
            version: VERSION,
            tools: self.tools.clone(),
        };
        let Ok(text) = serde_json::to_string_pretty(&file) else {
            return;
        };
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        if let Err(err) = fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, path)) {
            eprintln!("cache: cannot write {}: {err}", path.display());
            let _ = fs::remove_file(&tmp);
        }
    }
}

fn sort_usage(items: &mut Vec<(&String, &CachedTool)>) {
    items.sort_by(|a, b| {
        b.1.calls
            .cmp(&a.1.calls)
            .then_with(|| b.1.last_used.cmp(&a.1.last_used))
            .then_with(|| a.0.cmp(b.0))
    });
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Keep separate caches for separate config files so workspaces do not mix.
fn default_path(config_path: &Path) -> Option<PathBuf> {
    let dir = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("tokenmaxing")
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/tokenmaxing"))?
    };
    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config_path.hash(&mut hasher);
    Some(dir.join(format!("usage-{:016x}.json", hasher.finish())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{JsonObject, Tool};

    fn entry(name: &'static str, description: &'static str) -> CatalogEntry {
        CatalogEntry {
            server: "demo".to_string(),
            name: name.to_string(),
            tool: Tool::new(name, description, JsonObject::new()),
        }
    }

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "tokenmaxing-cache-{}-{}.json",
            std::process::id(),
            unix_seconds()
        ))
    }

    #[test]
    fn persists_counts_and_orders_hot_tools() {
        let path = temp_path();
        {
            let mut cache = UsageCache::with_path(path.clone());
            cache.record(&entry("cold", "rarely used"));
            cache.record(&entry("hot", "used a lot"));
            cache.record(&entry("hot", "used a lot"));
        }

        let cache = UsageCache::with_path(path.clone());
        assert_eq!(cache.counts().get("hot"), Some(&2));
        assert_eq!(cache.counts().get("cold"), Some(&1));
        let hot = cache.hot(2);
        assert_eq!(hot[0].name, "hot");
        assert_eq!(hot[0].description, "used a lot");
        assert_eq!(hot[1].name, "cold");
        let _ = fs::remove_file(path);
    }
}

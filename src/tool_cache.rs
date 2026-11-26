use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use rmcp::model::{CallToolResult, ContentBlock, MetaObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiktoken_rs::o200k_base_singleton;

use crate::config::{EmbeddingProvider, EmbeddingSettings, ToolCacheMode, ToolCacheSettings};
use crate::metrics;

const VERSION: u32 = 1;
const MAX_TEXT_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Exact,
    Semantic,
}

#[derive(Debug, Clone)]
pub struct CacheHit {
    pub result: CallToolResult,
    pub kind: HitKind,
}

#[derive(Debug, Clone, Default)]
pub struct StoreContext {
    pub exact_key: String,
    pub fingerprint: String,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Default)]
pub struct CacheLookup {
    pub hit: Option<CacheHit>,
    pub store: Option<StoreContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedResult {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    structured_content: Option<Value>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default, rename = "_meta")]
    meta: Option<MetaObject>,
}

impl From<&CallToolResult> for CachedResult {
    fn from(result: &CallToolResult) -> Self {
        Self {
            content: result.content.clone(),
            structured_content: result.structured_content.clone(),
            is_error: result.is_error,
            meta: result.meta.clone(),
        }
    }
}

impl CachedResult {
    fn into_result(self) -> CallToolResult {
        let mut result = CallToolResult::success(self.content);
        result.structured_content = self.structured_content;
        result.is_error = self.is_error;
        result.meta = self.meta;
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    tool: String,
    server: String,
    fingerprint: String,
    exact_key: String,
    embedding: Option<Vec<f32>>,
    result: CachedResult,
    created_at: u64,
    last_used: u64,
    hits: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    #[serde(default)]
    entries: Vec<CacheEntry>,
}

/// Persistent exact + semantic cache for MCP tool results.
pub struct ToolCache {
    settings: ToolCacheSettings,
    path: Option<PathBuf>,
    entries: Vec<CacheEntry>,
    exact: HashMap<String, usize>,
    embedder: Option<Embedder>,
    semantic: bool,
    ttl_seconds: u64,
    threshold: f32,
    max_entries: usize,
    max_result_bytes: usize,
}

impl ToolCache {
    pub fn from_settings(settings: ToolCacheSettings, config_path: &Path) -> anyhow::Result<Self> {
        if !settings.enabled {
            bail!("disabled");
        }
        if std::env::var("TOKENMAXING_TOOL_CACHE")
            .ok()
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.eq_ignore_ascii_case("off"))
        {
            bail!("disabled by TOKENMAXING_TOOL_CACHE");
        }
        if settings.tools.is_empty() {
            bail!("toolCache.tools is empty; refusing to cache every tool");
        }

        let mut semantic = settings.mode == ToolCacheMode::Semantic;
        let embedder = if semantic {
            match settings.embeddings.clone() {
                Some(embeddings) => match Embedder::new(embeddings) {
                    Ok(embedder) => Some(embedder),
                    Err(err) => {
                        eprintln!("tool cache: embeddings unavailable ({err:#}); using exact only");
                        semantic = false;
                        None
                    }
                },
                None => {
                    eprintln!("tool cache: semantic mode needs embeddings; using exact only");
                    semantic = false;
                    None
                }
            }
        } else {
            None
        };

        let mut cache = Self {
            path: resolve_path(&settings, config_path),
            ttl_seconds: settings.ttl_seconds.max(1),
            threshold: settings.threshold.clamp(0.0, 1.0),
            max_entries: settings.max_entries.clamp(1, 10_000),
            max_result_bytes: settings.max_result_bytes.clamp(256, 10 * 1024 * 1024),
            settings,
            entries: Vec::new(),
            exact: HashMap::new(),
            embedder,
            semantic,
        };
        cache.load();
        cache.prune_expired();
        metrics::set_tool_cache_entries(cache.entries.len());
        Ok(cache)
    }

    pub fn allows(&self, tool: &str) -> bool {
        self.settings.tools.iter().any(|pattern| {
            pattern == "*"
                || pattern == tool
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| tool.starts_with(prefix))
        })
    }

    pub fn semantic(&self) -> bool {
        self.semantic
    }

    pub fn embedder(&self) -> Option<Embedder> {
        self.embedder.clone()
    }

    pub fn exact_key(&self, tool: &str, server: &str, fingerprint: &str, args: &Value) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tool.hash(&mut hasher);
        server.hash(&mut hasher);
        fingerprint.hash(&mut hasher);
        canonical_json(args).hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    pub fn text(&self, tool: &str, server: &str, args: &Value) -> String {
        let text = format!("{tool} {server} {}", canonical_json(args));
        text.chars().take(MAX_TEXT_CHARS).collect()
    }

    pub fn exact_lookup(
        &mut self,
        tool: &str,
        server: &str,
        fingerprint: &str,
        key: &str,
    ) -> Option<CacheHit> {
        self.prune_expired();
        let index = *self.exact.get(key)?;
        let entry = self.entries.get(index)?;
        if entry.tool != tool || entry.server != server || entry.fingerprint != fingerprint {
            return None;
        }
        let result = entry.result.clone().into_result();
        self.touch(index);
        Some(CacheHit {
            result,
            kind: HitKind::Exact,
        })
    }

    pub fn semantic_lookup(
        &mut self,
        tool: &str,
        server: &str,
        fingerprint: &str,
        embedding: &[f32],
    ) -> Option<CacheHit> {
        self.prune_expired();
        let mut best: Option<(usize, f32)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.tool != tool || entry.server != server || entry.fingerprint != fingerprint {
                continue;
            }
            let Some(stored) = &entry.embedding else {
                continue;
            };
            if stored.len() != embedding.len() {
                continue;
            }
            let similarity = cosine(stored, embedding);
            if similarity >= self.threshold && best.is_none_or(|(_, current)| similarity > current)
            {
                best = Some((index, similarity));
            }
        }
        let (index, _) = best?;
        let result = self.entries[index].result.clone().into_result();
        self.touch(index);
        Some(CacheHit {
            result,
            kind: HitKind::Semantic,
        })
    }

    pub fn store(
        &mut self,
        tool: &str,
        server: &str,
        context: StoreContext,
        result: &CallToolResult,
    ) {
        if result.is_error == Some(true) || !self.allows(tool) {
            return;
        }
        let fingerprint = context.fingerprint.clone();
        let cached = CachedResult::from(result);
        let bytes = serde_json::to_vec(&cached)
            .map(|data| data.len())
            .unwrap_or(usize::MAX);
        if bytes > self.max_result_bytes {
            return;
        }

        let now = unix_seconds();
        if let Some(&index) = self.exact.get(&context.exact_key) {
            let entry = &mut self.entries[index];
            entry.tool = tool.to_string();
            entry.server = server.to_string();
            entry.fingerprint = fingerprint;
            entry.embedding = context.embedding;
            entry.result = cached;
            entry.created_at = now;
            entry.last_used = now;
            entry.hits = 0;
        } else {
            if self.entries.len() >= self.max_entries {
                self.evict_one();
            }
            let index = self.entries.len();
            self.entries.push(CacheEntry {
                tool: tool.to_string(),
                server: server.to_string(),
                fingerprint,
                exact_key: context.exact_key,
                embedding: context.embedding,
                result: cached,
                created_at: now,
                last_used: now,
                hits: 0,
            });
            self.exact
                .insert(self.entries[index].exact_key.clone(), index);
        }
        self.persist();
        metrics::set_tool_cache_entries(self.entries.len());
    }

    pub fn clear_server(&mut self, server: &str) {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.server != server);
        if self.entries.len() != before {
            self.rebuild_exact();
            self.persist();
            metrics::set_tool_cache_entries(self.entries.len());
        }
    }

    fn touch(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.hits = entry.hits.saturating_add(1);
            entry.last_used = unix_seconds();
        }
    }

    fn evict_one(&mut self) {
        if let Some((index, _)) = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| (entry.last_used, entry.hits))
        {
            self.entries.remove(index);
            self.rebuild_exact();
        }
    }

    fn prune_expired(&mut self) {
        let now = unix_seconds();
        let ttl = self.ttl_seconds;
        let before = self.entries.len();
        self.entries
            .retain(|entry| now.saturating_sub(entry.created_at) <= ttl);
        if self.entries.len() != before {
            self.rebuild_exact();
            self.persist();
            metrics::set_tool_cache_entries(self.entries.len());
        }
    }

    fn rebuild_exact(&mut self) {
        self.exact.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.exact.insert(entry.exact_key.clone(), index);
        }
    }

    fn load(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(file) if file.version == VERSION => {
                self.entries = file.entries;
                self.rebuild_exact();
            }
            Ok(_) => eprintln!(
                "tool cache: ignoring {}: unsupported version",
                path.display()
            ),
            Err(err) => eprintln!("tool cache: ignoring {}: {err}", path.display()),
        }
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!("tool cache: cannot create {}: {err}", parent.display());
            return;
        }
        let file = CacheFile {
            version: VERSION,
            entries: self.entries.clone(),
        };
        let Ok(text) = serde_json::to_string(&file) else {
            return;
        };
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        if let Err(err) = fs::write(&temp, text).and_then(|_| fs::rename(&temp, path)) {
            eprintln!("tool cache: cannot write {}: {err}", path.display());
            let _ = fs::remove_file(&temp);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }

    #[cfg(test)]
    fn expire_all(&mut self) {
        for entry in &mut self.entries {
            entry.created_at = 0;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Embedder {
    settings: EmbeddingSettings,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl Embedder {
    fn new(settings: EmbeddingSettings) -> anyhow::Result<Self> {
        if settings.provider != EmbeddingProvider::Hash && settings.model.trim().is_empty() {
            bail!("embeddings.model is required");
        }
        let timeout = Duration::from_millis(settings.timeout_ms.clamp(100, 60_000));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building embeddings HTTP client")?;
        let api_key = settings
            .api_key
            .clone()
            .or_else(|| std::env::var("TOKENMAXING_EMBEDDINGS_API_KEY").ok());
        Ok(Self {
            settings,
            http,
            api_key,
        })
    }

    pub async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        match self.settings.provider {
            EmbeddingProvider::Hash => Ok(hash_embedding(text, self.settings.dimensions)),
            EmbeddingProvider::Openai => self.openai(text).await,
            EmbeddingProvider::Ollama => self.ollama(text).await,
        }
    }

    async fn openai(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let base = self
            .settings
            .url
            .as_deref()
            .unwrap_or("https://api.openai.com/v1");
        let url = if base.trim_end_matches('/').ends_with("/embeddings") {
            base.to_string()
        } else {
            format!("{}/embeddings", base.trim_end_matches('/'))
        };
        let mut request = self.http.post(url).json(&serde_json::json!({
            "model": self.settings.model,
            "input": text,
        }));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.context("embeddings request")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("embeddings HTTP {status}: {body}");
        }
        let value: Value = serde_json::from_str(&body).context("parsing embeddings response")?;
        let embedding = value
            .pointer("/data/0/embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("embeddings response missing data[0].embedding"))?;
        parse_embedding(embedding)
    }

    async fn ollama(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let base = self
            .settings
            .url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = if base.trim_end_matches('/').ends_with("/api/embeddings") {
            base.to_string()
        } else {
            format!("{}/api/embeddings", base.trim_end_matches('/'))
        };
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "model": self.settings.model,
                "prompt": text,
            }))
            .send()
            .await
            .context("ollama embeddings request")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("ollama embeddings HTTP {status}: {body}");
        }
        let value: Value = serde_json::from_str(&body).context("parsing ollama response")?;
        let embedding = value
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("ollama response missing embedding"))?;
        parse_embedding(embedding)
    }
}

fn parse_embedding(values: &[Value]) -> anyhow::Result<Vec<f32>> {
    let mut embedding = Vec::with_capacity(values.len());
    for value in values {
        embedding.push(
            value
                .as_f64()
                .ok_or_else(|| anyhow!("embedding contains a non-number"))? as f32,
        );
    }
    Ok(normalize(embedding))
}

/// Deterministic token-hash embedding. This is lexical, not a semantic model:
/// useful for tests and offline use, but for real semantics configure
/// OpenAI-compatible or Ollama embeddings.
fn hash_embedding(text: &str, dimensions: usize) -> Vec<f32> {
    let dimensions = dimensions.clamp(8, 4_096);
    let bpe = o200k_base_singleton();
    let tokens = bpe.encode_ordinary(&text.to_lowercase());
    let mut vector = vec![0.0f32; dimensions];
    for token in tokens {
        vector[(token as usize) % dimensions] += 1.0;
    }
    normalize(vector)
}

fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Stable fingerprint of a tool definition, so schema changes invalidate
/// cached results for that tool.
pub fn tool_fingerprint(tool: &Tool) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(tool)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn resolve_path(settings: &ToolCacheSettings, config_path: &Path) -> Option<PathBuf> {
    match std::env::var("TOKENMAXING_TOOL_CACHE").ok().as_deref() {
        Some("off") | Some("") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => settings
            .path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| default_path(config_path)),
    }
}

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
    Some(dir.join(format!("tool-cache-{:016x}.json", hasher.finish())))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::JsonObject;
    use serde_json::json;

    fn settings(mode: ToolCacheMode, path: &Path) -> ToolCacheSettings {
        ToolCacheSettings {
            enabled: true,
            mode,
            ttl_seconds: 300,
            threshold: 0.8,
            max_entries: 10,
            max_result_bytes: 65_536,
            tools: vec!["*".to_string()],
            embeddings: Some(EmbeddingSettings {
                provider: EmbeddingProvider::Hash,
                model: String::new(),
                url: None,
                api_key: None,
                dimensions: 256,
                timeout_ms: 5_000,
            }),
            path: Some(path.to_string_lossy().to_string()),
        }
    }

    fn temp_path() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tokenmaxing-tool-cache-{}-{}-{sequence}.json",
            std::process::id(),
            unix_seconds()
        ))
    }

    fn tool(name: &'static str) -> Tool {
        Tool::new(name, "test tool", JsonObject::new())
    }

    fn result(text: &str) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn canonical_json_sorts_objects() {
        let a = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let b = json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn exact_cache_roundtrip_and_persistence() {
        let path = temp_path();
        let tool = tool("echo");
        let fingerprint = tool_fingerprint(&tool);
        let args = json!({"text": "hello"});
        let mut cache =
            ToolCache::from_settings(settings(ToolCacheMode::Exact, &path), &path).unwrap();
        let key = cache.exact_key("echo", "demo", &fingerprint, &args);
        assert!(
            cache
                .exact_lookup("echo", "demo", &fingerprint, &key)
                .is_none()
        );

        let context = StoreContext {
            exact_key: key.clone(),
            fingerprint: fingerprint.clone(),
            embedding: None,
        };
        cache.store("echo", "demo", context, &result("hello"));
        let hit = cache
            .exact_lookup("echo", "demo", &fingerprint, &key)
            .unwrap();
        assert_eq!(hit.kind, HitKind::Exact);
        assert_eq!(text_of(&hit.result), "hello");
        drop(cache);

        let mut cache =
            ToolCache::from_settings(settings(ToolCacheMode::Exact, &path), &path).unwrap();
        let hit = cache
            .exact_lookup("echo", "demo", &fingerprint, &key)
            .unwrap();
        assert_eq!(text_of(&hit.result), "hello");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn semantic_cache_matches_similar_text() {
        let path = temp_path();
        let tool = tool("docs");
        let fingerprint = tool_fingerprint(&tool);
        let mut cache =
            ToolCache::from_settings(settings(ToolCacheMode::Semantic, &path), &path).unwrap();
        let first = json!({"query": "read the readme file"});
        let embedding = hash_embedding(&canonical_json(&first), 256);
        let key = cache.exact_key("docs", "demo", &fingerprint, &first);
        let context = StoreContext {
            exact_key: key,
            fingerprint: fingerprint.clone(),
            embedding: Some(embedding),
        };
        cache.store("docs", "demo", context, &result("readme docs"));

        let second = json!({"query": "read the readme"});
        let query = hash_embedding(&canonical_json(&second), 256);
        let hit = cache
            .semantic_lookup("docs", "demo", &fingerprint, &query)
            .expect("semantic hit");
        assert_eq!(hit.kind, HitKind::Semantic);
        assert_eq!(text_of(&hit.result), "readme docs");

        let unrelated = json!({"query": "deploy kubernetes"});
        let unrelated = hash_embedding(&canonical_json(&unrelated), 256);
        assert!(
            cache
                .semantic_lookup("docs", "demo", &fingerprint, &unrelated)
                .is_none()
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn allowlist_ttl_size_and_eviction() {
        let path = temp_path();
        let mut config = settings(ToolCacheMode::Exact, &path);
        config.tools = vec!["echo".to_string()];
        config.max_entries = 1;
        config.max_result_bytes = 64;
        config.ttl_seconds = 1;
        let tool = tool("echo");
        let fingerprint = tool_fingerprint(&tool);
        let mut cache = ToolCache::from_settings(config, &path).unwrap();

        let args = json!({"text": "one"});
        let key = cache.exact_key("echo", "demo", &fingerprint, &args);
        let context = StoreContext {
            exact_key: key.clone(),
            fingerprint: fingerprint.clone(),
            embedding: None,
        };
        cache.store("other", "demo", context, &result("blocked"));
        assert!(cache.entries.is_empty());

        let context = StoreContext {
            exact_key: key.clone(),
            fingerprint: fingerprint.clone(),
            embedding: None,
        };
        cache.store("echo", "demo", context, &result("one"));
        assert_eq!(cache.entries.len(), 1);

        cache.expire_all();
        assert!(
            cache
                .exact_lookup("echo", "demo", &fingerprint, &key)
                .is_none()
        );
        assert!(cache.entries.is_empty());

        let long = "x".repeat(200);
        let args = json!({"text": "long"});
        let key = cache.exact_key("echo", "demo", &fingerprint, &args);
        let context = StoreContext {
            exact_key: key,
            fingerprint: fingerprint.clone(),
            embedding: None,
        };
        cache.store("echo", "demo", context, &result(&long));
        assert!(cache.entries.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clear_server_removes_entries() {
        let path = temp_path();
        let mut cache =
            ToolCache::from_settings(settings(ToolCacheMode::Exact, &path), &path).unwrap();
        let tool = tool("echo");
        let fingerprint = tool_fingerprint(&tool);
        let args = json!({"text": "hello"});
        let key = cache.exact_key("echo", "demo", &fingerprint, &args);
        let context = StoreContext {
            exact_key: key.clone(),
            fingerprint: fingerprint.clone(),
            embedding: None,
        };
        cache.store("echo", "demo", context, &result("hello"));
        cache.clear_server("demo");
        assert!(
            cache
                .exact_lookup("echo", "demo", &fingerprint, &key)
                .is_none()
        );
        let _ = fs::remove_file(path);
    }
}

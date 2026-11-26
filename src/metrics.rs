use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use prometheus::{
    Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder, exponential_buckets,
};
use tiktoken_rs::CoreBPE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Prometheus metrics collected by the gateway.
pub struct Metrics {
    registry: Registry,
    tool_calls: IntCounterVec,
    tool_call_duration: HistogramVec,
    meta_tool_calls: IntCounterVec,
    catalog_tools: IntGaugeVec,
    servers_connected: IntGaugeVec,
    compression: IntCounterVec,
    code_mode: IntCounterVec,
    code_mode_duration: Histogram,
    tool_definition_tokens: IntGaugeVec,
    meta_definition_tokens: IntGauge,
    inspection_tokens: IntCounter,
    compression_original_tokens: IntCounter,
    compression_compressed_tokens: IntCounter,
    compression_saved_tokens: IntCounter,
    tool_cache_lookups: IntCounterVec,
    tool_cache_entries: IntGauge,
    tool_cache_saved_calls: IntCounter,
    tool_cache_saved_result_bytes: IntCounter,
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();

        let tool_calls = IntCounterVec::new(
            Opts::new(
                "tokenmaxing_tool_calls_total",
                "Calls to upstream tools through call_tool",
            ),
            &["tool", "server", "status"],
        )
        .expect("valid metric");

        let tool_call_duration = HistogramVec::new(
            HistogramOpts::new(
                "tokenmaxing_tool_call_duration_seconds",
                "Latency of upstream tool calls",
            )
            .buckets(buckets()),
            &["tool", "server"],
        )
        .expect("valid metric");

        let meta_tool_calls = IntCounterVec::new(
            Opts::new(
                "tokenmaxing_meta_tool_calls_total",
                "Calls to the gateway's own meta-tools",
            ),
            &["tool", "status"],
        )
        .expect("valid metric");

        let catalog_tools = IntGaugeVec::new(
            Opts::new(
                "tokenmaxing_catalog_tools",
                "Number of tools in the catalog per upstream server",
            ),
            &["server"],
        )
        .expect("valid metric");

        let servers_connected = IntGaugeVec::new(
            Opts::new(
                "tokenmaxing_servers_connected",
                "Whether an upstream server is connected (1) or not (0)",
            ),
            &["server"],
        )
        .expect("valid metric");

        let compression = IntCounterVec::new(
            Opts::new(
                "tokenmaxing_compression_total",
                "Result of automatic compression of tool results",
            ),
            &["outcome"],
        )
        .expect("valid metric");

        let code_mode = IntCounterVec::new(
            Opts::new(
                "tokenmaxing_code_mode_executions_total",
                "Executions of execute_code by status",
            ),
            &["status"],
        )
        .expect("valid metric");

        let code_mode_duration = Histogram::with_opts(
            HistogramOpts::new(
                "tokenmaxing_code_mode_duration_seconds",
                "Duration of execute_code runs",
            )
            .buckets(buckets()),
        )
        .expect("valid metric");

        let tool_definition_tokens = IntGaugeVec::new(
            Opts::new(
                "tokenmaxing_tool_definition_tokens",
                "Tokenizer counts of all tool definitions per server (direct-client baseline)",
            ),
            &["server"],
        )
        .expect("valid metric");

        let meta_definition_tokens = IntGauge::new(
            "tokenmaxing_meta_tool_definition_tokens",
            "Tokenizer counts of the gateway's own meta-tool definitions",
        )
        .expect("valid metric");

        let inspection_tokens = IntCounter::new(
            "tokenmaxing_inspection_tokens_total",
            "Tokenizer counts of search_tools and get_tool_details responses",
        )
        .expect("valid metric");

        let compression_original_tokens = IntCounter::new(
            "tokenmaxing_compression_original_tokens_total",
            "Tokens before Headroom compression",
        )
        .expect("valid metric");

        let compression_compressed_tokens = IntCounter::new(
            "tokenmaxing_compression_compressed_tokens_total",
            "Tokens after Headroom compression",
        )
        .expect("valid metric");

        let compression_saved_tokens = IntCounter::new(
            "tokenmaxing_compression_saved_tokens_total",
            "Tokens saved by Headroom compression",
        )
        .expect("valid metric");

        let tool_cache_lookups = IntCounterVec::new(
            Opts::new(
                "tokenmaxing_tool_cache_lookups_total",
                "Tool result cache lookups by outcome",
            ),
            &["result"],
        )
        .expect("valid metric");

        let tool_cache_entries = IntGauge::new(
            "tokenmaxing_tool_cache_entries",
            "Entries in the tool result cache",
        )
        .expect("valid metric");

        let tool_cache_saved_calls = IntCounter::new(
            "tokenmaxing_tool_cache_saved_calls_total",
            "Upstream tool calls avoided by tool cache hits",
        )
        .expect("valid metric");

        let tool_cache_saved_result_bytes = IntCounter::new(
            "tokenmaxing_tool_cache_saved_result_bytes_total",
            "Serialized tool result bytes served from the cache",
        )
        .expect("valid metric");

        let tokenizer_info = IntGaugeVec::new(
            Opts::new(
                "tokenmaxing_tokenizer_info",
                "Tokenizer used for real token counts",
            ),
            &["tokenizer"],
        )
        .expect("valid metric");
        tokenizer_info.with_label_values(&[tokenizer_name()]).set(1);

        registry
            .register(Box::new(tool_calls.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_call_duration.clone()))
            .expect("register metric");
        registry
            .register(Box::new(meta_tool_calls.clone()))
            .expect("register metric");
        registry
            .register(Box::new(catalog_tools.clone()))
            .expect("register metric");
        registry
            .register(Box::new(servers_connected.clone()))
            .expect("register metric");
        registry
            .register(Box::new(compression.clone()))
            .expect("register metric");
        registry
            .register(Box::new(code_mode.clone()))
            .expect("register metric");
        registry
            .register(Box::new(code_mode_duration.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_definition_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(meta_definition_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(inspection_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(compression_original_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(compression_compressed_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(compression_saved_tokens.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_cache_lookups.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_cache_entries.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_cache_saved_calls.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tool_cache_saved_result_bytes.clone()))
            .expect("register metric");
        registry
            .register(Box::new(tokenizer_info.clone()))
            .expect("register metric");

        Self {
            registry,
            tool_calls,
            tool_call_duration,
            meta_tool_calls,
            catalog_tools,
            servers_connected,
            compression,
            code_mode,
            code_mode_duration,
            tool_definition_tokens,
            meta_definition_tokens,
            inspection_tokens,
            compression_original_tokens,
            compression_compressed_tokens,
            compression_saved_tokens,
            tool_cache_lookups,
            tool_cache_entries,
            tool_cache_saved_calls,
            tool_cache_saved_result_bytes,
        }
    }
}

fn buckets() -> Vec<f64> {
    exponential_buckets(0.001, 2.0, 14).expect("valid buckets")
}

/// Initialize the global metric registry. Safe to call more than once.
pub fn init() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

fn global() -> Option<&'static Metrics> {
    METRICS.get()
}

pub fn record_tool_call(tool: &str, server: &str, status: &str, seconds: f64) {
    if let Some(metrics) = global() {
        metrics
            .tool_calls
            .with_label_values(&[tool, server, status])
            .inc();
        metrics
            .tool_call_duration
            .with_label_values(&[tool, server])
            .observe(seconds);
    }
}

pub fn record_meta_tool(tool: &str, status: &str) {
    if let Some(metrics) = global() {
        metrics
            .meta_tool_calls
            .with_label_values(&[tool, status])
            .inc();
    }
}

pub fn set_catalog_tools(server: &str, count: usize) {
    if let Some(metrics) = global() {
        metrics
            .catalog_tools
            .with_label_values(&[server])
            .set(count as i64);
    }
}

pub fn set_server_connected(server: &str, connected: bool) {
    if let Some(metrics) = global() {
        metrics
            .servers_connected
            .with_label_values(&[server])
            .set(if connected { 1 } else { 0 });
    }
}

pub fn record_tool_cache_lookup(result: &str) {
    if let Some(metrics) = global() {
        metrics
            .tool_cache_lookups
            .with_label_values(&[result])
            .inc();
    }
}

pub fn set_tool_cache_entries(count: usize) {
    if let Some(metrics) = global() {
        metrics.tool_cache_entries.set(count as i64);
    }
}

pub fn record_tool_cache_saved_call(bytes: usize) {
    if let Some(metrics) = global() {
        metrics.tool_cache_saved_calls.inc();
        metrics.tool_cache_saved_result_bytes.inc_by(bytes as u64);
    }
}

pub fn record_compression(outcome: &str) {
    if let Some(metrics) = global() {
        metrics.compression.with_label_values(&[outcome]).inc();
    }
}

pub fn record_code_mode(status: &str, seconds: f64) {
    if let Some(metrics) = global() {
        metrics.code_mode.with_label_values(&[status]).inc();
        metrics.code_mode_duration.observe(seconds);
    }
}

fn tokenizer_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let value =
            std::env::var("TOKENMAXING_TOKENIZER").unwrap_or_else(|_| "o200k_base".to_string());
        match value.as_str() {
            "o200k_base" | "cl100k_base" | "p50k_base" | "r50k_base" => value,
            other => {
                eprintln!("metrics: unknown TOKENMAXING_TOKENIZER '{other}', using o200k_base");
                "o200k_base".to_string()
            }
        }
    })
}

fn bpe() -> &'static CoreBPE {
    match tokenizer_name() {
        "cl100k_base" => tiktoken_rs::cl100k_base_singleton(),
        "p50k_base" => tiktoken_rs::p50k_base_singleton(),
        "r50k_base" => tiktoken_rs::r50k_base_singleton(),
        _ => tiktoken_rs::o200k_base_singleton(),
    }
}

/// Real tokenizer count for the configured encoding (`TOKENMAXING_TOKENIZER`,
/// default `o200k_base`). Not chars/4.
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    bpe().encode_ordinary(text).len() as u64
}

pub fn set_server_definition_tokens(server: &str, tokens: u64) {
    if let Some(metrics) = global() {
        metrics
            .tool_definition_tokens
            .with_label_values(&[server])
            .set(tokens as i64);
    }
}

pub fn set_meta_definition_tokens(tokens: u64) {
    if let Some(metrics) = global() {
        metrics.meta_definition_tokens.set(tokens as i64);
    }
}

pub fn record_inspection_tokens(tokens: u64) {
    if let Some(metrics) = global() {
        metrics.inspection_tokens.inc_by(tokens);
    }
}

pub fn record_compression_tokens(original: u64, compressed: u64) {
    if let Some(metrics) = global() {
        metrics.compression_original_tokens.inc_by(original);
        metrics.compression_compressed_tokens.inc_by(compressed);
        metrics
            .compression_saved_tokens
            .inc_by(original.saturating_sub(compressed));
    }
}

/// Render all metrics in the Prometheus text exposition format.
pub fn render() -> String {
    match global() {
        Some(metrics) => TextEncoder::new()
            .encode_to_string(&metrics.registry.gather())
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// Minimal HTTP server for Prometheus scraping (`GET /metrics`).
pub async fn serve_http(addr: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("metrics: serving at http://{addr}/metrics");

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                eprintln!("metrics: accept failed: {err}");
                continue;
            }
        };

        tokio::spawn(async move {
            let mut request = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut request)).await;

            let body = render();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

/// Parse the `TOKENMAXING_METRICS_ADDR` setting. Returns `None` when disabled.
pub fn configured_addr() -> Option<SocketAddr> {
    let value =
        std::env::var("TOKENMAXING_METRICS_ADDR").unwrap_or_else(|_| "127.0.0.1:9464".into());
    if value.eq_ignore_ascii_case("off") || value.is_empty() {
        return None;
    }
    match value.parse() {
        Ok(addr) => Some(addr),
        Err(err) => {
            eprintln!("metrics: invalid TOKENMAXING_METRICS_ADDR '{value}': {err}");
            None
        }
    }
}

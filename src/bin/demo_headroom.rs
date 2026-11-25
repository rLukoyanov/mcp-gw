// A tiny stand-in for Headroom's MCP server, used to exercise the gateway's
// automatic compression path end-to-end. It exposes `headroom_compress` and
// `headroom_retrieve` with a truncation "compressor" and an in-memory store.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use rmcp::{
    ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompressParams {
    /// Text to compress.
    content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RetrieveParams {
    /// Hash returned by `headroom_compress`.
    hash: String,
}

#[derive(Clone, Default)]
struct HeadroomDemo {
    store: Arc<Mutex<HashMap<String, String>>>,
}

#[tool_router(server_handler)]
impl HeadroomDemo {
    #[tool(
        name = "headroom_compress",
        description = "Compress content on demand and store the original for retrieval."
    )]
    fn headroom_compress(
        &self,
        Parameters(CompressParams { content }): Parameters<CompressParams>,
    ) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        let compressed: String = content.chars().take(160).collect();
        let compressed = if compressed.len() < content.len() {
            format!("{compressed}…")
        } else {
            compressed
        };

        let original_tokens = content.split_whitespace().count();
        let compressed_tokens = compressed.split_whitespace().count();
        let savings_percent = if content.is_empty() {
            0.0
        } else {
            (1.0 - compressed.len() as f64 / content.len() as f64) * 100.0
        };

        self.store.lock().unwrap().insert(hash.clone(), content);

        json!({
            "compressed": compressed,
            "hash": hash,
            "original_tokens": original_tokens,
            "compressed_tokens": compressed_tokens,
            "savings_percent": savings_percent,
            "transforms": ["demo:truncate"],
        })
        .to_string()
    }

    #[tool(
        name = "headroom_retrieve",
        description = "Retrieve the original content for a hash returned by headroom_compress."
    )]
    fn headroom_retrieve(
        &self,
        Parameters(RetrieveParams { hash }): Parameters<RetrieveParams>,
    ) -> String {
        match self.store.lock().unwrap().get(&hash) {
            Some(content) => content.clone(),
            None => format!("entry not found or expired: {hash}"),
        }
    }

    #[tool(
        name = "headroom_stats",
        description = "Return simple session stats for the demo compressor."
    )]
    fn headroom_stats(&self) -> String {
        let entries = self.store.lock().unwrap().len();
        json!({ "stored_entries": entries }).to_string()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = HeadroomDemo::default().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

use crate::metrics;
use crate::registry::GatewayState;

/// Route a tool result through the configured compressor (Headroom) when it is
/// large enough. Fails open: any problem returns the original result unchanged.
///
/// Results that come from the compressor server itself (for example
/// `headroom_retrieve`) are never re-compressed.
pub async fn compress(
    state: &GatewayState,
    source_server: &str,
    result: CallToolResult,
) -> CallToolResult {
    if result.is_error == Some(true) {
        metrics::record_compression("tool_error");
        return result;
    }

    let Some(settings) = state.compressor() else {
        return result;
    };
    if !settings.enabled || settings.server == source_server {
        return result;
    }

    let payload = render_payload(&result);
    if payload.len() < settings.min_bytes {
        metrics::record_compression("skipped_small");
        return result;
    }

    let Some(peer) = state.peer(&settings.server) else {
        metrics::record_compression("no_peer");
        eprintln!(
            "compression: server '{}' is not connected; returning uncompressed result",
            settings.server
        );
        return result;
    };

    let Some(arguments) = json!({ "content": payload }).as_object().cloned() else {
        return result;
    };
    let request =
        CallToolRequestParams::new(settings.compress_tool.clone()).with_arguments(arguments);

    let compressed = match peer.call_tool(request).await {
        Ok(response) if response.is_error != Some(true) => response,
        Ok(_) => {
            metrics::record_compression("error");
            eprintln!(
                "compression: '{}' returned an error; returning uncompressed result",
                settings.compress_tool
            );
            return result;
        }
        Err(err) => {
            metrics::record_compression("error");
            eprintln!(
                "compression: call to '{}' failed: {err}",
                settings.compress_tool
            );
            return result;
        }
    };

    let Some(info) = parse_info(&compressed) else {
        metrics::record_compression("bad_response");
        eprintln!(
            "compression: unexpected response from '{}'; returning uncompressed result",
            settings.compress_tool
        );
        return result;
    };

    let original_tokens = info
        .original_tokens
        .unwrap_or_else(|| metrics::count_tokens(&payload));
    let compressed_tokens = info
        .compressed_tokens
        .unwrap_or_else(|| metrics::count_tokens(&info.compressed));

    // Do not make things worse if the compressor did not actually save tokens.
    // Prefer Headroom's own token counts; the compressed text can be longer in
    // characters while still being cheaper in tokens.
    if compressed_tokens >= original_tokens {
        metrics::record_compression("no_gain");
        return result;
    }

    metrics::record_compression("compressed");
    metrics::record_compression_tokens(original_tokens, compressed_tokens);

    let retrieve_hint = format!(
        "Call {} with {{\"hash\":\"{}\"}} to retrieve the full original content.",
        settings.retrieve_tool, info.hash
    );

    CallToolResult::structured(json!({
        "headroom": {
            "compressed": info.compressed,
            "hash": info.hash,
            "original_tokens": original_tokens,
            "compressed_tokens": compressed_tokens,
            "tokens_saved": original_tokens.saturating_sub(compressed_tokens),
            "savings_percent": info.savings_percent,
            "retrieve": retrieve_hint,
        }
    }))
}

/// Text the compressor should see: the text blocks if present, otherwise the
/// structured payload.
fn render_payload(result: &CallToolResult) -> String {
    let mut text = String::new();
    for block in &result.content {
        if let Some(block) = block.as_text() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block.text);
        }
    }

    if text.is_empty()
        && let Some(structured) = &result.structured_content
    {
        text = serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
    }

    text
}

struct CompressedInfo {
    compressed: String,
    hash: String,
    original_tokens: Option<u64>,
    compressed_tokens: Option<u64>,
    savings_percent: Option<f64>,
}

fn parse_info(result: &CallToolResult) -> Option<CompressedInfo> {
    if let Some(value) = result.structured_content.as_ref()
        && let Some(info) = info_from_value(value)
    {
        return Some(info);
    }

    for block in &result.content {
        if let Some(text) = block.as_text()
            && let Ok(value) = serde_json::from_str::<Value>(&text.text)
            && let Some(info) = info_from_value(&value)
        {
            return Some(info);
        }
    }

    None
}

fn info_from_value(value: &Value) -> Option<CompressedInfo> {
    Some(CompressedInfo {
        compressed: value.get("compressed")?.as_str()?.to_string(),
        hash: value.get("hash")?.as_str()?.to_string(),
        original_tokens: value.get("original_tokens").and_then(Value::as_u64),
        compressed_tokens: value.get("compressed_tokens").and_then(Value::as_u64),
        savings_percent: value.get("savings_percent").and_then(Value::as_f64),
    })
}

use rmcp::model::{CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

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
        return result;
    }

    let Some(peer) = state.peer(&settings.server) else {
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
            eprintln!(
                "compression: '{}' returned an error; returning uncompressed result",
                settings.compress_tool
            );
            return result;
        }
        Err(err) => {
            eprintln!(
                "compression: call to '{}' failed: {err}",
                settings.compress_tool
            );
            return result;
        }
    };

    let Some(info) = parse_info(&compressed) else {
        eprintln!(
            "compression: unexpected response from '{}'; returning uncompressed result",
            settings.compress_tool
        );
        return result;
    };

    // Do not make things worse if the compressor did not actually save anything.
    if info.compressed.len() >= payload.len() {
        return result;
    }

    let retrieve_hint = format!(
        "Call {} with {{\"hash\":\"{}\"}} to retrieve the full original content.",
        settings.retrieve_tool, info.hash
    );

    CallToolResult::structured(json!({
        "headroom": {
            "compressed": info.compressed,
            "hash": info.hash,
            "original_tokens": info.original_tokens,
            "compressed_tokens": info.compressed_tokens,
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

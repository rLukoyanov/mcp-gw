use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rquickjs::{AsyncContext, AsyncRuntime, Function, Promise, function::Async};
use serde_json::{Value, json};

use crate::registry::GatewayState;

/// JavaScript prelude installed before the model's code runs.
///
/// It exposes a single async `callTool(name, args)` that is brokered back to the
/// host, and routes `console.*` to the host log buffer. The generated code has
/// no filesystem, network, process or timer access — QuickJS provides only the
/// language intrinsics plus these two bridges.
const PRELUDE: &str = r#"
function __stringify(value) {
  if (typeof value === "string") return value;
  try { return JSON.stringify(value); } catch (_) { return String(value); }
}
const __console = ["log", "info", "warn", "error", "debug"];
globalThis.console = {};
for (const __level of __console) {
  globalThis.console[__level] = (...args) => {
    __log("[" + __level + "] " + args.map(__stringify).join(" "));
  };
}
globalThis.callTool = async function (name, args) {
  const envelope = JSON.parse(
    await __callTool(String(name), JSON.stringify(args === undefined ? {} : args))
  );
  if (!envelope.ok) throw new Error(envelope.error);
  return envelope.result;
};
"#;

/// Wrap model-authored code so top-level `await` works, the completion value is
/// captured, and any thrown error becomes a structured envelope.
fn wrap(code: &str) -> String {
    format!(
        r#"(async () => {{
  try {{
    const __value = await (async () => {{
{code}
    }})();
    let __data;
    try {{
      __data = __value === undefined ? null : JSON.parse(JSON.stringify(__value));
    }} catch (_) {{
      __data = String(__value);
    }}
    return JSON.stringify({{ ok: true, result: __data }});
  }} catch (e) {{
    let __message;
    if (e && typeof e === "object" && e.message) {{
      __message = (e.name ? e.name + ": " : "") + e.message;
      if (e.stack) __message += "\n" + e.stack;
    }} else {{
      __message = String(e);
    }}
    return JSON.stringify({{ ok: false, error: __message }});
  }}
}})()"#
    )
}

/// Run model-authored JavaScript and return it as a tool result.
///
/// Script-level failures (syntax errors, thrown exceptions, timeouts) are
/// returned as `isError` results so the model can self-correct, per the MCP
/// client best-practices guide.
pub async fn execute(code: &str, state: Arc<GatewayState>, timeout: Duration) -> CallToolResult {
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let script = wrap(code);
    let outcome = run(script, state, logs.clone(), timeout).await;

    let logs = std::mem::take(&mut *logs.lock().unwrap());

    let envelope = match outcome {
        Ok(envelope) => envelope,
        Err(message) => {
            return CallToolResult::structured_error(json!({ "logs": logs, "error": message }));
        }
    };

    match serde_json::from_str::<Value>(&envelope) {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            CallToolResult::structured(json!({
                "logs": logs,
                "result": value.get("result").cloned().unwrap_or(Value::Null),
            }))
        }
        Ok(value) => CallToolResult::structured_error(json!({
            "logs": logs,
            "error": value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("script failed"),
        })),
        Err(err) => CallToolResult::structured_error(json!({
            "logs": logs,
            "error": format!("invalid sandbox response: {err}"),
        })),
    }
}

async fn run(
    script: String,
    state: Arc<GatewayState>,
    logs: Arc<Mutex<Vec<String>>>,
    timeout: Duration,
) -> Result<String, String> {
    let runtime = AsyncRuntime::new().map_err(|err| format!("sandbox init failed: {err}"))?;

    let deadline = Instant::now() + timeout;
    runtime
        .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    runtime.set_memory_limit(64 * 1024 * 1024).await;
    runtime.set_max_stack_size(512 * 1024).await;

    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|err| format!("sandbox init failed: {err}"))?;

    let result = context
        .async_with(async move |ctx| -> rquickjs::Result<String> {
            let call_tool = Function::new(
                ctx.clone(),
                Async(move |name: String, args: String| {
                    let state = state.clone();
                    async move { call_upstream(&state, &name, &args).await }
                }),
            )?;

            let log = Function::new(ctx.clone(), {
                let logs = logs.clone();
                move |line: String| {
                    if let Ok(mut buffer) = logs.lock() {
                        buffer.push(line);
                    }
                }
            })?;

            let globals = ctx.globals();
            globals.set("__callTool", call_tool)?;
            globals.set("__log", log)?;
            ctx.eval::<(), _>(PRELUDE)?;

            let promise = match ctx.eval::<Promise, _>(script.as_str()) {
                Ok(promise) => promise,
                Err(_) => {
                    if Instant::now() >= deadline {
                        let message = format!("execution timed out after {timeout:?}");
                        return Ok(json!({ "ok": false, "error": message }).to_string());
                    }
                    let caught = ctx.catch();
                    let message = caught
                        .as_exception()
                        .and_then(|exception| exception.message())
                        .or_else(|| caught.get::<String>().ok())
                        .unwrap_or_else(|| "script error".to_string());
                    return Ok(json!({ "ok": false, "error": message }).to_string());
                }
            };

            promise.into_future::<String>().await
        })
        .await;

    match result {
        Ok(envelope) => Ok(envelope),
        Err(_) if Instant::now() >= deadline => {
            Err(format!("execution timed out after {timeout:?}"))
        }
        Err(err) => Err(format!("sandbox error: {err}")),
    }
}

/// Broker one `callTool` request to the owning upstream server.
async fn call_upstream(state: &GatewayState, name: &str, args_json: &str) -> String {
    let Some(entry) = state.get(name) else {
        return json!({
            "ok": false,
            "error": format!("tool '{name}' not found; use search_tools first"),
        })
        .to_string();
    };

    let Some(peer) = state.peer(&entry.server) else {
        return json!({
            "ok": false,
            "error": format!("server '{}' is not connected; use enable_server", entry.server),
        })
        .to_string();
    };

    let mut request = CallToolRequestParams::new(entry.tool.name.clone());
    if let Ok(Value::Object(arguments)) = serde_json::from_str::<Value>(args_json) {
        request = request.with_arguments(arguments);
    }

    match peer.call_tool(request).await {
        Ok(result) if result.is_error == Some(true) => json!({
            "ok": false,
            "error": tool_error_text(&result),
        })
        .to_string(),
        Ok(result) => {
            state.record_tool_use(&entry);
            match serde_json::to_value(&result) {
                Ok(value) => json!({ "ok": true, "result": value }).to_string(),
                Err(err) => json!({
                    "ok": false,
                    "error": format!("failed to serialize tool result: {err}"),
                })
                .to_string(),
            }
        }
        Err(err) => json!({
            "ok": false,
            "error": format!("upstream call failed: {err}"),
        })
        .to_string(),
    }
}

fn tool_error_text(result: &CallToolResult) -> String {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect();
    if texts.is_empty() {
        "tool reported an error".to_string()
    } else {
        texts.join("\n")
    }
}

//! Built-in prelude `invoke_all` driver for bounded-concurrency fan-out/fan-in
//! over `exec`, `http`, and `mcp` specs with per-item error isolation and deterministic ordering.

use anyhow::{Context, bail};
use futures::stream::{self, StreamExt};
use serde_json::{Value, json};

use crate::builtins::{exec, http, mcp};
use crate::trace::{ProgressKind, report_progress};

const DEFAULT_MAX_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY_CAP: usize = 64;

#[derive(Debug, Clone)]
struct ParsedBatchItem {
    index: usize,
    id: String,
    kind: String,
    spec: Value,
}

fn infer_kind(explicit: Option<&str>, spec: &Value) -> anyhow::Result<String> {
    if let Some(k) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_ascii_lowercase());
    }
    let Some(obj) = spec.as_object() else {
        bail!("batch item `spec` must be an ExecSpec, HttpRequest, or McpCallSpec object");
    };
    if obj.contains_key("cmd") {
        return Ok("exec".to_owned());
    }
    if obj.contains_key("url") {
        return Ok("http".to_owned());
    }
    if obj.contains_key("server") && obj.contains_key("tool") {
        return Ok("mcp".to_owned());
    }
    bail!(
        "could not infer batch item `kind` from `spec` (expected `cmd`, `url`, or `server`+`tool`; pass `kind = ...` explicitly)"
    )
}

async fn run_single_item(ctx: Value, item: ParsedBatchItem) -> (usize, Value) {
    let ParsedBatchItem {
        index,
        id,
        kind,
        spec,
    } = item;

    report_progress(ProgressKind::ToolCall, format!("{id} ({kind}) running"));

    let res = match kind.as_str() {
        "exec" => exec::execute_command(ctx, spec).await,
        "http" => http::execute_http_request(ctx, spec).await,
        "mcp" => mcp::execute_mcp_client_call(ctx, spec).await,
        other => Err(anyhow::anyhow!(
            "unsupported batch item kind `{other}` (expected `exec`, `http`, or `mcp`)"
        )),
    };

    match res {
        Ok(output) => {
            report_progress(ProgressKind::ToolCall, format!("{id} ({kind}) ok"));
            (
                index,
                json!({
                    "id": id,
                    "kind": kind,
                    "ok": true,
                    "output": output,
                    "error": Value::Null,
                }),
            )
        }
        Err(err) => {
            let msg = format!("{err:#}");
            report_progress(ProgressKind::ToolCall, format!("{id} ({kind}) error: {msg}"));
            (
                index,
                json!({
                    "id": id,
                    "kind": kind,
                    "ok": false,
                    "output": Value::Null,
                    "error": msg,
                }),
            )
        }
    }
}

/// Executes a dynamic batch of `exec` / `http` / `mcp` specs concurrently with bounded parallelism.
///
/// Dropping the returned future (for example on `INTERRUPT` or state exit) cancels all in-flight
/// child operations immediately.
pub async fn execute_batch(ctx: Value, input: Value) -> anyhow::Result<Value> {
    let obj = input
        .as_object()
        .context("invoke_all input must be an object with `items`")?;

    let raw_items = obj
        .get("items")
        .and_then(Value::as_array)
        .context("invoke_all input missing `items` array")?;

    let max_concurrency = obj
        .get("max_concurrency")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_CONCURRENCY_CAP))
        .unwrap_or(DEFAULT_MAX_CONCURRENCY);

    let mut parsed = Vec::with_capacity(raw_items.len());
    for (idx, raw) in raw_items.iter().enumerate() {
        let item_obj = raw
            .as_object()
            .with_context(|| format!("invoke_all item #{idx} must be a dict"))?;
        let id = item_obj
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("item_{idx}"));
        let spec = item_obj
            .get("spec")
            .cloned()
            .with_context(|| format!("invoke_all item `{id}` missing `spec`"))?;
        let kind = infer_kind(item_obj.get("kind").and_then(Value::as_str), &spec)
            .with_context(|| format!("invoke_all item `{id}`"))?;
        parsed.push(ParsedBatchItem {
            index: idx,
            id,
            kind,
            spec,
        });
    }

    let mut indexed_results: Vec<(usize, Value)> = stream::iter(parsed)
        .map(|item| {
            let ctx_clone = ctx.clone();
            async move { run_single_item(ctx_clone, item).await }
        })
        .buffer_unordered(max_concurrency)
        .collect()
        .await;

    indexed_results.sort_by_key(|(idx, _)| *idx);

    let mut ok_count: usize = 0;
    let mut error_count: usize = 0;
    let results: Vec<Value> = indexed_results
        .into_iter()
        .map(|(_, v)| {
            if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                ok_count += 1;
            } else {
                error_count += 1;
            }
            v
        })
        .collect();

    Ok(json!({
        "results": results,
        "ok_count": ok_count,
        "error_count": error_count,
        "all_ok": error_count == 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn executes_exec_batch_in_parallel_preserving_order_and_isolating_errors() {
        let input = json!({
            "max_concurrency": 3,
            "items": [
                {
                    "id": "call_1",
                    "spec": { "cmd": ["sh", "-c", "printf one"], "check": true, "timeout_ms": 5000 }
                },
                {
                    "id": "call_2",
                    "spec": { "cmd": ["sh", "-c", "echo boom >&2; exit 7"], "check": true, "timeout_ms": 5000 }
                },
                {
                    "id": "call_3",
                    "spec": { "cmd": ["sh", "-c", "printf three"], "check": true, "timeout_ms": 5000 }
                }
            ]
        });

        let out = execute_batch(json!({}), input)
            .await
            .expect("batch should succeed with per-item error isolation");
        assert_eq!(out["ok_count"], 2);
        assert_eq!(out["error_count"], 1);
        assert_eq!(out["all_ok"], false);

        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["id"], "call_1");
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[0]["output"]["stdout"], "one");

        assert_eq!(results[1]["id"], "call_2");
        assert_eq!(results[1]["ok"], false);
        assert!(results[1]["error"].as_str().unwrap().contains("exit"));

        assert_eq!(results[2]["id"], "call_3");
        assert_eq!(results[2]["ok"], true);
        assert_eq!(results[2]["output"]["stdout"], "three");
    }
}

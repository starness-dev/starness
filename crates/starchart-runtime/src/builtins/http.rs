//! Built-in `@starchart/http` async HTTP driver backed by `reqwest` + `rustls`.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{Client, Method};
use secrecy::ExposeSecret;
use serde_json::{Map, Value, json};

use crate::builtins::secrets::{
    SecretScrubber, resolve_secret_string_map, resolve_secrets_in_value,
};
use crate::trace::{ProgressKind, report_progress};

fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent("starchart-http/0.1")
            .build()
            .expect("failed to initialize reqwest HTTP client")
    })
}

/// Executes an `HttpRequest` JSON specification resolved from `@starchart/http`.
pub async fn execute_http_request(_ctx: Value, input: Value) -> anyhow::Result<Value> {
    let mut scrubber = SecretScrubber::new();
    let res = execute_http_request_inner(&input, &mut scrubber).await;
    scrubber.scrub_result(res)
}

async fn execute_http_request_inner(
    input: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Value> {
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`@starchart/http` input must be an HttpRequest object"))?;

    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("HttpRequest missing string field `url`"))?;

    let method_str = obj.get("method").and_then(Value::as_str).unwrap_or("GET");
    let method = Method::from_bytes(method_str.as_bytes())
        .with_context(|| format!("invalid HTTP method `{method_str}`"))?;

    let timeout_ms = obj
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    if timeout_ms == 0 {
        bail!("HttpRequest `timeout_ms` must be > 0");
    }

    let resolved_headers =
        resolve_secret_string_map(obj.get("headers").and_then(Value::as_object), scrubber)?;

    let mut req = shared_http_client()
        .request(method, url)
        .timeout(Duration::from_millis(timeout_ms));

    for (k, val_str) in &resolved_headers {
        req = req.header(k.as_str(), val_str.expose_secret());
    }

    let json_payload = match obj.get("json").filter(|v| !v.is_null()) {
        Some(raw_json) => Some(resolve_secrets_in_value(raw_json, scrubber)?),
        None => None,
    };
    let raw_body = obj.get("body").and_then(Value::as_str);

    if let Some(j) = &json_payload {
        req = req.json(j);
    } else if let Some(b) = raw_body {
        req = req.body(b.to_owned());
    }

    report_progress(ProgressKind::Http, format!("{method_str} {url}"));

    let response = req
        .send()
        .await
        .with_context(|| format!("HTTP request to `{url}` failed"))?;

    let status = response.status().as_u16();
    let ok = response.status().is_success();

    let mut resp_headers = Map::new();
    for (name, val) in response.headers() {
        if let Ok(s) = val.to_str() {
            resp_headers.insert(name.as_str().to_owned(), Value::String(s.to_owned()));
        }
    }

    let bytes = response
        .bytes()
        .await
        .context("failed reading HTTP response body")?;

    report_progress(
        ProgressKind::Http,
        format!("{method_str} {url} -> {status} ({} bytes)", bytes.len()),
    );

    let body_val = if bytes.is_empty() {
        Value::Null
    } else if let Ok(parsed_json) = serde_json::from_slice::<Value>(&bytes) {
        parsed_json
    } else {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    };

    Ok(json!({
        "status": status,
        "ok": ok,
        "headers": resp_headers,
        "body": body_val,
    }))
}

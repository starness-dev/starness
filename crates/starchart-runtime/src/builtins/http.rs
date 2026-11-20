//! Built-in `@starchart/http` async HTTP driver backed by `reqwest` + `rustls`.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{Client, Method};
use serde_json::{Map, Value, json};

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
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`@starchart/http` input must be an HttpRequest object, got `{input}`"))?;

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

    let mut req = shared_http_client()
        .request(method, url)
        .timeout(Duration::from_millis(timeout_ms));

    if let Some(headers) = obj.get("headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(val_str) = v.as_str() {
                req = req.header(k.as_str(), val_str);
            }
        }
    }

    let json_payload = obj.get("json").filter(|v| !v.is_null());
    let raw_body = obj.get("body").and_then(Value::as_str);

    if let Some(j) = json_payload {
        req = req.json(j);
    } else if let Some(b) = raw_body {
        req = req.body(b.to_owned());
    }

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

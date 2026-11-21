//! Built-in `@starness/llm` runtime driver powered by `genai`.
//!
//! Executes single-shot stateless text generation and structured `Record` / JSON Schema
//! extraction across all `genai` providers (OpenAI, Anthropic, Gemini, Vertex, Bedrock,
//! OpenRouter, Ollama, Groq, xAI, DeepSeek, Mistral, Cohere, and custom OpenAI-compatible
//! endpoints), with streaming [`TraceHub`](crate::TraceHub) token events and `@starness/secrets`
//! resolution and redaction.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use genai::Client;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ChatStreamEvent, JsonSpec,
    ReasoningEffort, StopReason, Usage,
};
use genai::resolver::{AuthData, Endpoint};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::builtins::secrets::{
    SecretScrubber, parse_secret_descriptor, resolve_secret_descriptor, resolve_secrets_in_value,
};
use crate::trace::{ProgressKind, report_progress};

fn shared_genai_client() -> anyhow::Result<&'static Client> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| Client::builder().build().map_err(|e| e.to_string())) {
        Ok(client) => Ok(client),
        Err(msg) => bail!("failed initializing genai HTTP client: {msg}"),
    }
}

#[derive(Debug)]
struct ParsedModelSpec {
    model: String,
    api_key: Option<SecretString>,
    endpoint: Option<String>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    top_p: Option<f64>,
    reasoning_effort: Option<ReasoningEffort>,
}

/// Executes an `invoke_llm(...)` specification from `@starness/llm`.
pub async fn execute_llm(ctx: Value, input: Value) -> anyhow::Result<Value> {
    let mut scrubber = SecretScrubber::new();
    let res = execute_llm_inner(&ctx, &input, &mut scrubber).await;
    scrubber.scrub_result(res)
}

async fn execute_llm_inner(
    machine_ctx: &Value,
    input: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Value> {
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`@starness/llm` input must be an object"))?;

    let raw_model = obj
        .get("model")
        .ok_or_else(|| anyhow::anyhow!("`invoke_llm` missing `model`"))?;
    let model_spec = parse_model_spec(raw_model, scrubber)?;

    let prompt_val = obj
        .get("prompt")
        .ok_or_else(|| anyhow::anyhow!("`invoke_llm` missing `prompt`"))?;
    let resolved_prompt = resolve_secrets_in_value(prompt_val, scrubber)?;
    let prompt_str = resolved_prompt
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`invoke_llm` `prompt` must be a non-empty string"))?;

    let system_opt = match obj.get("system") {
        None | Some(Value::Null) => None,
        Some(sys_val) => {
            let resolved_sys = resolve_secrets_in_value(sys_val, scrubber)?;
            let s = resolved_sys.as_str().ok_or_else(|| {
                anyhow::anyhow!("`invoke_llm` `system` must evaluate to a string or None")
            })?;
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
    };

    let explicit_ctx = match obj.get("context") {
        None | Some(Value::Null) => None,
        Some(c) => Some(resolve_secrets_in_value(c, scrubber)?),
    };
    let user_content = match render_state_context_block(machine_ctx, explicit_ctx.as_ref()) {
        Some(ctx_block) => format!("{ctx_block}\n\n{prompt_str}"),
        None => prompt_str.to_owned(),
    };

    let schema_spec = parse_schema_spec(obj.get("schema"))?;
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(true);
    let timeout_ms = obj
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(60_000);
    if timeout_ms == 0 {
        bail!("`invoke_llm` `timeout_ms` must be > 0");
    }

    let client = shared_genai_client()?;
    let mut target = client
        .resolve_service_target(model_spec.model.as_str())
        .await
        .with_context(|| format!("failed resolving LLM model `{}`", model_spec.model))?;

    if let Some(ep) = &model_spec.endpoint {
        target.endpoint = Endpoint::from_owned(normalize_base_endpoint(ep));
    }

    if let Some(api_key) = &model_spec.api_key {
        target.auth = AuthData::from_single(api_key.expose_secret());
    } else if let AuthData::FromEnv(env_name) = &target.auth {
        match std::env::var(env_name) {
            Ok(val) if !val.is_empty() => {
                let secret = SecretString::from(val.clone());
                scrubber.register(env_name, &secret);
                target.auth = AuthData::from_single(val);
            }
            _ if model_spec.endpoint.is_some() => {
                // Local/self-hosted OpenAI-compatible servers (Ollama, vLLM, llama.cpp, mock servers)
                // frequently require no API key when `endpoint` is explicitly overridden.
                target.auth = AuthData::from_single("");
            }
            _ => {}
        }
    }

    let provider_str = target.model.adapter_kind.as_lower_str().to_owned();
    let requested_model_name = target.model.model_name.to_string();

    let mut chat_req = ChatRequest::default();
    if let Some(sys) = system_opt {
        chat_req = chat_req.with_system(sys);
    }
    chat_req = chat_req.append_message(ChatMessage::user(user_content));

    let mut options = ChatOptions::default()
        .with_capture_usage(true)
        .with_capture_content(true)
        .with_capture_reasoning_content(true);
    if let Some(temp) = model_spec.temperature {
        options = options.with_temperature(temp);
    }
    if let Some(max_tok) = model_spec.max_tokens {
        options = options.with_max_tokens(max_tok);
    }
    if let Some(top_p) = model_spec.top_p {
        options = options.with_top_p(top_p);
    }
    if let Some(effort) = model_spec.reasoning_effort {
        options = options.with_reasoning_effort(effort);
    }
    if let Some((spec_name, spec_schema)) = &schema_spec {
        options = options.with_response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
            spec_name.clone(),
            spec_schema.clone(),
        )));
    }

    let deadline = Duration::from_millis(timeout_ms);
    let outcome = tokio::time::timeout(deadline, async {
        if stream {
            run_streaming_chat(client, target, chat_req, &options).await
        } else {
            run_oneshot_chat(client, target, chat_req, &options).await
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "LLM call to `{provider_str}` (`{requested_model_name}`) timed out after {timeout_ms}ms"
        )
    })?
    .with_context(|| {
        format!("LLM call to `{provider_str}` (`{requested_model_name}`) failed")
    })?;

    let structured_val = match &schema_spec {
        Some((spec_name, _)) => {
            let parsed = parse_structured_json(&outcome.text).with_context(|| {
                format!(
                    "LLM `{requested_model_name}` did not return valid JSON for schema `{spec_name}`"
                )
            })?;
            report_progress(ProgressKind::Artifact, format!("{spec_name}: {parsed}"));
            parsed
        }
        None => Value::Null,
    };

    let prompt_tokens = outcome.usage.prompt_tokens.unwrap_or(0);
    let completion_tokens = outcome.usage.completion_tokens.unwrap_or(0);
    let total_tokens = outcome
        .usage
        .total_tokens
        .unwrap_or(prompt_tokens + completion_tokens);

    Ok(json!({
        "model": outcome.reported_model.unwrap_or(requested_model_name),
        "provider": provider_str,
        "text": outcome.text,
        "structured": structured_val,
        "reasoning": outcome.reasoning,
        "stop_reason": outcome.stop_reason.map(|r| r.raw().to_owned()),
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        },
    }))
}

struct ChatOutcome {
    reported_model: Option<String>,
    text: String,
    reasoning: Option<String>,
    stop_reason: Option<StopReason>,
    usage: Usage,
}

async fn run_oneshot_chat(
    client: &Client,
    target: genai::ServiceTarget,
    chat_req: ChatRequest,
    options: &ChatOptions,
) -> anyhow::Result<ChatOutcome> {
    let res = client.exec_chat(target, chat_req, Some(options)).await?;
    let reported_model = Some(res.provider_model_iden.model_name.to_string());
    let reasoning = res.reasoning_content.clone().filter(|s| !s.is_empty());
    let stop_reason = res.stop_reason.clone();
    let usage = res.usage.clone();
    let text = res.into_texts().join("");

    if let Some(thought) = &reasoning {
        report_progress(ProgressKind::AgentThought, thought.clone());
    }
    if !text.is_empty() {
        report_progress(ProgressKind::AgentText, text.clone());
    }

    Ok(ChatOutcome {
        reported_model,
        text,
        reasoning,
        stop_reason,
        usage,
    })
}

async fn run_streaming_chat(
    client: &Client,
    target: genai::ServiceTarget,
    chat_req: ChatRequest,
    options: &ChatOptions,
) -> anyhow::Result<ChatOutcome> {
    let mut stream_res = client
        .exec_chat_stream(target, chat_req, Some(options))
        .await?;
    let reported_model = Some(stream_res.model_iden.model_name.to_string());

    let mut streamed_text = String::new();
    let mut streamed_reasoning = String::new();
    let mut end_usage: Option<Usage> = None;
    let mut end_stop_reason: Option<StopReason> = None;
    let mut end_text: Option<String> = None;
    let mut end_reasoning: Option<String> = None;

    while let Some(event_res) = stream_res.stream.next().await {
        let event = event_res?;
        match event {
            ChatStreamEvent::Start
            | ChatStreamEvent::Heartbeat
            | ChatStreamEvent::ThoughtSignatureChunk(_)
            | ChatStreamEvent::ToolCallChunk(_) => {}
            ChatStreamEvent::Chunk(chunk) => {
                if !chunk.content.is_empty() {
                    streamed_text.push_str(&chunk.content);
                    report_progress(ProgressKind::AgentText, chunk.content);
                }
            }
            ChatStreamEvent::ReasoningChunk(chunk) => {
                if !chunk.content.is_empty() {
                    streamed_reasoning.push_str(&chunk.content);
                    report_progress(ProgressKind::AgentThought, chunk.content);
                }
            }
            ChatStreamEvent::End(end) => {
                end_usage = end.captured_usage.clone();
                end_stop_reason = end.captured_stop_reason.clone();
                if let Some(content) = &end.captured_content {
                    let joined = content.texts().join("");
                    if !joined.is_empty() {
                        end_text = Some(joined);
                    }
                }
                if let Some(r) = end.captured_reasoning_content.filter(|s| !s.is_empty()) {
                    end_reasoning = Some(r);
                }
            }
        }
    }

    let final_text = end_text.unwrap_or(streamed_text);
    let final_reasoning =
        end_reasoning.or((!streamed_reasoning.is_empty()).then_some(streamed_reasoning));

    Ok(ChatOutcome {
        reported_model,
        text: final_text,
        reasoning: final_reasoning,
        stop_reason: end_stop_reason,
        usage: end_usage.unwrap_or_default(),
    })
}

fn parse_model_spec(
    raw: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<ParsedModelSpec> {
    if let Some(model_str) = raw.as_str() {
        let trimmed = model_str.trim();
        if trimmed.is_empty() {
            bail!("`invoke_llm` `model` string must not be empty");
        }
        return Ok(ParsedModelSpec {
            model: trimmed.to_owned(),
            api_key: None,
            endpoint: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            reasoning_effort: None,
        });
    }

    let obj = raw.as_object().ok_or_else(|| {
        anyhow::anyhow!("`invoke_llm` `model` must be a `str` or `LlmModel` record")
    })?;
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`LlmModel` missing non-empty `model` field"))?
        .to_owned();

    let api_key = resolve_optional_secret_value(obj.get("api_key"), "api_key", scrubber)?;
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let temperature = match obj.get("temperature") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| anyhow::anyhow!("`LlmModel.temperature` must be a number"))?,
        ),
    };
    let max_tokens = match obj.get("max_tokens") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let n = v
                .as_u64()
                .filter(|&n| n > 0 && n <= u64::from(u32::MAX))
                .ok_or_else(|| {
                    anyhow::anyhow!("`LlmModel.max_tokens` must be a positive 32-bit integer")
                })?;
            Some(n as u32)
        }
    };
    let top_p = match obj.get("top_p") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| anyhow::anyhow!("`LlmModel.top_p` must be a number"))?,
        ),
    };
    let reasoning_effort = match obj.get("reasoning_effort") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let kw = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("`LlmModel.reasoning_effort` must be a string"))?
                .trim()
                .to_ascii_lowercase();
            Some(ReasoningEffort::from_keyword(&kw).ok_or_else(|| {
                anyhow::anyhow!(
                    "unsupported `reasoning_effort` `{kw}` (expected `zero`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`)"
                )
            })?)
        }
    };

    Ok(ParsedModelSpec {
        model,
        api_key,
        endpoint,
        temperature,
        max_tokens,
        top_p,
        reasoning_effort,
    })
}

fn resolve_optional_secret_value(
    val: Option<&Value>,
    field_label: &str,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Option<SecretString>> {
    let Some(v) = val else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    if let Some(s) = v.as_str() {
        if s.is_empty() {
            return Ok(None);
        }
        return Ok(Some(SecretString::from(s.to_owned())));
    }
    if let Some(desc) = parse_secret_descriptor(v)? {
        return resolve_secret_descriptor(&desc, scrubber);
    }
    bail!("expected string, `secret(...)`, or `None` for `{field_label}`, got `{v}`")
}

fn parse_schema_spec(raw: Option<&Value>) -> anyhow::Result<Option<(String, Value)>> {
    let Some(val) = raw else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let obj = val
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`invoke_llm` `schema` descriptor must be an object"))?;
    if let (Some(name), Some(schema)) = (
        obj.get("name").and_then(Value::as_str),
        obj.get("schema"),
    ) {
        return Ok(Some((name.to_owned(), schema.clone())));
    }
    // Allow direct raw JSON Schema object if passed via custom host input.
    Ok(Some(("structured_output".to_owned(), val.clone())))
}

fn render_state_context_block(machine_ctx: &Value, explicit_ctx: Option<&Value>) -> Option<String> {
    let target = match explicit_ctx {
        None | Some(Value::Null) | Some(Value::Bool(false)) => return None,
        Some(Value::Bool(true)) => machine_ctx,
        Some(other) => other,
    };
    let body = match target {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    Some(format!("<context>\n{body}\n</context>"))
}

/// Ensures an explicit `endpoint` base URL ends its path with `/` so `reqwest::Url::join`
/// appends provider routes (`chat/completions`, `messages`, etc.) beneath the path prefix
/// (e.g. `http://127.0.0.1:11434/v1` -> `http://127.0.0.1:11434/v1/`).
fn normalize_base_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.split_once('?') {
        Some((path_part, query)) => {
            if path_part.ends_with('/') {
                trimmed.to_owned()
            } else {
                format!("{path_part}/?{query}")
            }
        }
        None => {
            if trimmed.ends_with('/') {
                trimmed.to_owned()
            } else {
                format!("{trimmed}/")
            }
        }
    }
}

/// Parses structured JSON text from the model, tolerating optional surrounding markdown code fences.
fn parse_structured_json(raw: &str) -> anyhow::Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("model returned empty text instead of JSON");
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    if let Some(fenced) = strip_markdown_json_fence(trimmed)
        && let Ok(v) = serde_json::from_str::<Value>(fenced)
    {
        return Ok(v);
    }
    let err = serde_json::from_str::<Value>(trimmed).unwrap_err();
    Err(anyhow::anyhow!("invalid JSON response `{trimmed}`: {err}"))
}

fn strip_markdown_json_fence(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("```")?.strip_suffix("```")?.trim();
    let without_lang = inner
        .strip_prefix("json")
        .or_else(|| inner.strip_prefix("JSON"))
        .unwrap_or(inner);
    Some(without_lang.trim())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn normalizes_base_endpoint_trailing_slash_and_strips_json_fences() {
        assert_eq!(
            normalize_base_endpoint("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434/v1/"
        );
        assert_eq!(
            normalize_base_endpoint("http://127.0.0.1:11434/v1/"),
            "http://127.0.0.1:11434/v1/"
        );
        assert_eq!(
            normalize_base_endpoint("https://proxy.example.com/v1?api-version=2025-01"),
            "https://proxy.example.com/v1/?api-version=2025-01"
        );

        let fenced = "```json\n{\"headline\": \"v1.0\", \"breaking\": false}\n```";
        let parsed = parse_structured_json(fenced).expect("fenced JSON should parse");
        assert_eq!(parsed["headline"], "v1.0");
        assert_eq!(parsed["breaking"], false);
    }

    #[tokio::test]
    async fn executes_streaming_structured_llm_call_against_mock_openai_and_scrubs_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // Request 1: SSE streaming structured output
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let req_text = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(
                req_text.starts_with("POST /v1/chat/completions "),
                "expected /v1/chat/completions, got:\n{req_text}"
            );
            assert!(
                req_text.to_ascii_lowercase().contains("authorization: bearer sk-llm-unit-secret"),
                "expected bearer header in:\n{req_text}"
            );
            assert!(
                req_text.contains("\"json_schema\"") && req_text.contains("\"ReleaseSummary\""),
                "expected JsonSpec response_format in:\n{req_text}"
            );

            let sse_body = concat!(
                "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini-2024-07-18\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Checking changelog...\"}}]}\n\n",
                "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini-2024-07-18\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"headline\\\":\\\"Ship @starness/llm\\\"\"}}]}\n\n",
                "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o-mini-2024-07-18\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\",\\\"breaking\\\":false}\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":12,\"total_tokens\":54}}\n\n",
                "data: [DONE]\n\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;

            // Request 2: 401 echoing secret back -> must be scrubbed!
            let (mut sock2, _) = listener.accept().await.unwrap();
            let _ = sock2.read(&mut buf).await.unwrap();
            let err_body = "{\"error\":{\"message\":\"Invalid key sk-llm-unit-secret\"}}";
            let err_resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                err_body.len(),
                err_body
            );
            sock2.write_all(err_resp.as_bytes()).await.unwrap();
            let _ = sock2.shutdown().await;
        });

        let secret_file = std::env::temp_dir().join(format!(
            "starness_llm_key_{}.txt",
            std::process::id()
        ));
        std::fs::write(&secret_file, "sk-llm-unit-secret\n").unwrap();

        let input = json!({
            "model": {
                "model": "openai::gpt-4o-mini",
                "api_key": {
                    "$starness_secret": {
                        "source": "file",
                        "name": secret_file.to_string_lossy(),
                        "prefix": "",
                        "optional": false
                    }
                },
                "endpoint": format!("http://{addr}/v1"),
                "temperature": 0.1
            },
            "system": "Extract structured JSON.",
            "prompt": "Summarize release",
            "context": {"tag": "v0.2.0"},
            "schema": {
                "name": "ReleaseSummary",
                "schema": {
                    "type": "object",
                    "properties": {
                        "headline": {"type": "string"},
                        "breaking": {"type": "boolean"}
                    },
                    "required": ["headline", "breaking"],
                    "additionalProperties": false
                }
            },
            "stream": true,
            "timeout_ms": 10000
        });

        let out = execute_llm(json!({}), input.clone())
            .await
            .expect("streaming structured LLM call should succeed");
        assert_eq!(out["provider"], "openai");
        assert_eq!(out["structured"]["headline"], "Ship @starness/llm");
        assert_eq!(out["structured"]["breaking"], false);
        assert_eq!(out["reasoning"], "Checking changelog...");
        assert_eq!(out["usage"]["prompt_tokens"], 42);
        assert_eq!(out["usage"]["completion_tokens"], 12);
        assert_eq!(out["usage"]["total_tokens"], 54);

        let mut non_stream_input = input;
        non_stream_input["stream"] = json!(false);
        let err = execute_llm(json!({}), non_stream_input)
            .await
            .expect_err("401 should fail");
        let _ = std::fs::remove_file(&secret_file);
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("sk-llm-unit-secret"),
            "secret leaked in error message: {msg}"
        );

        server.await.unwrap();
    }
}

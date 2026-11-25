//! Built-in `@starness/llm` runtime driver powered by `genai`.
//!
//! Executes single-shot or multi-turn stateless chat requests, native tool calling,
//! constrained-decoding structured `Record` / JSON Schema extraction, binary attachments,
//! prompt caching, and lossless reasoning/thought-signature round-tripping across all `genai`
//! providers, with streaming [`TraceHub`](crate::TraceHub) token events and `@starness/secrets`
//! resolution and redaction.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use genai::Client;
use genai::chat::{
    Binary, CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat,
    ChatStreamEvent, ContentPart, JsonSpec, MessageContent, MessageOptions, ReasoningEffort,
    StopReason, Tool, ToolCall, ToolChoice, ToolResponse, Usage,
};
use genai::resolver::{AuthData, Endpoint};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::builtins::secrets::{
    SecretScrubber, parse_secret_descriptor, resolve_secret_descriptor, resolve_secrets_in_value,
};
use crate::trace::{ProgressKind, report_progress};

/// Wrapper key for lossless round-tripping of serialized `genai::chat::ChatMessage`s in Starlark context.
const LLM_MSG_ENVELOPE_KEY: &str = "$starness_llm_msg";

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
    let system_cache = parse_optional_cache(obj.get("system_cache"))?;

    let prompt_opt = match obj.get("prompt") {
        None | Some(Value::Null) => None,
        Some(prompt_val) => {
            let resolved_prompt = resolve_secrets_in_value(prompt_val, scrubber)?;
            let s = resolved_prompt.as_str().ok_or_else(|| {
                anyhow::anyhow!("`invoke_llm` `prompt` must evaluate to a string or None")
            })?;
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
    };

    let explicit_ctx = match obj.get("context") {
        None | Some(Value::Null) => None,
        Some(c) => Some(resolve_secrets_in_value(c, scrubber)?),
    };
    let ctx_block_opt = render_state_context_block(machine_ctx, explicit_ctx.as_ref());

    let mut chat_req = ChatRequest::default();
    if let Some(sys) = system_opt {
        if let Some(cache) = system_cache {
            chat_req = chat_req
                .append_message(ChatMessage::system(sys).with_options(MessageOptions::from(cache)));
        } else {
            chat_req = chat_req.with_system(sys);
        }
    }

    let mut has_history_messages = false;
    if let Some(msgs_val) = obj.get("messages").filter(|v| !v.is_null()) {
        let resolved_msgs = resolve_secrets_in_value(msgs_val, scrubber)?;
        let arr = resolved_msgs.as_array().ok_or_else(|| {
            anyhow::anyhow!("`invoke_llm` `messages` must evaluate to a list of message dicts")
        })?;
        if !arr.is_empty() {
            has_history_messages = true;
            if prompt_opt.is_none()
                && let Some(ctx_block) = ctx_block_opt.as_ref()
            {
                chat_req = chat_req.append_message(ChatMessage::user(ctx_block.clone()));
            }
            for (idx, item) in arr.iter().enumerate() {
                let msg = parse_chat_message(item)
                    .with_context(|| format!("invalid `messages[{idx}]` in `invoke_llm`"))?;
                chat_req = chat_req.append_message(msg);
            }
        }
    }

    if let Some(prompt_str) = prompt_opt {
        let user_content = match ctx_block_opt {
            Some(ctx_block) => format!("{ctx_block}\n\n{prompt_str}"),
            None => prompt_str,
        };
        chat_req = chat_req.append_message(ChatMessage::user(user_content));
    } else if !has_history_messages {
        bail!("`invoke_llm` requires a non-empty `prompt` or non-empty `messages` list");
    }

    if let Some(tools_val) = obj.get("tools").filter(|v| !v.is_null()) {
        let tools_arr = tools_val
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("`invoke_llm` `tools` must be a list"))?;
        if !tools_arr.is_empty() {
            let mut parsed_tools = Vec::with_capacity(tools_arr.len());
            for (idx, item) in tools_arr.iter().enumerate() {
                parsed_tools.push(
                    parse_tool_def(item)
                        .with_context(|| format!("invalid `tools[{idx}]` in `invoke_llm`"))?,
                );
            }
            chat_req = chat_req.with_tools(parsed_tools);
        }
    }

    let tool_choice_opt = parse_optional_tool_choice(obj.get("tool_choice"))?;
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

    let mut options = ChatOptions::default()
        .with_capture_usage(true)
        .with_capture_content(true)
        .with_capture_reasoning_content(true)
        .with_capture_tool_calls(true);
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
    if let Some(tc) = tool_choice_opt {
        options = options.with_tool_choice(tc);
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

    let is_tool_turn = !outcome.tool_calls.is_empty()
        || matches!(outcome.stop_reason, Some(StopReason::ToolCall(_)));
    let structured_val = match (&schema_spec, is_tool_turn) {
        (Some((spec_name, _)), false) => {
            let parsed = parse_structured_json(&outcome.text).with_context(|| {
                format!(
                    "LLM `{requested_model_name}` did not return valid JSON for schema `{spec_name}`"
                )
            })?;
            report_progress(ProgressKind::Artifact, format!("{spec_name}: {parsed}"));
            parsed
        }
        _ => Value::Null,
    };

    let tool_calls_json: Vec<Value> = outcome
        .tool_calls
        .iter()
        .map(|tc| {
            json!({
                "id": tc.call_id,
                "name": tc.fn_name,
                "args": tc.fn_arguments,
            })
        })
        .collect();

    let message_envelope = json!({
        (LLM_MSG_ENVELOPE_KEY): serde_json::to_value(&outcome.assistant_message)
            .context("failed serializing assistant ChatMessage for round-tripping")?,
    });

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
        "tool_calls": tool_calls_json,
        "message": message_envelope,
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
    tool_calls: Vec<ToolCall>,
    assistant_message: ChatMessage,
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
    let text = res.texts().join("");
    let tool_calls: Vec<ToolCall> = res.tool_calls().into_iter().cloned().collect();

    if let Some(thought) = &reasoning {
        report_progress(ProgressKind::AgentThought, thought.clone());
    }
    if !text.is_empty() {
        report_progress(ProgressKind::AgentText, text.clone());
    }
    for tc in &tool_calls {
        report_progress(
            ProgressKind::ToolCall,
            format!("{}({})", tc.fn_name, tc.fn_arguments),
        );
    }

    let contains_reasoning = res.content.contains_reasoning_content();
    let mut assistant_message = ChatMessage::assistant(res.content);
    if !contains_reasoning {
        assistant_message = assistant_message.with_reasoning_content(reasoning.clone());
    }

    Ok(ChatOutcome {
        reported_model,
        text,
        reasoning,
        stop_reason,
        tool_calls,
        assistant_message,
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
    let mut streamed_thought_sigs: Vec<String> = Vec::new();
    let mut streamed_tool_calls: Vec<ToolCall> = Vec::new();
    let mut end_usage: Option<Usage> = None;
    let mut end_stop_reason: Option<StopReason> = None;
    let mut end_content: Option<MessageContent> = None;
    let mut end_reasoning: Option<String> = None;

    while let Some(event_res) = stream_res.stream.next().await {
        let event = event_res?;
        match event {
            ChatStreamEvent::Start | ChatStreamEvent::Heartbeat => {}
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
            ChatStreamEvent::ThoughtSignatureChunk(chunk) => {
                if !chunk.content.is_empty() {
                    streamed_thought_sigs.push(chunk.content);
                }
            }
            ChatStreamEvent::ToolCallChunk(tc_chunk) => {
                let tc = tc_chunk.tool_call;
                report_progress(
                    ProgressKind::ToolCall,
                    format!("{}({})", tc.fn_name, tc.fn_arguments),
                );
                streamed_tool_calls.push(tc);
            }
            ChatStreamEvent::End(end) => {
                end_usage = end.captured_usage.clone();
                end_stop_reason = end.captured_stop_reason.clone();
                end_content = end.captured_content;
                if let Some(r) = end.captured_reasoning_content.filter(|s| !s.is_empty()) {
                    end_reasoning = Some(r);
                }
            }
        }
    }

    let final_reasoning =
        end_reasoning.or((!streamed_reasoning.is_empty()).then_some(streamed_reasoning));

    let (final_text, final_tool_calls, assistant_message) = if let Some(content) = end_content {
        let joined = content.texts().join("");
        let text = if joined.is_empty() {
            streamed_text
        } else {
            joined
        };
        let calls: Vec<ToolCall> = content.tool_calls().into_iter().cloned().collect();
        let final_calls = if calls.is_empty() {
            streamed_tool_calls
        } else {
            calls
        };
        let contains_reasoning = content.contains_reasoning_content();
        let mut msg = ChatMessage::assistant(content);
        if !contains_reasoning {
            msg = msg.with_reasoning_content(final_reasoning.clone());
        }
        (text, final_calls, msg)
    } else {
        let mut parts: Vec<ContentPart> = Vec::new();
        for sig in streamed_thought_sigs {
            parts.push(ContentPart::ThoughtSignature(sig));
        }
        if let Some(r) = &final_reasoning {
            parts.push(ContentPart::ReasoningContent(r.clone()));
        }
        if !streamed_text.is_empty() {
            parts.push(ContentPart::from_text(streamed_text.clone()));
        }
        for tc in &streamed_tool_calls {
            parts.push(ContentPart::ToolCall(tc.clone()));
        }
        let msg = ChatMessage::assistant(MessageContent::from_parts(parts));
        (streamed_text, streamed_tool_calls, msg)
    };

    Ok(ChatOutcome {
        reported_model,
        text: final_text,
        reasoning: final_reasoning,
        stop_reason: end_stop_reason,
        tool_calls: final_tool_calls,
        assistant_message,
        usage: end_usage.unwrap_or_default(),
    })
}

fn parse_optional_cache(raw: Option<&Value>) -> anyhow::Result<Option<CacheControl>> {
    let Some(val) = raw else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let s = val
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("`cache` policy must be a string, got `{val}`"))?
        .trim()
        .to_ascii_lowercase();
    let parsed = match s.as_str() {
        "ephemeral" => CacheControl::Ephemeral,
        "memory" => CacheControl::Memory,
        "5m" => CacheControl::Ephemeral5m,
        "1h" => CacheControl::Ephemeral1h,
        "24h" => CacheControl::Ephemeral24h,
        other => bail!(
            "unsupported `cache` policy `{other}` (expected `ephemeral`, `memory`, `5m`, `1h`, or `24h`)"
        ),
    };
    Ok(Some(parsed))
}

fn parse_optional_tool_choice(raw: Option<&Value>) -> anyhow::Result<Option<ToolChoice>> {
    let Some(val) = raw else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let s = val
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("`tool_choice` must be a string, got `{val}`"))?
        .trim();
    if s.is_empty() {
        bail!("`tool_choice` must not be empty");
    }
    let tc = match s.to_ascii_lowercase().as_str() {
        "auto" => ToolChoice::Auto,
        "none" => ToolChoice::None,
        "required" => ToolChoice::Required,
        _ => ToolChoice::tool(s),
    };
    Ok(Some(tc))
}

fn parse_tool_def(raw: &Value) -> anyhow::Result<Tool> {
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool definition must be a dict created by `tool_def(...)`"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("tool definition missing non-empty `name`"))?;
    let mut tool = Tool::new(name);
    if let Some(desc) = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        tool = tool.with_description(desc);
    }
    if let Some(schema_val) = obj.get("schema").filter(|v| !v.is_null()) {
        let json_schema = schema_val
            .get("schema")
            .cloned()
            .unwrap_or_else(|| schema_val.clone());
        tool = tool.with_schema(json_schema);
    }
    if let Some(strict) = obj.get("strict").and_then(Value::as_bool) {
        tool = tool.with_strict(strict);
    }
    if let Some(cache) = parse_optional_cache(obj.get("cache"))? {
        tool = tool.with_cache_control(cache);
    }
    Ok(tool)
}

fn parse_chat_message(raw: &Value) -> anyhow::Result<ChatMessage> {
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("chat message must be a dict"))?;

    let cache_opt = parse_optional_cache(obj.get("cache"))?;

    // Lossless round-tripped `genai::chat::ChatMessage` produced by `assistant_msg(ev)`
    // (either legacy `$starness_llm_msg` dict envelope or typed `LlmMessage.raw`).
    if let Some(native_val) = obj
        .get(LLM_MSG_ENVELOPE_KEY)
        .or_else(|| obj.get("raw"))
        .filter(|v| v.is_object())
    {
        let mut msg: ChatMessage = serde_json::from_value(native_val.clone())
            .context("failed deserializing round-tripped `$starness_llm_msg`")?;
        if let Some(cache) = cache_opt {
            msg = msg.with_options(MessageOptions::from(cache));
        }
        return Ok(msg);
    }

    let role = obj
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("chat message missing `role`"))?
        .trim()
        .to_ascii_lowercase();

    let mut msg = match role.as_str() {
        "system" => {
            let text = obj
                .get("text")
                .or_else(|| obj.get("content"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("system message missing `text`"))?;
            ChatMessage::system(text)
        }
        "assistant" => {
            let text = obj
                .get("text")
                .or_else(|| obj.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            ChatMessage::assistant(text)
        }
        "user" => {
            let mut parts: Vec<ContentPart> = Vec::new();
            if let Some(text) = obj
                .get("text")
                .or_else(|| obj.get("content"))
                .and_then(Value::as_str)
                && !text.is_empty()
            {
                parts.push(ContentPart::from_text(text));
            }
            if let Some(attachments_val) = obj.get("attachments").filter(|v| !v.is_null()) {
                let arr = attachments_val
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("user message `attachments` must be a list"))?;
                for (i, att) in arr.iter().enumerate() {
                    parts.push(
                        parse_attachment(att)
                            .with_context(|| format!("invalid attachment[{i}]"))?,
                    );
                }
            }
            if parts.is_empty() {
                bail!("user message must contain non-empty `text` or at least one attachment");
            }
            ChatMessage::user(MessageContent::from_parts(parts))
        }
        "tool" => {
            if let Some(results_val) = obj.get("results").filter(|v| !v.is_null()) {
                let arr = results_val.as_array().ok_or_else(|| {
                    anyhow::anyhow!("tool message `results` must be a list of tool_result dicts")
                })?;
                if arr.is_empty() {
                    bail!("tool_results_msg `results` must not be empty");
                }
                let mut parts = Vec::with_capacity(arr.len());
                for (i, item) in arr.iter().enumerate() {
                    let tr = parse_single_tool_response(item)
                        .with_context(|| format!("invalid tool_result[{i}]"))?;
                    parts.push(ContentPart::ToolResponse(tr));
                }
                ChatMessage::tool(MessageContent::from_parts(parts))
            } else {
                let tr = parse_single_tool_response(raw)?;
                ChatMessage::tool(tr)
            }
        }
        other => bail!("unsupported chat message `role` `{other}`"),
    };

    if let Some(cache) = cache_opt {
        msg = msg.with_options(MessageOptions::from(cache));
    }
    Ok(msg)
}

fn parse_single_tool_response(raw: &Value) -> anyhow::Result<ToolResponse> {
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_result must be a dict"))?;
    let call_id = obj
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("tool_result missing non-empty `call_id`"))?;
    let content_val = obj
        .get("content")
        .ok_or_else(|| anyhow::anyhow!("tool_result missing `content`"))?;
    let content_str = match content_val {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    };
    let mut tr = ToolResponse::new(call_id, content_str);
    if let Some(fn_name) = obj
        .get("fn_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        tr = tr.with_fn_name(fn_name);
    }
    Ok(tr)
}

fn parse_attachment(raw: &Value) -> anyhow::Result<ContentPart> {
    let obj = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("attachment must be a dict created by `attachment(...)`"))?;
    let path_opt = obj
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let b64_opt = obj
        .get("base64")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let url_opt = obj
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mime_opt = obj
        .get("mime")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let name_opt = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    match (path_opt, b64_opt, url_opt) {
        (Some(path), None, None) => {
            let mut bin = Binary::from_file(path)
                .with_context(|| format!("failed reading attachment file `{path}`"))?;
            if let Some(mime) = mime_opt {
                bin.content_type = mime.to_owned();
            }
            if let Some(name) = name_opt {
                bin.name = Some(name);
            }
            Ok(ContentPart::Binary(bin))
        }
        (None, Some(b64), None) => {
            let mime = mime_opt
                .ok_or_else(|| anyhow::anyhow!("base64 attachment requires non-empty `mime`"))?;
            Ok(ContentPart::from_binary_base64(mime, b64, name_opt))
        }
        (None, None, Some(url)) => {
            let mime = mime_opt
                .ok_or_else(|| anyhow::anyhow!("url attachment requires non-empty `mime`"))?;
            Ok(ContentPart::from_binary_url(mime, url, name_opt))
        }
        _ => bail!("attachment requires exactly one of `path`, `base64`, or `url`"),
    }
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
/// appends provider routes (`chat/completions`, `messages`, etc.) beneath the path prefix.
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

    #[test]
    fn round_trips_thought_signatures_attachments_and_cache_control() {
        let tc = ToolCall {
            call_id: "call_1".to_owned(),
            fn_name: "bash".to_owned(),
            fn_arguments: json!({"command": "ls"}),
            thought_signatures: Some(vec!["sig_abc_123".to_owned()]),
        };
        let native_msg = ChatMessage::assistant_tool_calls_with_thoughts(
            vec![tc],
            vec!["sig_abc_123".to_owned()],
        );
        let envelope = json!({
            (LLM_MSG_ENVELOPE_KEY): serde_json::to_value(&native_msg).unwrap(),
            "cache": "1h",
        });

        let restored = parse_chat_message(&envelope).expect("round-tripped message should parse");
        assert_eq!(
            restored.options.as_ref().and_then(|o| o.cache_control.clone()),
            Some(CacheControl::Ephemeral1h)
        );
        assert_eq!(restored.content.tool_calls().len(), 1);
        assert_eq!(
            restored.content.tool_calls()[0].thought_signatures.as_deref(),
            Some(&["sig_abc_123".to_owned()][..])
        );

        let user_with_img = json!({
            "role": "user",
            "text": "What is in this screenshot?",
            "attachments": [
                {"base64": "iVBORw0KGgo=", "mime": "image/png", "name": "shot.png"}
            ],
            "cache": "ephemeral",
        });
        let parsed_user = parse_chat_message(&user_with_img).expect("user attachment message");
        assert_eq!(
            parsed_user.options.as_ref().and_then(|o| o.cache_control.clone()),
            Some(CacheControl::Ephemeral)
        );
        assert_eq!(parsed_user.content.len(), 2);

        let batch_tools = json!({
            "role": "tool",
            "results": [
                {"role": "tool", "call_id": "call_1", "fn_name": "bash", "content": {"stdout": "ok"}},
                {"role": "tool", "call_id": "call_2", "fn_name": "read", "content": "hello"}
            ],
            "cache": "5m",
        });
        let parsed_tools = parse_chat_message(&batch_tools).expect("tool_results_msg");
        assert_eq!(parsed_tools.content.len(), 2);
        assert_eq!(
            parsed_tools.options.as_ref().and_then(|o| o.cache_control.clone()),
            Some(CacheControl::Ephemeral5m)
        );
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
        assert!(out["message"][LLM_MSG_ENVELOPE_KEY].is_object());

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

    #[tokio::test]
    async fn executes_multiturn_tool_call_and_round_trips_assistant_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // Turn 1: model responds with a tool call
            let (mut sock1, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n1 = sock1.read(&mut buf).await.unwrap();
            let req1 = String::from_utf8_lossy(&buf[..n1]).into_owned();
            assert!(
                req1.contains("\"tools\"") && req1.contains("\"bash\""),
                "expected bash tool in turn 1:\n{req1}"
            );

            let body1 = json!({
                "id": "chatcmpl-tool-1",
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_bash_42",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\":\"echo hi\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 30, "completion_tokens": 10, "total_tokens": 40}
            })
            .to_string();
            let resp1 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body1}",
                body1.len()
            );
            sock1.write_all(resp1.as_bytes()).await.unwrap();
            let _ = sock1.shutdown().await;

            // Turn 2: verify assistant tool_call + tool response are present on wire!
            let (mut sock2, _) = listener.accept().await.unwrap();
            let n2 = sock2.read(&mut buf).await.unwrap();
            let req2 = String::from_utf8_lossy(&buf[..n2]).into_owned();
            assert!(
                req2.contains("call_bash_42") && req2.contains("hi from bash"),
                "expected round-tripped tool call + tool result in turn 2:\n{req2}"
            );

            let body2 = json!({
                "id": "chatcmpl-tool-2",
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "The command printed: hi from bash"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 55, "completion_tokens": 9, "total_tokens": 64}
            })
            .to_string();
            let resp2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body2}",
                body2.len()
            );
            sock2.write_all(resp2.as_bytes()).await.unwrap();
            let _ = sock2.shutdown().await;
        });

        let base_model = json!({
            "model": "openai::gpt-4o-mini",
            "endpoint": format!("http://{addr}/v1"),
        });
        let tools = json!([{
            "name": "bash",
            "description": "Run a shell command",
            "schema": {
                "name": "BashArgs",
                "schema": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        }]);

        let turn1 = execute_llm(
            json!({}),
            json!({
                "model": base_model,
                "system": "You are a coding assistant.",
                "messages": [{"role": "user", "text": "Run echo hi"}],
                "tools": tools,
                "tool_choice": "auto",
                "stream": false,
                "timeout_ms": 10000
            }),
        )
        .await
        .expect("turn 1 tool call");
        assert_eq!(turn1["tool_calls"][0]["id"], "call_bash_42");
        assert_eq!(turn1["tool_calls"][0]["name"], "bash");
        assert_eq!(turn1["tool_calls"][0]["args"]["command"], "echo hi");

        let turn2 = execute_llm(
            json!({}),
            json!({
                "model": base_model,
                "system": "You are a coding assistant.",
                "messages": [
                    {"role": "user", "text": "Run echo hi"},
                    turn1["message"].clone(),
                    {
                        "role": "tool",
                        "call_id": "call_bash_42",
                        "fn_name": "bash",
                        "content": {"stdout": "hi from bash", "exit_code": 0}
                    }
                ],
                "tools": tools,
                "stream": false,
                "timeout_ms": 10000
            }),
        )
        .await
        .expect("turn 2 final reply");
        assert_eq!(turn2["text"], "The command printed: hi from bash");
        assert_eq!(turn2["tool_calls"], json!([]));

        server.await.unwrap();
    }
}

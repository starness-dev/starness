//! Built-in `@starchart/acp` Agent Client Protocol (ACP v1) client driver and agent server.
//!
//! Supports both:
//! - **ACP Client (`execute_acp_turn`)**: Spawns and drives external ACP agents (`agy acp`,
//!   `opencode acp`, `claude-agent-acp`, `codex-acp`, or `starchart acp <subchart.star>`) over
//!   newline-delimited JSON-RPC 2.0 `stdio`.
//! - **ACP Agent Server (`serve_chart_as_acp_agent`)**: Exposes a `CompiledChart` / `StarchartActor`
//!   over `stdio` as a compliant ACP v1 Agent for IDEs (Zed, JetBrains, Neovim) or parent workflows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use starchart_starlark::CompiledChart;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use xstate_statemachine::core::Status;

use crate::trace::{ProgressKind, progress_enabled, report_progress};
use crate::{HostCallbacks, StarchartActor};

/// Executes a single ACP prompt turn against a configured `AcpAgent` subprocess over `stdio`.
pub async fn execute_acp_turn(_ctx: Value, input: Value) -> anyhow::Result<Value> {
    let obj = input
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`@starchart/acp` input must be an object, got `{input}`"))?;

    let agent = obj
        .get("agent")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("`@starchart/acp` input missing `agent` record"))?;

    let prompt_text = obj
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("`@starchart/acp` `prompt` must resolve to a `str`"))?;

    let protocol = agent
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("ACP");

    let command_prog = agent
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("AcpAgent missing `command`"))?;

    let args: Vec<String> = agent
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let model = agent.get("model").and_then(Value::as_str).unwrap_or("");
    let timeout_ms = agent
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300_000);

    let session_cwd: PathBuf = match agent.get("cwd").and_then(Value::as_str) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let canonical_cwd = std::fs::canonicalize(&session_cwd).unwrap_or(session_cwd);
    let env_map = agent.get("env").and_then(Value::as_object);
    let config_map = agent.get("config").and_then(Value::as_object);

    match protocol {
        "CLAUDE_CLI" | "CODEX_CLI" => {
            // These CLIs return the whole answer at the end, so report it as one chunk.
            report_progress(ProgressKind::Log, format!("running {command_prog} ({protocol})"));
            let result = if protocol == "CLAUDE_CLI" {
                execute_claude_cli_turn(
                    command_prog,
                    &args,
                    &canonical_cwd,
                    model,
                    env_map,
                    config_map,
                    timeout_ms,
                    prompt_text,
                )
                .await
            } else {
                execute_codex_cli_turn(
                    command_prog,
                    &args,
                    &canonical_cwd,
                    model,
                    env_map,
                    config_map,
                    timeout_ms,
                    prompt_text,
                )
                .await
            }?;
            if let Some(text) = result.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                report_progress(ProgressKind::AgentText, text);
            }
            return Ok(result);
        }
        _ => {}
    }

    let mut cmd = Command::new(command_prog);
    cmd.args(&args)
        .current_dir(&canonical_cwd)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if let Some(envs) = agent.get("env").and_then(Value::as_object) {
        for (k, v) in envs {
            if let Some(val_str) = v.as_str() {
                cmd.env(k, val_str);
            }
        }
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn ACP agent `{command_prog}`"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open stdin pipe to ACP agent"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open stdout pipe from ACP agent"))?;

    let config_map = agent.get("config").and_then(Value::as_object).cloned();

    let turn_fut = run_acp_client_session(
        stdin,
        stdout,
        &canonical_cwd,
        model,
        config_map.as_ref(),
        prompt_text,
    );

    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), turn_fut)
        .await
        .map_err(|_| anyhow::anyhow!("ACP agent `{command_prog}` timed out after {timeout_ms}ms"))??;

    let _ = child.kill().await;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn execute_claude_cli_turn(
    command_prog: &str,
    extra_args: &[String],
    cwd: &Path,
    model: &str,
    env_map: Option<&serde_json::Map<String, Value>>,
    config_map: Option<&serde_json::Map<String, Value>>,
    timeout_ms: u64,
    prompt_text: &str,
) -> anyhow::Result<Value> {
    let mut cmd = Command::new(command_prog);
    cmd.arg("-p")
        .arg("--output-format")
        .arg("json")
        .args(extra_args)
        .current_dir(cwd)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    if let Some(cfg) = config_map
        && let Some(perm_mode) = cfg.get("permission_mode").and_then(Value::as_str)
        && !perm_mode.is_empty()
    {
        cmd.arg("--permission-mode").arg(perm_mode);
    }
    if let Some(envs) = env_map {
        for (k, v) in envs {
            if let Some(val_str) = v.as_str() {
                cmd.env(k, val_str);
            }
        }
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn Claude CLI `{command_prog}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt_text.as_bytes()).await?;
        drop(stdin);
    }

    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("Claude CLI `{command_prog}` timed out after {timeout_ms}ms"))?
        .with_context(|| format!("failed waiting on Claude CLI `{command_prog}`"))?;

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let detail = if !stderr_str.trim().is_empty() {
            stderr_str.trim()
        } else {
            stdout_str.trim()
        };
        bail!("Claude CLI exited with {}: {detail}", output.status);
    }

    if let Ok(parsed) = serde_json::from_str::<Value>(stdout_str.trim()) {
        if parsed.get("is_error").and_then(Value::as_bool) == Some(true) {
            let err_msg = parsed
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("Claude CLI reported error");
            bail!("Claude CLI error: {err_msg}");
        }
        let text = parsed
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let session_id = parsed
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return Ok(json!({
            "session_id": session_id,
            "stop_reason": "end_turn",
            "text": text,
            "result": parsed,
        }));
    }

    Ok(json!({
        "session_id": "",
        "stop_reason": "end_turn",
        "text": stdout_str.trim(),
        "result": {},
    }))
}

#[allow(clippy::too_many_arguments)]
async fn execute_codex_cli_turn(
    command_prog: &str,
    extra_args: &[String],
    cwd: &Path,
    model: &str,
    env_map: Option<&serde_json::Map<String, Value>>,
    config_map: Option<&serde_json::Map<String, Value>>,
    timeout_ms: u64,
    prompt_text: &str,
) -> anyhow::Result<Value> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let last_msg_file = std::env::temp_dir().join(format!(
        "starchart_codex_out_{}_{}.txt",
        std::process::id(),
        uniq
    ));

    let mut cmd = Command::new(command_prog);
    cmd.arg("exec")
        .arg("-o")
        .arg(&last_msg_file)
        .args(extra_args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if !model.is_empty() {
        cmd.arg("-m").arg(model);
    }
    if let Some(cfg) = config_map
        && let Some(sandbox_mode) = cfg.get("sandbox").and_then(Value::as_str)
        && !sandbox_mode.is_empty()
    {
        cmd.arg("-s").arg(sandbox_mode);
    }
    cmd.arg("-");

    if let Some(envs) = env_map {
        for (k, v) in envs {
            if let Some(val_str) = v.as_str() {
                cmd.env(k, val_str);
            }
        }
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn Codex CLI `{command_prog}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt_text.as_bytes()).await?;
        drop(stdin);
    }

    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| {
            let _ = std::fs::remove_file(&last_msg_file);
            anyhow::anyhow!("Codex CLI `{command_prog}` timed out after {timeout_ms}ms")
        })?
        .with_context(|| format!("failed waiting on Codex CLI `{command_prog}`"))?;

    let last_msg = std::fs::read_to_string(&last_msg_file).unwrap_or_default();
    let _ = std::fs::remove_file(&last_msg_file);

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let detail = if !stderr_str.trim().is_empty() {
            stderr_str.trim()
        } else {
            stdout_str.trim()
        };
        bail!("Codex CLI exited with {}: {detail}", output.status);
    }

    let text = if !last_msg.trim().is_empty() {
        last_msg.trim().to_owned()
    } else {
        stdout_str.trim().to_owned()
    };

    Ok(json!({
        "session_id": "",
        "stop_reason": "end_turn",
        "text": text,
        "result": {},
    }))
}

async fn run_acp_client_session(
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    cwd: &Path,
    model: &str,
    config_map: Option<&serde_json::Map<String, Value>>,
    prompt_text: &str,
) -> anyhow::Result<Value> {
    let mut reader = BufReader::new(stdout).lines();
    let mut next_id: u64 = 1;
    let mut updates: Vec<Value> = Vec::new();
    let mut accumulated_text = String::new();

    // 1. `initialize`
    let init_id = next_id;
    next_id += 1;
    send_jsonrpc(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": true,
                        "writeTextFile": true,
                    },
                    "terminal": false,
                },
                "clientInfo": {
                    "name": "starchart",
                    "title": "Starchart Statechart Engine",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        }),
    )
    .await?;

    let _init_res = await_rpc_response(
        init_id,
        &mut reader,
        &mut stdin,
        cwd,
        &mut accumulated_text,
        &mut updates,
    )
    .await
    .context("ACP `initialize` failed")?;

    // 2. `session/new`
    let new_sess_id = next_id;
    next_id += 1;
    send_jsonrpc(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": new_sess_id,
            "method": "session/new",
            "params": {
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            },
        }),
    )
    .await?;

    let sess_res = await_rpc_response(
        new_sess_id,
        &mut reader,
        &mut stdin,
        cwd,
        &mut accumulated_text,
        &mut updates,
    )
    .await
    .context("ACP `session/new` failed")?;

    let session_id = sess_res
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ACP `session/new` response missing `sessionId`: {sess_res}"))?
        .to_owned();

    // 3. `session/set_config_option` for `model` + any custom config options.
    if !model.is_empty() {
        let cfg_id = next_id;
        next_id += 1;
        send_jsonrpc(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": cfg_id,
                "method": "session/set_config_option",
                "params": {
                    "sessionId": session_id,
                    "configId": "model",
                    "id": "model",
                    "value": model,
                },
            }),
        )
        .await?;
        if let Err(err) = await_rpc_response(
            cfg_id,
            &mut reader,
            &mut stdin,
            cwd,
            &mut accumulated_text,
            &mut updates,
        )
        .await
        {
            let msg = format!("{err:#}");
            // Tolerate `-32601` (method not implemented), but surface invalid model errors (`-32602`).
            if !msg.contains("-32601") {
                return Err(err.context(format!("failed setting ACP model `{model}`")));
            }
        }
    }

    if let Some(cfg) = config_map {
        for (opt_id, opt_val) in cfg {
            if let Some(val_str) = opt_val.as_str() {
                let req_id = next_id;
                next_id += 1;
                send_jsonrpc(
                    &mut stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "session/set_config_option",
                        "params": {
                            "sessionId": session_id,
                            "configId": opt_id,
                            "id": opt_id,
                            "value": val_str,
                        },
                    }),
                )
                .await?;
                let _ = await_rpc_response(
                    req_id,
                    &mut reader,
                    &mut stdin,
                    cwd,
                    &mut accumulated_text,
                    &mut updates,
                )
                .await;
            }
        }
    }

    // 4. `session/prompt`
    let prompt_req_id = next_id;
    send_jsonrpc(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": prompt_req_id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": prompt_text,
                    }
                ],
            },
        }),
    )
    .await?;

    let prompt_res = await_rpc_response(
        prompt_req_id,
        &mut reader,
        &mut stdin,
        cwd,
        &mut accumulated_text,
        &mut updates,
    )
    .await
    .context("ACP `session/prompt` failed")?;

    let stop_reason = prompt_res
        .get("stopReason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");

    Ok(json!({
        "session_id": session_id,
        "stop_reason": stop_reason,
        "text": accumulated_text,
        "updates": updates,
        "result": prompt_res,
    }))
}

async fn send_jsonrpc<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn await_rpc_response<W: AsyncWrite + Unpin>(
    expected_id: u64,
    reader: &mut tokio::io::Lines<BufReader<ChildStdout>>,
    writer: &mut W,
    cwd: &Path,
    accumulated_text: &mut String,
    updates: &mut Vec<Value>,
) -> anyhow::Result<Value> {
    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        // 1. Notification from agent (`method` without `id`)
        if let Some(method) = msg.get("method").and_then(Value::as_str)
            && msg.get("id").is_none()
        {
            if method == "session/update"
                && let Some(params) = msg.get("params")
                && let Some(update) = params.get("update")
            {
                extract_chunk_text(update, accumulated_text);
                report_acp_update(update);
                updates.push(update.clone());
            }
            continue;
        }

        // 2. Agent-to-client request (`method` + `id`)
        if let (Some(method), Some(req_id)) = (msg.get("method").and_then(Value::as_str), msg.get("id"))
        {
            let reply = handle_agent_client_request(method, msg.get("params"), cwd, req_id).await;
            send_jsonrpc(writer, &reply).await?;
            continue;
        }

        // 3. Response matching `expected_id`
        if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
            if let Some(err) = msg.get("error") {
                bail!("ACP JSON-RPC error: {err}");
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    bail!("ACP agent closed stdout before responding to request id {expected_id}")
}

/// Forwards one ACP `session/update` as trace progress (agent text, thoughts, tool calls, plans).
fn report_acp_update(update: &Value) {
    if !progress_enabled() {
        return;
    }
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let content_text = || {
        update
            .get("content")
            .and_then(|c| c.get("text"))
            .or_else(|| update.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match kind {
        "agent_message_chunk" | "message_chunk" | "" => {
            if let Some(t) = content_text() {
                report_progress(ProgressKind::AgentText, t);
            }
        }
        "agent_thought_chunk" => {
            if let Some(t) = content_text() {
                report_progress(ProgressKind::AgentThought, t);
            }
        }
        "tool_call" | "tool_call_update" => {
            let title = update.get("title").and_then(Value::as_str).unwrap_or("");
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            let id = update.get("toolCallId").and_then(Value::as_str).unwrap_or("");
            let label = if title.is_empty() { id } else { title };
            let chunk = if status.is_empty() {
                label.to_owned()
            } else {
                format!("{label} [{status}]")
            };
            report_progress(ProgressKind::ToolCall, chunk);
        }
        "plan" => {
            let entries = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|e| {
                            let content = e.get("content").and_then(Value::as_str)?;
                            let status = e.get("status").and_then(Value::as_str).unwrap_or("");
                            Some(format!("- [{status}] {content}"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            report_progress(ProgressKind::Plan, entries);
        }
        _ => {}
    }
}

fn extract_chunk_text(update: &Value, out: &mut String) {
    let update_kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if matches!(update_kind, "agent_message_chunk" | "message_chunk" | "") {
        if let Some(text) = update
            .get("content")
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
        {
            out.push_str(text);
        } else if let Some(text) = update.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
}

async fn handle_agent_client_request(
    method: &str,
    params: Option<&Value>,
    cwd: &Path,
    req_id: &Value,
) -> Value {
    match method {
        "fs/read_text_file" => {
            let Some(path_str) = params.and_then(|p| p.get("path")).and_then(Value::as_str) else {
                return jsonrpc_err(req_id, -32602, "missing `path`");
            };
            match resolve_scoped_path(cwd, path_str) {
                Ok(resolved) => match std::fs::read_to_string(&resolved) {
                    Ok(content) => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": { "content": content },
                    }),
                    Err(e) => jsonrpc_err(req_id, -32000, &format!("failed reading file: {e}")),
                },
                Err(e) => jsonrpc_err(req_id, -32001, &e),
            }
        }
        "fs/write_text_file" => {
            let Some(path_str) = params.and_then(|p| p.get("path")).and_then(Value::as_str) else {
                return jsonrpc_err(req_id, -32602, "missing `path`");
            };
            let content = params
                .and_then(|p| p.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match resolve_scoped_path(cwd, path_str) {
                Ok(resolved) => {
                    if let Some(parent) = resolved.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&resolved, content) {
                        Ok(()) => json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": {},
                        }),
                        Err(e) => jsonrpc_err(req_id, -32000, &format!("failed writing file: {e}")),
                    }
                }
                Err(e) => jsonrpc_err(req_id, -32001, &e),
            }
        }
        "session/request_permission" => {
            let first_option_id = params
                .and_then(|p| p.get("options"))
                .and_then(Value::as_array)
                .and_then(|opts| {
                    opts.iter()
                        .find(|o| {
                            o.get("kind")
                                .and_then(Value::as_str)
                                .is_some_and(|k| k.contains("allow"))
                        })
                        .or_else(|| opts.first())
                })
                .and_then(|o| o.get("optionId").or_else(|| o.get("id")))
                .and_then(Value::as_str)
                .unwrap_or("allow");

            json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": first_option_id,
                    }
                }
            })
        }
        _ => jsonrpc_err(req_id, -32601, &format!("unsupported client method `{method}`")),
    }
}

fn resolve_scoped_path(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(requested);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let canonical_root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // For new files that don't exist yet, canonicalize parent.
    let check_path = if joined.exists() {
        std::fs::canonicalize(&joined).map_err(|e| e.to_string())?
    } else if let Some(parent) = joined.parent() {
        let canon_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        canon_parent.join(joined.file_name().unwrap_or_default())
    } else {
        joined.clone()
    };

    if !check_path.starts_with(&canonical_root) {
        return Err(format!(
            "path `{}` is outside session cwd `{}`",
            check_path.display(),
            canonical_root.display()
        ));
    }
    Ok(check_path)
}

fn jsonrpc_err(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

// =============================================================================================
// Upward ACP v1 Agent Server (`starchart acp <workflow.star>`)
// =============================================================================================

struct SessionState {
    _cwd: PathBuf,
    config: HashMap<String, String>,
}

/// Serves a [`CompiledChart`] over `reader`/`writer` (`stdio`) as a compliant ACP v1 Agent.
///
/// Each `session/prompt` turn injects the prompt text into context (`prompt` or `task` field if
/// present) / dispatches a `PROMPT` event, spawns a [`StarchartActor`] with all built-in drivers
/// enabled, streams state transitions via `session/update`, and supports `session/cancel`.
pub async fn serve_chart_as_acp_agent<R, W>(
    chart: CompiledChart,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    let mut session_seq: u64 = 0;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        let req_id = msg.get("id").cloned();
        let params = msg.get("params");

        match method {
            "initialize" => {
                if let Some(id) = req_id {
                    let title = chart
                        .ir
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("Starchart ({})", chart.ir.id));
                    send_jsonrpc(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": 1,
                                "agentCapabilities": {
                                    "loadSession": false,
                                    "promptCapabilities": {},
                                },
                                "agentInfo": {
                                    "name": chart.ir.id,
                                    "title": title,
                                    "version": env!("CARGO_PKG_VERSION"),
                                },
                                "authMethods": [],
                            }
                        }),
                    )
                    .await?;
                }
            }
            "session/new" => {
                if let Some(id) = req_id {
                    session_seq += 1;
                    let session_id = format!("sess_{session_seq}");
                    let cwd = params
                        .and_then(|p| p.get("cwd"))
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
                    sessions.insert(
                        session_id.clone(),
                        SessionState {
                            _cwd: cwd,
                            config: HashMap::new(),
                        },
                    );
                    send_jsonrpc(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "sessionId": session_id,
                                "configOptions": [],
                            }
                        }),
                    )
                    .await?;
                }
            }
            "session/set_config_option" => {
                if let Some(id) = req_id {
                    if let (Some(sess_id), Some(opt_id), Some(val)) = (
                        params.and_then(|p| p.get("sessionId")).and_then(Value::as_str),
                        params
                            .and_then(|p| p.get("configId").or_else(|| p.get("id")))
                            .and_then(Value::as_str),
                        params.and_then(|p| p.get("value")).and_then(Value::as_str),
                    ) && let Some(sess) = sessions.get_mut(sess_id)
                    {
                        sess.config.insert(opt_id.to_owned(), val.to_owned());
                    }
                    send_jsonrpc(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "configOptions": [] },
                        }),
                    )
                    .await?;
                }
            }
            "session/prompt" => {
                let Some(id) = req_id else {
                    continue;
                };
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or("sess_1")
                    .to_owned();
                let prompt_text = extract_prompt_blocks(params);

                match execute_chart_prompt_turn(
                    &chart,
                    &session_id,
                    &prompt_text,
                    &mut lines,
                    &mut writer,
                )
                .await
                {
                    Ok(stop_reason) => {
                        send_jsonrpc(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "stopReason": stop_reason,
                                }
                            }),
                        )
                        .await?;
                    }
                    Err(err) => {
                        send_jsonrpc(
                            &mut writer,
                            &jsonrpc_err(&id, -32000, &format!("{err:#}")),
                        )
                        .await?;
                    }
                }
            }
            "session/cancel" => {
                // Idle cancel notification outside an active `session/prompt` turn; ignore cleanly.
            }
            _ => {
                if let Some(id) = req_id {
                    send_jsonrpc(
                        &mut writer,
                        &jsonrpc_err(&id, -32601, &format!("method `{method}` not found")),
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

fn extract_prompt_blocks(params: Option<&Value>) -> String {
    let Some(blocks) = params.and_then(|p| p.get("prompt")).and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for b in blocks {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            parts.push(t);
        }
    }
    parts.join("\n")
}

async fn send_agent_chunk<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    send_jsonrpc(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "type": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": text,
                    }
                }
            }
        }),
    )
    .await
}

async fn execute_chart_prompt_turn<R, W>(
    base_chart: &CompiledChart,
    session_id: &str,
    prompt_text: &str,
    lines: &mut tokio::io::Lines<BufReader<R>>,
    writer: &mut W,
) -> anyhow::Result<&'static str>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut turn_chart = base_chart.clone();
    if let Value::Object(ctx_map) = &mut turn_chart.ir.context {
        if ctx_map.contains_key("prompt") {
            ctx_map.insert("prompt".to_owned(), Value::String(prompt_text.to_owned()));
        } else if ctx_map.contains_key("task") {
            ctx_map.insert("task".to_owned(), Value::String(prompt_text.to_owned()));
        }
    }

    let actor = StarchartActor::spawn(&turn_chart, HostCallbacks::with_builtins())?;
    let mut rx = actor.subscribe();

    // Also dispatch `PROMPT` in case the initial state transitions on an explicit `PROMPT` event.
    if !actor.is_done() {
        let _ = actor
            .send_with_payload("PROMPT", json!({ "text": prompt_text }))
            .await;
    }

    let mut last_states: Vec<String> = Vec::new();

    loop {
        let view = rx.borrow_and_update().clone();
        if view.state_ids != last_states {
            last_states.clone_from(&view.state_ids);
            let active = actor.active_states().join(", ");
            send_agent_chunk(writer, session_id, &format!("[{}] -> {}\n", turn_chart.ir.id, active))
                .await?;
        }

        if view.status != Status::Running {
            if let Some(err_msg) = view.error {
                bail!("statechart `{}` errored: {err_msg}", turn_chart.ir.id);
            }
            let final_ctx = actor.context().await.unwrap_or(Value::Null);
            let summary = if let Some(s) = view.output.as_str() {
                s.to_owned()
            } else if let Some(s) = view.output.get("text").and_then(Value::as_str) {
                s.to_owned()
            } else if !view.output.is_null() {
                view.output.to_string()
            } else if let Some(s) = final_ctx.get("result").and_then(Value::as_str) {
                s.to_owned()
            } else {
                final_ctx.to_string()
            };
            send_agent_chunk(writer, session_id, &summary).await?;
            actor.stop().await;
            return Ok("end_turn");
        }

        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return Ok("end_turn");
                }
            }
            maybe_line = lines.next_line() => {
                if let Ok(Some(line)) = maybe_line
                    && let Ok(msg) = serde_json::from_str::<Value>(line.trim())
                    && msg.get("method").and_then(Value::as_str) == Some("session/cancel")
                {
                    actor.stop().await;
                    return Ok("cancelled");
                }
            }
        }
    }
}

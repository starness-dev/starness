//! Ephemeral turn-scoped MCP server (`artifact_tool` + `read_tool`) and `stdio` bridge.
//!
//! When an `invoke_agent(...)` state declares `tools = [...]`, the runtime spins up an isolated,
//! turn-scoped local listener backed by the compiled Starlark `CompiledAgentTool` definitions and
//! the machine's state-entry context snapshot. Subprocess agents connect via a lightweight `stdio`
//! MCP bridge (`starchart mcp-bridge --addr ... --token ...`, with a stdlib Python fallback when
//! `starchart-runtime` is embedded outside `starchart-cli`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use starchart_starlark::{
    ArtifactMode, CompiledAgentTool, INVOKE_TOOL_SLOT_KEY, TypeRegistry,
    instantiate_record_with_registry,
};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::OwnedFrozenValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::trace::{self, ProgressKind, ServiceScope, report_progress};

static BRIDGE_TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

const HANDSHAKE_TOKEN_KEY: &str = "__starchart_bridge_token__";
const DEFAULT_MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Minimal stdlib-only Python TCP <-> stdio line relay used only when `starchart-runtime` is
/// embedded in a host binary (such as `cargo test -p starchart-runtime`) where the `starchart`
/// CLI executable is not available on disk.
const PYTHON_STDIO_RELAY: &str = r#"
import json, socket, sys, threading
addr, token = sys.argv[1], sys.argv[2]
host, port = addr.rsplit(":", 1)
sock = socket.create_connection((host, int(port)))
sock.sendall((json.dumps({"__starchart_bridge_token__": token}) + "\n").encode("utf-8"))
def pump_out():
    f = sock.makefile("rb")
    for line in f:
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()
t = threading.Thread(target=pump_out, daemon=True)
t.start()
for line in sys.stdin.buffer:
    sock.sendall(line)
try:
    sock.shutdown(socket.SHUT_WR)
except OSError:
    pass
t.join(timeout=2.0)
"#;

/// Per-invocation Starlark tool evaluation context bound via `tokio::task_local!`.
#[derive(Clone)]
pub(crate) struct ToolRuntimeScope {
    pub tools: Arc<HashMap<String, CompiledAgentTool>>,
    pub type_registry: Arc<TypeRegistry>,
    pub context_record_type: Option<OwnedFrozenValue>,
    pub ctx_snapshot: Value,
}

tokio::task_local! {
    static TOOL_RUNTIME_SCOPE: ToolRuntimeScope;
}

pub(crate) async fn with_tool_runtime_scope<F: std::future::Future>(
    scope: ToolRuntimeScope,
    fut: F,
) -> F::Output {
    TOOL_RUNTIME_SCOPE.scope(scope, fut).await
}

pub(crate) fn current_tool_runtime_scope() -> Option<ToolRuntimeScope> {
    TOOL_RUNTIME_SCOPE.try_with(Clone::clone).ok()
}

/// Command + arguments for launching a `stdio` MCP bridge pointing at a [`TurnMcpServer`].
#[derive(Debug, Clone)]
pub struct BridgeLaunchSpec {
    pub command: String,
    pub args: Vec<String>,
}

impl BridgeLaunchSpec {
    /// Formats an ACP v1 `session/new` `mcpServers` entry (`McpServerStdio`).
    pub fn to_acp_mcp_server(&self) -> Value {
        json!({
            "name": "starchart",
            "command": self.command,
            "args": self.args,
            "env": [],
        })
    }

    /// Formats a Claude CLI `--mcp-config` inline JSON string.
    pub fn to_claude_mcp_config_json(&self) -> String {
        json!({
            "mcpServers": {
                "starchart": {
                    "type": "stdio",
                    "command": self.command,
                    "args": self.args,
                }
            }
        })
        .to_string()
    }

    /// Formats Codex CLI `-c mcp_servers.starchart...` override flags.
    pub fn to_codex_cli_args(&self) -> Vec<String> {
        let cmd_lit = serde_json::to_string(&self.command).unwrap_or_default();
        let args_lit = serde_json::to_string(&self.args).unwrap_or_else(|_| "[]".to_owned());
        vec![
            "-c".to_owned(),
            format!("mcp_servers.starchart.command={cmd_lit}"),
            "-c".to_owned(),
            format!("mcp_servers.starchart.args={args_lit}"),
        ]
    }
}

struct TurnState {
    artifacts: Map<String, Value>,
    submitted: HashSet<String>,
}

struct ServerContext {
    ordered_tools: Vec<CompiledAgentTool>,
    tools_by_name: HashMap<String, CompiledAgentTool>,
    runtime_scope: ToolRuntimeScope,
    trace_scope: Option<ServiceScope>,
    token: String,
    state: Mutex<TurnState>,
}

/// Active turn-scoped MCP server bound on `127.0.0.1:<ephemeral_port>`.
pub struct TurnMcpServer {
    launch_spec: BridgeLaunchSpec,
    ctx: Arc<ServerContext>,
    accept_handle: JoinHandle<()>,
}

impl Drop for TurnMcpServer {
    fn drop(&mut self) {
        self.accept_handle.abort();
    }
}

impl TurnMcpServer {
    /// Starts an ephemeral turn MCP server if `input` declares non-empty `tools`.
    pub async fn start_if_configured(input: &Value) -> anyhow::Result<Option<Self>> {
        let Some(tool_refs) = input.get("tools").and_then(Value::as_array) else {
            return Ok(None);
        };
        if tool_refs.is_empty() {
            return Ok(None);
        }

        let runtime_scope = current_tool_runtime_scope().ok_or_else(|| {
            anyhow::anyhow!(
                "`invoke_agent` declared MCP `tools`, but no compiled Starlark tool scope is active"
            )
        })?;

        let mut ordered_tools = Vec::with_capacity(tool_refs.len());
        let mut tools_by_name = HashMap::with_capacity(tool_refs.len());
        let mut initial_artifacts = Map::new();

        for item in tool_refs {
            let slot_name = item
                .get(INVOKE_TOOL_SLOT_KEY)
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid tool entry in `invoke_agent`: expected tool slot ref, got {item}")
                })?;
            let compiled = runtime_scope
                .tools
                .get(slot_name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing compiled MCP tool slot `{slot_name}`"))?;
            let tool_name = compiled.name().to_owned();
            if tools_by_name.contains_key(&tool_name) {
                bail!("duplicate MCP tool name `{tool_name}` in `invoke_agent`");
            }
            if let CompiledAgentTool::Artifact {
                mode: ArtifactMode::Append,
                ..
            } = &compiled
            {
                initial_artifacts.insert(tool_name.clone(), Value::Array(Vec::new()));
            }
            tools_by_name.insert(tool_name, compiled.clone());
            ordered_tools.push(compiled);
        }

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind ephemeral localhost listener for turn MCP server")?;
        let local_addr = listener
            .local_addr()
            .context("failed to query local MCP listener address")?;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = BRIDGE_TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
        let token = format!("sc-{}-{nanos:x}-{seq}", std::process::id());

        let launch_spec = resolve_bridge_launch_spec(&local_addr.to_string(), &token);
        let trace_scope = trace::current_scope();

        let server_ctx = Arc::new(ServerContext {
            ordered_tools,
            tools_by_name,
            runtime_scope,
            trace_scope,
            token,
            state: Mutex::new(TurnState {
                artifacts: initial_artifacts,
                submitted: HashSet::new(),
            }),
        });

        let accept_ctx = Arc::clone(&server_ctx);
        let accept_handle = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let conn_ctx = Arc::clone(&accept_ctx);
                tokio::spawn(async move {
                    let fut = handle_bridge_connection(stream, Arc::clone(&conn_ctx));
                    match conn_ctx.trace_scope.clone() {
                        Some(scope) => {
                            let _ = trace::with_async_scope(scope, fut).await;
                        }
                        None => {
                            let _ = fut.await;
                        }
                    }
                });
            }
        });

        Ok(Some(Self {
            launch_spec,
            ctx: server_ctx,
            accept_handle,
        }))
    }

    pub fn launch_spec(&self) -> &BridgeLaunchSpec {
        &self.launch_spec
    }

    /// Returns formatted names (`\`submit_plan\``) of `required = True` artifact tools not yet submitted.
    pub fn missing_required_tools(&self) -> Vec<String> {
        let guard = self
            .ctx
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut missing = Vec::new();
        for tool in &self.ctx.ordered_tools {
            if let CompiledAgentTool::Artifact {
                name,
                required: true,
                ..
            } = tool
                && !guard.submitted.contains(name)
            {
                missing.push(format!("`{name}`"));
            }
        }
        missing
    }

    /// Shuts down the listener, verifies all `required = True` artifact tools were submitted,
    /// and returns the collected `artifacts` object for `event["output"]["artifacts"]`.
    pub fn finalize(self) -> anyhow::Result<Map<String, Value>> {
        self.accept_handle.abort();
        let missing = self.missing_required_tools();
        if !missing.is_empty() {
            bail!(
                "agent finished turn without calling required artifact tool(s): {}",
                missing.join(", ")
            );
        }
        let guard = self
            .ctx
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(guard.artifacts.clone())
    }
}

fn resolve_bridge_launch_spec(addr: &str, token: &str) -> BridgeLaunchSpec {
    if let Ok(explicit) = std::env::var("STARCHART_MCP_BRIDGE_BIN") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return BridgeLaunchSpec {
                command: trimmed.to_owned(),
                args: vec![
                    "mcp-bridge".to_owned(),
                    "--addr".to_owned(),
                    addr.to_owned(),
                    "--token".to_owned(),
                    token.to_owned(),
                ],
            };
        }
    }

    if let Some(starchart_bin) = discover_starchart_cli_binary() {
        return BridgeLaunchSpec {
            command: starchart_bin.to_string_lossy().into_owned(),
            args: vec![
                "mcp-bridge".to_owned(),
                "--addr".to_owned(),
                addr.to_owned(),
                "--token".to_owned(),
                token.to_owned(),
            ],
        };
    }

    BridgeLaunchSpec {
        command: "python3".to_owned(),
        args: vec![
            "-u".to_owned(),
            "-c".to_owned(),
            PYTHON_STDIO_RELAY.to_owned(),
            addr.to_owned(),
            token.to_owned(),
        ],
    }
}

fn discover_starchart_cli_binary() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let bin_name = if cfg!(windows) {
        "starchart.exe"
    } else {
        "starchart"
    };
    if current.file_name().and_then(|n| n.to_str()) == Some(bin_name) {
        return Some(current);
    }
    // Check sibling (e.g. `target/debug/starchart`) or parent dir (when running inside `target/debug/deps/...`).
    let dir = current.parent()?;
    let sibling = dir.join(bin_name);
    if sibling.is_file() {
        return Some(sibling);
    }
    let parent_sibling = dir.parent()?.join(bin_name);
    if parent_sibling.is_file() {
        return Some(parent_sibling);
    }
    None
}

async fn handle_bridge_connection(stream: TcpStream, ctx: Arc<ServerContext>) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // 1. Authenticate handshake line.
    let mut handshake_line = String::new();
    let n = reader.read_line(&mut handshake_line).await?;
    if n == 0 {
        return Ok(());
    }
    let handshake: Value = serde_json::from_str(handshake_line.trim())
        .context("invalid MCP bridge handshake JSON")?;
    let provided = handshake
        .get(HANDSHAKE_TOKEN_KEY)
        .and_then(Value::as_str)
        .unwrap_or("");
    if provided != ctx.token {
        bail!("MCP bridge token mismatch");
    }

    // 2. Process newline-delimited MCP JSON-RPC 2.0 messages.
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {err}"),
                    }
                });
                write_json_line(&mut write_half, &err_resp).await?;
                continue;
            }
        };

        if let Some(resp) = dispatch_mcp_jsonrpc(&msg, &ctx) {
            write_json_line(&mut write_half, &resp).await?;
        }
    }

    Ok(())
}

async fn write_json_line<W: AsyncWriteExt + Unpin>(writer: &mut W, val: &Value) -> anyhow::Result<()> {
    let mut encoded = serde_json::to_string(val)?;
    encoded.push('\n');
    writer.write_all(encoded.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

fn dispatch_mcp_jsonrpc(msg: &Value, ctx: &ServerContext) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications (no `id`) never produce a JSON-RPC response.
    let req_id = id?;

    match method {
        "initialize" => {
            let client_ver = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_MCP_PROTOCOL_VERSION);
            let negotiated = match client_ver {
                "2024-11-05" | "2025-03-26" | "2025-06-18" => client_ver,
                _ => DEFAULT_MCP_PROTOCOL_VERSION,
            };
            Some(json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "starchart",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }))
        }
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {}
        })),
        "tools/list" => {
            let tools: Vec<Value> = ctx
                .ordered_tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name(),
                        "description": t.description(),
                        "inputSchema": t.input_schema(),
                    })
                })
                .collect();
            Some(json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": tools
                }
            }))
        }
        "tools/call" => {
            let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32602,
                        "message": "tools/call missing string `name` parameter",
                    }
                }));
            };
            let raw_args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let args = if raw_args.is_null() {
                json!({})
            } else {
                raw_args
            };
            let call_result = execute_mcp_tool_call(tool_name, &args, ctx);
            Some(json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": call_result,
            }))
        }
        other => Some(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: `{other}`"),
            }
        })),
    }
}

fn execute_mcp_tool_call(tool_name: &str, args: &Value, ctx: &ServerContext) -> Value {
    let Some(tool) = ctx.tools_by_name.get(tool_name) else {
        let msg = format!("Unknown tool `{tool_name}`.");
        report_progress(ProgressKind::ToolCall, format!("mcp `{tool_name}` -> {msg}"));
        return mcp_tool_text_result(msg, true);
    };

    match tool {
        CompiledAgentTool::Read {
            name,
            shape,
            schema_ctor,
            handler,
            ..
        } => match eval_read_tool(
            name,
            shape.as_ref(),
            schema_ctor.as_ref(),
            handler,
            args,
            &ctx.runtime_scope,
        ) {
            Ok(text) => {
                report_progress(
                    ProgressKind::ToolCall,
                    format!("mcp read `{name}` -> ok ({} bytes)", text.len()),
                );
                mcp_tool_text_result(text, false)
            }
            Err(err) => {
                let msg = format!("Read tool `{name}` failed: {err:#}");
                report_progress(
                    ProgressKind::ToolCall,
                    format!("mcp read `{name}` -> error: {msg}"),
                );
                mcp_tool_text_result(msg, true)
            }
        },
        CompiledAgentTool::Artifact {
            name,
            shape,
            schema_ctor,
            mode,
            validate,
            ..
        } => {
            if *mode == ArtifactMode::Once {
                let already = ctx
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .submitted
                    .contains(name);
                if already {
                    let msg = format!(
                        "Artifact `{name}` has `mode = \"once\"` and was already submitted in this turn."
                    );
                    report_progress(
                        ProgressKind::ToolCall,
                        format!("mcp artifact `{name}` rejected: {msg}"),
                    );
                    return mcp_tool_text_result(msg, true);
                }
            }

            match eval_and_validate_artifact(
                name,
                shape,
                schema_ctor,
                validate.as_ref(),
                args,
                &ctx.runtime_scope,
            ) {
                Ok(artifact_json) => {
                    {
                        let mut guard = ctx.state.lock().unwrap_or_else(|p| p.into_inner());
                        guard.submitted.insert(name.clone());
                        match mode {
                            ArtifactMode::Replace | ArtifactMode::Once => {
                                guard.artifacts.insert(name.clone(), artifact_json.clone());
                            }
                            ArtifactMode::Append => {
                                let entry = guard
                                    .artifacts
                                    .entry(name.clone())
                                    .or_insert_with(|| Value::Array(Vec::new()));
                                if let Value::Array(items) = entry {
                                    items.push(artifact_json.clone());
                                }
                            }
                        }
                    }
                    report_progress(
                        ProgressKind::Artifact,
                        json!({
                            "tool": name,
                            "mode": mode.as_str(),
                            "artifact": artifact_json,
                        })
                        .to_string(),
                    );
                    mcp_tool_text_result(format!("Artifact `{name}` validated and recorded."), false)
                }
                Err(err) => {
                    let msg = format!("Validation failed for `{name}`: {err:#}");
                    report_progress(
                        ProgressKind::ToolCall,
                        format!("mcp artifact `{name}` rejected: {msg}"),
                    );
                    mcp_tool_text_result(msg, true)
                }
            }
        }
    }
}

fn eval_read_tool(
    tool_name: &str,
    shape: Option<&starchart_starlark::TypeShape>,
    schema_ctor: Option<&OwnedFrozenValue>,
    handler: &OwnedFrozenValue,
    args: &Value,
    scope: &ToolRuntimeScope,
) -> anyhow::Result<String> {
    starchart_starlark::with_active_type_registry(&scope.type_registry, || {
        Module::with_temp_heap(|module| {
            let ctx_val = crate::alloc_context_on_heap(
                &module,
                scope.context_record_type.as_ref(),
                Some(&scope.type_registry),
                &scope.ctx_snapshot,
            )
            .map_err(|e| anyhow::anyhow!("context error in `{tool_name}`: {e}"))?;

            let call_args = match (schema_ctor, shape) {
                (Some(ctor), Some(s)) => {
                    let args_rec = instantiate_record_with_registry(
                        &module,
                        ctor,
                        s,
                        &scope.type_registry,
                        args,
                    )?;
                    vec![ctx_val, args_rec]
                }
                _ => vec![ctx_val],
            };

            // SAFETY: `module.frozen_heap()` retains a reference to `handler.owner()` for `'v`.
            let fn_val = unsafe { handler.owned_frozen_value(module.frozen_heap()) }.to_value();
            let mut eval = Evaluator::new(&module);
            let result = eval
                .eval_function(fn_val, &call_args, &[])
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;

            if let Some(s) = result.unpack_str() {
                return Ok(s.to_owned());
            }
            let json_val = result
                .to_json_value()
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
            serde_json::to_string_pretty(&json_val).map_err(Into::into)
        })
    })
}

fn eval_and_validate_artifact(
    tool_name: &str,
    shape: &starchart_starlark::TypeShape,
    schema_ctor: &OwnedFrozenValue,
    validate: Option<&OwnedFrozenValue>,
    args: &Value,
    scope: &ToolRuntimeScope,
) -> anyhow::Result<Value> {
    starchart_starlark::with_active_type_registry(&scope.type_registry, || {
        Module::with_temp_heap(|module| {
            let artifact_rec = instantiate_record_with_registry(
                &module,
                schema_ctor,
                shape,
                &scope.type_registry,
                args,
            )?;

            if let Some(validator_fn) = validate {
                let ctx_val = crate::alloc_context_on_heap(
                    &module,
                    scope.context_record_type.as_ref(),
                    Some(&scope.type_registry),
                    &scope.ctx_snapshot,
                )
                .map_err(|e| anyhow::anyhow!("context error in validator for `{tool_name}`: {e}"))?;

                // SAFETY: `module.frozen_heap()` retains a reference to `validator_fn.owner()` for `'v`.
                let fn_val =
                    unsafe { validator_fn.owned_frozen_value(module.frozen_heap()) }.to_value();
                let mut eval = Evaluator::new(&module);
                let verdict = eval
                    .eval_function(fn_val, &[ctx_val, artifact_rec], &[])
                    .map_err(|e| anyhow::anyhow!("{e:#}"))?;

                if !verdict.is_none() {
                    if let Some(err_msg) = verdict.unpack_str() {
                        if !err_msg.trim().is_empty() {
                            bail!("{err_msg}");
                        }
                    } else {
                        bail!(
                            "validator for `{tool_name}` must return `str | None`, got `{}`",
                            verdict.get_type()
                        );
                    }
                }
            }

            artifact_rec
                .to_json_value()
                .map_err(|e| anyhow::anyhow!("{e:#}"))
        })
    })
}

fn mcp_tool_text_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ],
        "isError": is_error,
    })
}

/// Runs the `starchart mcp-bridge --addr <ADDR> --token <TOKEN>` stdio <-> TCP relay.
pub async fn run_stdio_bridge(addr: &str, token: &str) -> anyhow::Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("failed to connect MCP bridge to `{addr}`"))?;
    let (tcp_read, mut tcp_write) = stream.into_split();

    let handshake = json!({ HANDSHAKE_TOKEN_KEY: token });
    write_json_line(&mut tcp_write, &handshake).await?;

    let mut stdin_reader = BufReader::new(tokio::io::stdin());
    let mut tcp_reader = BufReader::new(tcp_read);
    let mut stdout = tokio::io::stdout();

    let stdin_to_tcp = async move {
        let mut line = String::new();
        loop {
            line.clear();
            let n = stdin_reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            tcp_write.write_all(line.as_bytes()).await?;
            tcp_write.flush().await?;
        }
        let _ = tcp_write.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };

    let tcp_to_stdout = async move {
        let mut line = String::new();
        loop {
            line.clear();
            let n = tcp_reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        res = stdin_to_tcp => res?,
        res = tcp_to_stdout => res?,
    }

    Ok(())
}

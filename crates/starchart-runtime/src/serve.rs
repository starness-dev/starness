//! HTTP/1.1 + SSE daemon (`starchart serve`) and JSON state-file persistence.
//!
//! Exposes a running [`StarchartActor`] over HTTP with zero external framework dependencies:
//! - `GET /health` — liveness & machine status
//! - `GET /state` (and `GET /`) — active states, context, history, status, output
//! - `GET /diagram` — Harel `stateDiagram-v2` Mermaid definition
//! - `GET /trace` — lossless Server-Sent Events (`text/event-stream`) or JSONL (`?format=jsonl`)
//! - `POST /events` / `POST /events/<EVENT>` — dispatch external events & return updated state

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use starchart_ir::{MachineDef, StateId, StateKind, StateNodeDef};
use starchart_starlark::CompiledChart;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{HostCallbacks, StarchartActor, render_mermaid};

/// Human-readable, resumable state snapshot persisted by `--state-file`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedChartState {
    pub machine: String,
    pub status: String,
    pub done: bool,
    pub active_states: Vec<String>,
    #[serde(default)]
    pub history: HashMap<String, String>,
    pub context: Value,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// Applies a [`PersistedChartState`] onto a cloned [`MachineDef`] so spawning a new actor
/// resumes in the saved active states, context, and shallow-history targets.
pub fn apply_persisted_state_to_ir(ir: &mut MachineDef, saved: &PersistedChartState) {
    if saved.context.is_object() {
        ir.context = saved.context.clone();
    }

    // Restore shallow/deep history `default_target` fallbacks from `saved.history`.
    for (compound_id, last_child) in &saved.history {
        set_history_default_recursive(&mut ir.states, compound_id, last_child);
    }

    // Restore `initial` pointers along the saved active state path(s).
    for path in &saved.active_states {
        let trimmed = path
            .strip_prefix(&format!("{}.", ir.id))
            .unwrap_or(path.as_str());
        let segments: Vec<&str> = trimmed.split('.').filter(|s| !s.is_empty()).collect();
        if let Some((&root_seg, rest)) = segments.split_first()
            && ir.states.contains_key(&StateId::from(root_seg))
        {
            ir.initial = StateId::new(root_seg);
            if let Some(root_node) = ir.states.get_mut(&StateId::from(root_seg)) {
                set_compound_initials_along_path(root_node, rest);
            }
        }
    }
}

fn set_compound_initials_along_path(node: &mut StateNodeDef, remaining: &[&str]) {
    let Some((&next_seg, rest)) = remaining.split_first() else {
        return;
    };
    match &mut node.kind {
        StateKind::Compound { initial, states } => {
            if states.contains_key(&StateId::from(next_seg)) {
                *initial = StateId::new(next_seg);
                if let Some(child) = states.get_mut(&StateId::from(next_seg)) {
                    set_compound_initials_along_path(child, rest);
                }
            }
        }
        StateKind::Parallel { states } => {
            if let Some(region) = states.get_mut(&StateId::from(next_seg)) {
                set_compound_initials_along_path(region, rest);
            }
        }
        StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
    }
}

fn set_history_default_recursive(
    states: &mut indexmap::IndexMap<StateId, StateNodeDef>,
    target_compound: &str,
    last_child: &str,
) {
    for (sid, node) in states.iter_mut() {
        match &mut node.kind {
            StateKind::Compound {
                states: children, ..
            } => {
                if sid.as_str() == target_compound {
                    for child_node in children.values_mut() {
                        if let StateKind::History { default_target, .. } = &mut child_node.kind {
                            *default_target = Some(StateId::new(last_child));
                        }
                    }
                }
                set_history_default_recursive(children, target_compound, last_child);
            }
            StateKind::Parallel { states: children } => {
                set_history_default_recursive(children, target_compound, last_child);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

/// Records direct `<compound> -> <child>` pairs observed in `active_states` for history persistence.
pub fn record_compound_history(active_states: &[String], history: &mut HashMap<String, String>) {
    for path in active_states {
        let segs: Vec<&str> = path.split('.').collect();
        for window in segs.windows(2) {
            history.insert(window[0].to_owned(), window[1].to_owned());
        }
    }
}

/// Configuration for [`serve_chart_http`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind_addr: String,
    pub state_file: Option<PathBuf>,
    pub exit_on_done: bool,
}

struct SharedServerState {
    diagram: String,
    actor: StarchartActor,
    state_file: Option<PathBuf>,
    exit_on_done: bool,
    history: Mutex<HashMap<String, String>>,
    persist_lock: AsyncMutex<()>,
    event_lock: AsyncMutex<()>,
    shutdown: Notify,
}

impl SharedServerState {
    async fn snapshot_state(&self) -> PersistedChartState {
        let view = self.actor.subscribe().borrow().clone();
        let active_states = self.actor.active_states();
        let history = {
            let mut h = self.history.lock().unwrap_or_else(|p| p.into_inner());
            record_compound_history(&active_states, &mut h);
            h.clone()
        };
        let context = self.actor.context().await.unwrap_or(Value::Null);
        PersistedChartState {
            machine: self.actor.machine_id.clone(),
            status: view.status.as_str().to_owned(),
            done: self.actor.is_done(),
            active_states,
            history,
            context,
            output: self.actor.output(),
            error: view.error,
        }
    }

    async fn persist_if_configured(&self) -> anyhow::Result<PersistedChartState> {
        let _guard = self.persist_lock.lock().await;
        let snap = self.snapshot_state().await;
        if let Some(path) = &self.state_file {
            write_state_file_atomic(path, &snap)?;
        }
        Ok(snap)
    }
}

fn write_state_file_atomic(path: &Path, snap: &PersistedChartState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_name = format!(
        "{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        std::process::id(),
        nanos
    );
    let tmp_path = path.with_file_name(tmp_name);
    let bytes = serde_json::to_vec_pretty(snap)?;
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Binds a TCP listener for [`serve_chart_http`], returning `(TcpListener, SocketAddr)` so callers
/// can log the bound address (including ephemeral `127.0.0.1:0` ports) before entering the loop.
pub async fn bind_serve_listener(bind_addr: &str) -> anyhow::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind HTTP server on `{bind_addr}`"))?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Starts a [`StarchartActor`] (resuming from `options.state_file` if it exists on disk) and serves
/// HTTP + SSE endpoints on `listener`.
pub async fn serve_chart_http(
    mut chart: CompiledChart,
    callbacks: HostCallbacks,
    listener: TcpListener,
    options: ServeOptions,
) -> anyhow::Result<()> {
    let mut initial_history = HashMap::new();
    if let Some(path) = &options.state_file
        && path.is_file()
    {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading state file `{}`", path.display()))?;
        let saved: PersistedChartState = serde_json::from_str(&raw)
            .with_context(|| format!("invalid JSON in state file `{}`", path.display()))?;
        if saved.machine != chart.ir.id {
            bail!(
                "state file `{}` belongs to machine `{}`, expected `{}`",
                path.display(),
                saved.machine,
                chart.ir.id
            );
        }
        initial_history = saved.history.clone();
        apply_persisted_state_to_ir(&mut chart.ir, &saved);
    }

    let diagram = render_mermaid(&chart)?;
    let actor = StarchartActor::spawn(&chart, callbacks)?;
    record_compound_history(&actor.active_states(), &mut initial_history);

    let shared = Arc::new(SharedServerState {
        diagram,
        actor,
        state_file: options.state_file,
        exit_on_done: options.exit_on_done,
        history: Mutex::new(initial_history),
        persist_lock: AsyncMutex::new(()),
        event_lock: AsyncMutex::new(()),
        shutdown: Notify::new(),
    });

    // Persist initial state immediately.
    let initial_snap = shared.persist_if_configured().await?;
    if shared.exit_on_done && initial_snap.done {
        shared.actor.stop().await;
        shared.actor.trace_hub().close();
        return Ok(());
    }

    // Background watcher: persists state on autonomous `invoke`/`after` transitions
    // and triggers `--exit-on-done` once the machine reaches a final state.
    let watcher_state = Arc::clone(&shared);
    let watcher_handle = tokio::spawn(async move {
        let mut rx = watcher_state.actor.subscribe();
        while rx.changed().await.is_ok() {
            let Ok(snap) = watcher_state.persist_if_configured().await else {
                continue;
            };
            if watcher_state.exit_on_done && (snap.done || snap.error.is_some()) {
                watcher_state.shutdown.notify_waiters();
                break;
            }
        }
    });

    loop {
        tokio::select! {
            _ = shared.shutdown.notified() => {
                break;
            }
            accept_res = listener.accept() => {
                let (stream, _peer) = match accept_res {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                let conn_state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let _ = handle_http_connection(stream, conn_state).await;
                });
            }
        }
    }

    watcher_handle.abort();
    // Acquire `event_lock` so any in-flight `POST /events` response finishes flushing before shutdown.
    let _drain_events = shared.event_lock.lock().await;
    let _ = shared.persist_if_configured().await;
    shared.actor.stop().await;
    shared.actor.trace_hub().close();
    Ok(())
}

async fn handle_http_connection(
    stream: TcpStream,
    state: Arc<SharedServerState>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        write_json_response(
            &mut write_half,
            "400 Bad Request",
            &json!({"error": "malformed request line"}),
        )
        .await?;
        return Ok(());
    }
    let method = parts[0].to_ascii_uppercase();
    let raw_target = parts[1];
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_target, ""),
    };

    let mut content_length: usize = 0;
    let mut accept_header = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "content-length" {
                content_length = val.parse::<usize>().unwrap_or(0);
            } else if key == "accept" {
                accept_header = val.to_ascii_lowercase();
            }
        }
    }

    if content_length > 16 * 1024 * 1024 {
        write_json_response(
            &mut write_half,
            "413 Payload Too Large",
            &json!({"error": "request body exceeds 16 MiB"}),
        )
        .await?;
        return Ok(());
    }

    let mut body_bytes = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_bytes).await?;
    }

    if method == "OPTIONS" {
        let resp = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        write_half.write_all(resp.as_bytes()).await?;
        write_half.flush().await?;
        return Ok(());
    }

    match (method.as_str(), path) {
        ("GET", "/health") => {
            let snap = state.snapshot_state().await;
            write_json_response(
                &mut write_half,
                "200 OK",
                &json!({
                    "ok": snap.error.is_none(),
                    "machine": snap.machine,
                    "status": snap.status,
                    "done": snap.done,
                }),
            )
            .await?;
        }
        ("GET", "/" | "/state") => {
            let snap = state.snapshot_state().await;
            let val = serde_json::to_value(&snap)?;
            write_json_response(&mut write_half, "200 OK", &val).await?;
        }
        ("GET", "/diagram") => {
            write_text_response(
                &mut write_half,
                "200 OK",
                "text/plain; charset=utf-8",
                &state.diagram,
            )
            .await?;
        }
        ("GET", "/trace") => {
            let wants_jsonl = query.split('&').any(|kv| kv == "format=jsonl")
                || accept_header.contains("application/x-ndjson");
            stream_trace_response(&mut write_half, &state, wants_jsonl).await?;
        }
        ("POST", p) if p == "/events" || p == "/event" || p.starts_with("/events/") => {
            let path_event = p.strip_prefix("/events/").filter(|s| !s.is_empty());
            let (event_type, payload) = match parse_http_event(path_event, &body_bytes) {
                Ok(pair) => pair,
                Err(err) => {
                    write_json_response(
                        &mut write_half,
                        "400 Bad Request",
                        &json!({ "error": format!("{err:#}") }),
                    )
                    .await?;
                    return Ok(());
                }
            };

            let _guard = state.event_lock.lock().await;
            let send_res = if payload.is_null() {
                state.actor.send(&event_type).await
            } else {
                state.actor.send_with_payload(&event_type, payload).await
            };
            if let Err(err) = send_res {
                write_json_response(
                    &mut write_half,
                    "500 Internal Server Error",
                    &json!({ "error": err.to_string() }),
                )
                .await?;
                return Ok(());
            }

            let snap = state.persist_if_configured().await?;
            let val = serde_json::to_value(&snap)?;
            write_json_response(&mut write_half, "200 OK", &val).await?;

            if state.exit_on_done && (snap.done || snap.error.is_some()) {
                state.shutdown.notify_waiters();
            }
        }
        _ => {
            write_json_response(
                &mut write_half,
                "404 Not Found",
                &json!({ "error": format!("route `{method} {path}` not found") }),
            )
            .await?;
        }
    }

    Ok(())
}

fn parse_http_event(path_event: Option<&str>, body: &[u8]) -> anyhow::Result<(String, Value)> {
    let body_str = std::str::from_utf8(body)
        .context("request body must be valid UTF-8")?
        .trim();

    if let Some(ev_from_path) = path_event {
        let ev_name = ev_from_path.trim();
        if ev_name.is_empty() {
            bail!("event name in URL path must not be empty");
        }
        if body_str.is_empty() {
            return Ok((ev_name.to_owned(), Value::Null));
        }
        let parsed: Value = serde_json::from_str(body_str)
            .context("invalid JSON payload in POST /events/<event>")?;
        return Ok((ev_name.to_owned(), parsed));
    }

    if body_str.is_empty() {
        bail!("POST /events requires a JSON object `{{\"type\": \"...\"}}` or plain event name");
    }

    if body_str.starts_with('{') {
        let mut val: Value =
            serde_json::from_str(body_str).context("invalid JSON event object")?;
        let obj = val
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("event JSON must be an object"))?;
        let ty = obj
            .remove("type")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| anyhow::anyhow!("event JSON object must include string field `type`"))?;
        let payload = if obj.is_empty() {
            Value::Null
        } else if obj.len() == 1 && obj.contains_key("payload") {
            obj.remove("payload").unwrap_or(Value::Null)
        } else {
            Value::Object(std::mem::take(obj))
        };
        Ok((ty, payload))
    } else {
        Ok((body_str.to_owned(), Value::Null))
    }
}

async fn stream_trace_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    state: &SharedServerState,
    as_jsonl: bool,
) -> anyhow::Result<()> {
    let content_type = if as_jsonl {
        "application/x-ndjson; charset=utf-8"
    } else {
        "text/event-stream; charset=utf-8"
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
    );
    writer.write_all(header.as_bytes()).await?;
    writer.flush().await?;

    let mut rx = state.actor.subscribe_traces();
    loop {
        tokio::select! {
            _ = state.shutdown.notified() => {
                // Drain any buffered final records before closing stream.
                for record in rx.drain() {
                    write_trace_frame(writer, &record, as_jsonl).await?;
                }
                break;
            }
            maybe_rec = rx.recv() => {
                match maybe_rec {
                    Some(record) => write_trace_frame(writer, &record, as_jsonl).await?,
                    None => break,
                }
            }
        }
    }
    Ok(())
}

async fn write_trace_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    record: &crate::TraceRecord,
    as_jsonl: bool,
) -> anyhow::Result<()> {
    let json_line = record.to_json_line();
    let frame = if as_jsonl {
        format!("{json_line}\n")
    } else {
        format!("id: {}\ndata: {json_line}\n\n", record.seq)
    };
    writer.write_all(frame.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_json_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: &str,
    body: &Value,
) -> anyhow::Result<()> {
    let serialized = serde_json::to_string(body)?;
    write_text_response(writer, status, "application/json; charset=utf-8", &serialized).await
}

async fn write_text_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: &str,
    content_type: &str,
    body: &str,
) -> anyhow::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await?;
    let _ = writer.shutdown().await;
    Ok(())
}

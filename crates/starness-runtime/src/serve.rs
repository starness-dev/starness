//! Multi-machine, multi-instance HTTP/1.1 + SSE daemon (`starness serve`) backed by SQLite.
//!
//! Routes:
//! - `GET /health` (and `GET /`) — daemon liveness, loaded machines, live actor count, DB target
//! - `GET /trace` — server-wide SSE / JSONL trace firehose across all instances
//! - `GET /machines` — list registered `.star` machine definitions
//! - `GET /machines/:machine` — inspect a machine definition & instance counts
//! - `GET /machines/:machine/diagram` — Harel `stateDiagram-v2` Mermaid definition
//! - `POST /machines/:machine/instances` — create/start an instance (`{"id"?, "context"?, "idempotent"?}`)
//! - `GET /machines/:machine/instances` — list instances (`?status=active|waiting|completed|cancelled|failed`)
//! - `GET /machines/:machine/instances/:id` — inspect an instance snapshot
//! - `POST /machines/:machine/instances/:id/events` (and `/events/:event`) — route an event to an instance
//! - `POST /machines/:machine/instances/:id/cancel` — cancel a running/waiting instance
//! - `DELETE /machines/:machine/instances/:id` — cancel instance (or delete from SQLite with `?purge=true`)
//! - `GET /machines/:machine/instances/:id/trace` — per-instance SSE (`text/event-stream`) or JSONL (`?format=jsonl`)

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use indexmap::IndexMap;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use starness_ir::{MachineDef, StateId, StateKind, StateNodeDef, UiViewSheet};
use starness_starlark::CompiledChart;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use xstate_statemachine::core::Status;

use crate::mcp_server::{
    DEFAULT_MCP_SETTLE_TIMEOUT, McpInstanceView, McpMachineBackend, McpMachineEntry,
    StarnessMcpHandler, build_stateless_http_mcp_service, run_ephemeral_event,
    wait_actor_until_settled,
};
use crate::store::{InstanceSnapshot, InstanceStatus, SqliteStore};
use crate::{
    HostCallbacks, StarnessActor, active_states_have_autonomous_work, machine_is_completable,
    render_mermaid,
};

/// Human-readable, resumable state snapshot shape used when rehydrating an [`MachineDef`].
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
    apply_snapshot_fields_to_ir(ir, &saved.context, &saved.history, &saved.active_states);
}

/// Applies an [`InstanceSnapshot`] onto a cloned [`MachineDef`] for SQLite rehydration.
pub fn apply_instance_snapshot_to_ir(ir: &mut MachineDef, snap: &InstanceSnapshot) {
    apply_snapshot_fields_to_ir(ir, &snap.context, &snap.history, &snap.active_states);
}

fn apply_snapshot_fields_to_ir(
    ir: &mut MachineDef,
    context: &Value,
    history: &HashMap<String, String>,
    active_states: &[String],
) {
    if context.is_object() {
        ir.context = context.clone();
    }

    // Restore shallow/deep history `default_target` fallbacks from `history`.
    for (compound_id, last_child) in history {
        set_history_default_recursive(&mut ir.states, compound_id, last_child);
    }

    // Restore `initial` pointers along the saved active state path(s).
    for path in active_states {
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

/// One compiled statechart definition plus host callbacks and optional external view-sheet
/// (`*.ui.json`) to register in [`serve_charts_http`].
#[derive(Clone)]
pub struct ServeDefinition {
    pub chart: CompiledChart,
    pub callbacks: HostCallbacks,
    pub ui_sheet: Option<UiViewSheet>,
}

impl std::fmt::Debug for ServeDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeDefinition")
            .field("machine_id", &self.chart.ir.id)
            .finish_non_exhaustive()
    }
}

/// Configuration for [`serve_charts_http`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind_addr: String,
    /// SQLite database target (`":memory:"` by default, or file path on disk).
    pub db: String,
    /// Optional shutdown notifier enabling callers/tests to stop the HTTP loop cleanly.
    pub shutdown: Option<Arc<Notify>>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8080".to_owned(),
            db: ":memory:".to_owned(),
            shutdown: None,
        }
    }
}

struct RegisteredMachine {
    chart: CompiledChart,
    callbacks: HostCallbacks,
    ui_sheet: Option<UiViewSheet>,
    diagram: String,
    completable: bool,
}

struct LiveInstance {
    machine_id: String,
    instance_id: String,
    ir: MachineDef,
    actor: StarnessActor,
    created_at_ms: u64,
    history: Mutex<HashMap<String, String>>,
    persist_lock: AsyncMutex<()>,
    event_lock: AsyncMutex<()>,
    trace_closed: AtomicBool,
    trace_notify: Arc<Notify>,
    finalized: AtomicBool,
}

type HttpMcpService = StreamableHttpService<StarnessMcpHandler, LocalSessionManager>;

struct McpEndpoints {
    root: Arc<HttpMcpService>,
    per_machine: HashMap<String, Arc<HttpMcpService>>,
    cancel: CancellationToken,
}

struct ServerState {
    machines: IndexMap<String, Arc<RegisteredMachine>>,
    store: Arc<SqliteStore>,
    live_instances: Mutex<HashMap<(String, String), Arc<LiveInstance>>>,
    create_lock: AsyncMutex<()>,
    next_id_counter: AtomicU64,
    global_trace_notify: Notify,
    shutdown: Arc<Notify>,
    mcp: Mutex<Option<Arc<McpEndpoints>>>,
}

impl LiveInstance {
    async fn capture_snapshot(&self, forced_status: Option<InstanceStatus>) -> InstanceSnapshot {
        let view = self.actor.subscribe().borrow().clone();
        let active_states = self.actor.active_states();
        let history = {
            let mut h = self
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            record_compound_history(&active_states, &mut h);
            h.clone()
        };
        let context = self.actor.context().await.unwrap_or(Value::Null);
        let computed_status = if let Some(forced) = forced_status {
            forced
        } else if view.error.is_some() {
            InstanceStatus::Failed
        } else if self.actor.is_done() || view.status == Status::Done {
            InstanceStatus::Completed
        } else if view.status == Status::Stopped {
            InstanceStatus::Cancelled
        } else if active_states_have_autonomous_work(&self.ir, &active_states) {
            InstanceStatus::Active
        } else {
            InstanceStatus::Waiting
        };

        InstanceSnapshot {
            machine: self.machine_id.clone(),
            id: self.instance_id.clone(),
            status: computed_status,
            done: computed_status == InstanceStatus::Completed,
            active_states,
            history,
            context,
            output: self.actor.output(),
            error: view.error,
            created_at_ms: self.created_at_ms,
            updated_at_ms: now_epoch_ms(),
        }
    }
}

impl ServerState {
    fn get_live(&self, machine_id: &str, instance_id: &str) -> Option<Arc<LiveInstance>> {
        let guard = self
            .live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get(&(machine_id.to_owned(), instance_id.to_owned()))
            .cloned()
    }

    fn remove_live(&self, machine_id: &str, instance_id: &str) -> Option<Arc<LiveInstance>> {
        let mut guard = self
            .live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&(machine_id.to_owned(), instance_id.to_owned()))
    }

    fn live_count(&self) -> usize {
        self.live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|inst| !inst.finalized.load(Ordering::Relaxed))
            .count()
    }

    fn generate_instance_id(&self) -> String {
        let seq = self.next_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let ms = now_epoch_ms();
        format!("inst-{ms:x}-{seq}")
    }
}

fn now_epoch_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0)
}

/// Binds a TCP listener for [`serve_charts_http`], returning `(TcpListener, SocketAddr)` so callers
/// can log the bound address (including ephemeral `127.0.0.1:0` ports) before entering the loop.
pub async fn bind_serve_listener(bind_addr: &str) -> anyhow::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind HTTP server on `{bind_addr}`"))?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Hosts one or more compiled `.star` statechart definitions on `listener`, persisting instance
/// snapshots and execution traces to SQLite (`:memory:` by default, or `options.db` on disk).
/// Any non-terminal (`active` or `waiting`) instances present in `options.db` are automatically
/// rehydrated on startup.
pub async fn serve_charts_http(
    definitions: Vec<ServeDefinition>,
    listener: TcpListener,
    options: ServeOptions,
) -> anyhow::Result<()> {
    if definitions.is_empty() {
        bail!("serve_charts_http requires at least one statechart definition");
    }

    let mut machines = IndexMap::with_capacity(definitions.len());
    for def in definitions {
        let id = def.chart.ir.id.clone();
        if machines.contains_key(&id) {
            bail!("duplicate machine id `{id}` registered in starness serve");
        }
        if let Some(sheet) = &def.ui_sheet
            && let Err(errs) = sheet.validate_against(&def.chart.ir)
        {
            let joined = errs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ");
            bail!("invalid UI view-sheet for machine `{id}`:\n  {joined}");
        }
        let diagram = render_mermaid(&def.chart)
            .with_context(|| format!("failed rendering Mermaid diagram for `{id}`"))?;
        let completable = machine_is_completable(&def.chart.ir);
        machines.insert(
            id,
            Arc::new(RegisteredMachine {
                chart: def.chart,
                callbacks: def.callbacks,
                ui_sheet: def.ui_sheet,
                diagram,
                completable,
            }),
        );
    }

    let store = Arc::new(SqliteStore::open(&options.db)?);
    let shutdown = options
        .shutdown
        .unwrap_or_else(|| Arc::new(Notify::new()));

    let state = Arc::new(ServerState {
        machines,
        store,
        live_instances: Mutex::new(HashMap::new()),
        create_lock: AsyncMutex::new(()),
        next_id_counter: AtomicU64::new(0),
        global_trace_notify: Notify::new(),
        shutdown: Arc::clone(&shutdown),
        mcp: Mutex::new(None),
    });

    // Build stateless (2026-07-28) Streamable HTTP MCP endpoints for `/mcp` and `/machines/:m/mcp`.
    {
        let mcp_cancel = CancellationToken::new();
        let all_entries: Vec<McpMachineEntry> = state
            .machines
            .iter()
            .map(|(id, reg)| McpMachineEntry {
                machine_id: id.clone(),
                chart: reg.chart.clone(),
                diagram: reg.diagram.clone(),
            })
            .collect();
        let root_backend: Arc<dyn McpMachineBackend> = Arc::new(ServeMcpBackend {
            server: Arc::downgrade(&state),
            machines: all_entries.clone(),
        });
        let root_svc = Arc::new(build_stateless_http_mcp_service(
            root_backend,
            mcp_cancel.child_token(),
        ));
        let mut per_machine = HashMap::with_capacity(all_entries.len());
        for entry in all_entries {
            let mid = entry.machine_id.clone();
            let backend: Arc<dyn McpMachineBackend> = Arc::new(ServeMcpBackend {
                server: Arc::downgrade(&state),
                machines: vec![entry],
            });
            per_machine.insert(
                mid,
                Arc::new(build_stateless_http_mcp_service(
                    backend,
                    mcp_cancel.child_token(),
                )),
            );
        }
        *state
            .mcp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(McpEndpoints {
            root: root_svc,
            per_machine,
            cancel: mcp_cancel,
        }));
    }

    // Rehydrate any `active` or `waiting` instances stored in SQLite across restarts.
    let resumable = state.store.list_resumable_instances()?;
    for saved in resumable {
        if let Some(reg) = state.machines.get(&saved.machine).cloned() {
            let _ = spawn_or_rehydrate_instance(&state, &reg, saved.id.clone(), None, Some(&saved))
                .await?;
        }
    }

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                if let Some(mcp) = state
                    .mcp
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    mcp.cancel.cancel();
                }
                break;
            }
            accept_res = listener.accept() => {
                let (stream, _peer) = match accept_res {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                let conn_state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = handle_http_connection(stream, conn_state).await;
                });
            }
        }
    }

    // Gracefully flush and stop all currently live actors on daemon shutdown (preserving their
    // `active`/`waiting` status in SQLite so they resume cleanly on the next startup).
    let remaining: Vec<Arc<LiveInstance>> = {
        let mut map = state
            .live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.drain().map(|(_, v)| v).collect()
    };
    for live in remaining {
        let _ev_guard = live.event_lock.lock().await;
        let _p_guard = live.persist_lock.lock().await;
        let snap = live.capture_snapshot(None).await;
        let _ = state.store.upsert_instance(&snap);
        live.actor.stop().await;
        live.actor.trace_hub().close();
    }

    Ok(())
}

async fn spawn_or_rehydrate_instance(
    server: &Arc<ServerState>,
    reg: &Arc<RegisteredMachine>,
    instance_id: String,
    context_patch: Option<&serde_json::Map<String, Value>>,
    rehydrated: Option<&InstanceSnapshot>,
) -> anyhow::Result<InstanceSnapshot> {
    let mut chart = reg.chart.clone();
    let mut initial_history = HashMap::new();
    let created_at_ms = match rehydrated {
        Some(saved) => {
            initial_history = saved.history.clone();
            apply_instance_snapshot_to_ir(&mut chart.ir, saved);
            saved.created_at_ms
        }
        None => {
            if let Some(patch) = context_patch
                && let Value::Object(ctx_map) = &mut chart.ir.context
            {
                for (k, v) in patch {
                    ctx_map.insert(k.clone(), v.clone());
                }
            }
            now_epoch_ms()
        }
    };

    let machine_id = chart.ir.id.clone();
    let base_seq = server.store.max_trace_seq(&machine_id, &instance_id)?;
    let instance_ir = chart.ir.clone();
    let actor = StarnessActor::spawn_instance(
        &chart,
        reg.callbacks.clone(),
        instance_id.clone(),
        base_seq,
    )?;
    let mut trace_rx = actor.subscribe_traces();

    let live = Arc::new(LiveInstance {
        machine_id: machine_id.clone(),
        instance_id: instance_id.clone(),
        ir: instance_ir,
        actor,
        created_at_ms,
        history: Mutex::new(initial_history),
        persist_lock: AsyncMutex::new(()),
        event_lock: AsyncMutex::new(()),
        trace_closed: AtomicBool::new(false),
        trace_notify: Arc::new(Notify::new()),
        finalized: AtomicBool::new(false),
    });

    // Persist the `instances` row BEFORE inserting `trace_events` (foreign key constraint).
    let initial_snap = {
        let _p_guard = live.persist_lock.lock().await;
        let snap = live.capture_snapshot(None).await;
        server.store.upsert_instance(&snap)?;
        snap
    };

    let initial_traces = trace_rx.drain();
    if !initial_traces.is_empty() {
        let _ = server
            .store
            .insert_trace_records(&machine_id, &instance_id, &initial_traces);
        live.trace_notify.notify_waiters();
        server.global_trace_notify.notify_waiters();
    }

    // If the statechart entered a `final_state` immediately during startup, finalize and drop actor now.
    if initial_snap.status.is_terminal() {
        live.finalized.store(true, Ordering::SeqCst);
        live.actor.stop().await;
        live.actor.trace_hub().close();
        let tail = trace_rx.drain();
        if !tail.is_empty() {
            let _ = server
                .store
                .insert_trace_records(&machine_id, &instance_id, &tail);
        }
        live.trace_closed.store(true, Ordering::SeqCst);
        live.trace_notify.notify_waiters();
        server.global_trace_notify.notify_waiters();
        return Ok(initial_snap);
    }

    {
        let mut map = server
            .live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert((machine_id.clone(), instance_id.clone()), Arc::clone(&live));
    }

    // Background task 1: stream `TraceRecord`s into SQLite `trace_events` and wake SSE followers.
    let trace_server = Arc::clone(server);
    let trace_live = Arc::clone(&live);
    tokio::spawn(async move {
        while let Some(first) = trace_rx.recv().await {
            let mut batch = vec![first];
            batch.extend(trace_rx.drain());
            let _ = trace_server.store.insert_trace_records(
                &trace_live.machine_id,
                &trace_live.instance_id,
                &batch,
            );
            trace_live.trace_notify.notify_waiters();
            trace_server.global_trace_notify.notify_waiters();
        }
        trace_live.trace_closed.store(true, Ordering::SeqCst);
        trace_server.remove_live(&trace_live.machine_id, &trace_live.instance_id);
        trace_live.trace_notify.notify_waiters();
        trace_server.global_trace_notify.notify_waiters();
    });

    // Background task 2: persist autonomous state transitions (`invoke` / `after`) and drop
    // completed/failed actors from RAM as soon as they reach a terminal state.
    let watch_server = Arc::clone(server);
    let watch_live = Arc::clone(&live);
    tokio::spawn(async move {
        let mut rx = watch_live.actor.subscribe();
        while rx.changed().await.is_ok() {
            let Ok(snap) = sync_and_maybe_finalize(&watch_server, &watch_live, None).await else {
                continue;
            };
            if snap.status.is_terminal() {
                break;
            }
        }
    });

    Ok(initial_snap)
}

async fn sync_and_maybe_finalize(
    server: &Arc<ServerState>,
    live: &Arc<LiveInstance>,
    forced_status: Option<InstanceStatus>,
) -> anyhow::Result<InstanceSnapshot> {
    let snap = {
        let _p_guard = live.persist_lock.lock().await;
        let snap = live.capture_snapshot(forced_status).await;
        server.store.upsert_instance(&snap)?;
        snap
    };

    if snap.status.is_terminal() && !live.finalized.swap(true, Ordering::SeqCst) {
        live.actor.stop().await;
        live.actor.trace_hub().close();
    }

    Ok(snap)
}

#[derive(Debug, Default, Deserialize)]
struct CreateInstanceBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default)]
    idempotent: Option<bool>,
}

async fn handle_http_connection(
    stream: TcpStream,
    state: Arc<ServerState>,
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
    let (raw_path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_target, ""),
    };

    let mut content_length: usize = 0;
    let mut accept_header = String::new();
    let mut last_event_id: Option<u64> = None;
    let mut raw_headers: Vec<(String, String)> = Vec::new();
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
            raw_headers.push((key.clone(), val.to_owned()));
            match key.as_str() {
                "content-length" => content_length = val.parse::<usize>().unwrap_or(0),
                "accept" => accept_header = val.to_ascii_lowercase(),
                "last-event-id" => last_event_id = val.parse::<u64>().ok(),
                _ => {}
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
        let resp = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Accept, Last-Event-ID, Mcp-Session-Id, Mcp-Protocol-Version, Mcp-Method, Mcp-Name\r\nAccess-Control-Expose-Headers: Mcp-Session-Id, Mcp-Protocol-Version\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        write_half.write_all(resp.as_bytes()).await?;
        write_half.flush().await?;
        return Ok(());
    }

    let query_params = parse_query_params(query);
    let segments: Vec<String> = raw_path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let seg_refs: Vec<&str> = segments.iter().map(String::as_str).collect();

    match (method.as_str(), seg_refs.as_slice()) {
        // Stateless (2026-07-28) Streamable HTTP MCP endpoints
        (_, ["mcp"]) => {
            let svc = state
                .mcp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|m| Arc::clone(&m.root));
            if let Some(svc) = svc {
                handle_mcp_http_request(
                    &mut write_half,
                    &svc,
                    &method,
                    raw_target,
                    &raw_headers,
                    body_bytes,
                )
                .await?;
            } else {
                write_not_found(&mut write_half, "MCP service is not initialized".to_owned())
                    .await?;
            }
        }
        (_, ["machines", machine_id, "mcp"]) => {
            let svc = state
                .mcp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|m| m.per_machine.get(*machine_id).cloned());
            if let Some(svc) = svc {
                handle_mcp_http_request(
                    &mut write_half,
                    &svc,
                    &method,
                    raw_target,
                    &raw_headers,
                    body_bytes,
                )
                .await?;
            } else {
                write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await?;
            }
        }

        // GET /ui or GET /ui/* — embedded / static Web Studio
        ("GET", ["ui"]) => {
            serve_web_ui(&mut write_half, "").await?;
        }
        ("GET", ["ui", rest @ ..]) => {
            let sub = rest.join("/");
            serve_web_ui(&mut write_half, &sub).await?;
        }

        // GET / (HTML browsers get Web Studio; API clients get JSON health)
        ("GET", []) if accept_header.contains("text/html") => {
            serve_web_ui(&mut write_half, "").await?;
        }

        // GET / or GET /health
        ("GET", [] | ["health"]) => {
            let machine_ids: Vec<&String> = state.machines.keys().collect();
            write_json_response(
                &mut write_half,
                "200 OK",
                &json!({
                    "ok": true,
                    "db": state.store.target(),
                    "machines": machine_ids,
                    "live_instances": state.live_count(),
                }),
            )
            .await?;
        }

        // GET /trace — server-wide firehose across all machines and instances
        ("GET", ["trace"]) => {
            let wants_jsonl = query_params.get("format").is_some_and(|v| v == "jsonl")
                || accept_header.contains("application/x-ndjson");
            let follow = query_params
                .get("follow")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true);
            stream_global_trace(&mut write_half, &state, wants_jsonl, follow).await?;
        }

        // GET /machines — list registered machine definitions
        ("GET", ["machines"]) => {
            let mut list = Vec::with_capacity(state.machines.len());
            for (id, reg) in &state.machines {
                let counts = state
                    .store
                    .count_instances_by_status(id)
                    .unwrap_or_default();
                list.push(machine_summary_json(reg, &counts));
            }
            write_json_response(&mut write_half, "200 OK", &json!({ "machines": list })).await?;
        }

        // GET /machines/:machine
        ("GET", ["machines", machine_id]) => {
            let Some(reg) = state.machines.get(*machine_id) else {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };
            let counts = state
                .store
                .count_instances_by_status(machine_id)
                .unwrap_or_default();
            write_json_response(&mut write_half, "200 OK", &machine_summary_json(reg, &counts))
                .await?;
        }

        // GET /machines/:machine/diagram
        ("GET", ["machines", machine_id, "diagram"]) => {
            let Some(reg) = state.machines.get(*machine_id) else {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };
            write_text_response(
                &mut write_half,
                "200 OK",
                "text/plain; charset=utf-8",
                &reg.diagram,
            )
            .await?;
        }

        // POST /machines/:machine/instances — spawn a new instance
        ("POST", ["machines", machine_id, "instances"]) => {
            let Some(reg) = state.machines.get(*machine_id).cloned() else {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };

            let req_body: CreateInstanceBody = if body_bytes.iter().all(u8::is_ascii_whitespace) {
                CreateInstanceBody::default()
            } else {
                match serde_json::from_slice(&body_bytes) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        return write_json_response(
                            &mut write_half,
                            "400 Bad Request",
                            &json!({"error": format!("invalid JSON body: {err}")}),
                        )
                        .await;
                    }
                }
            };

            let context_patch = match &req_body.context {
                None | Some(Value::Null) => None,
                Some(Value::Object(map)) => Some(map),
                Some(_) => {
                    return write_json_response(
                        &mut write_half,
                        "400 Bad Request",
                        &json!({"error": "`context` override must be a JSON object"}),
                    )
                    .await;
                }
            };

            let instance_id = match req_body.id {
                Some(raw_id) => {
                    let trimmed = raw_id.trim().to_owned();
                    if trimmed.is_empty()
                        || trimmed.contains('/')
                        || trimmed.chars().any(char::is_whitespace)
                    {
                        return write_json_response(
                            &mut write_half,
                            "400 Bad Request",
                            &json!({"error": "instance `id` must be non-empty and must not contain `/` or whitespace"}),
                        )
                        .await;
                    }
                    trimmed
                }
                None => state.generate_instance_id(),
            };

            let idempotent = req_body.idempotent.unwrap_or(false)
                || query_params
                    .get("idempotent")
                    .is_some_and(|v| v == "true" || v == "1");

            let _create_guard = state.create_lock.lock().await;
            if let Some(existing) = state.store.get_instance(machine_id, &instance_id)? {
                let status_line = if idempotent {
                    "200 OK"
                } else {
                    "409 Conflict"
                };
                let body = if idempotent {
                    enriched_instance_json(&reg, &existing)?
                } else {
                    json!({
                        "error": format!(
                            "instance `{instance_id}` already exists for machine `{machine_id}`"
                        ),
                        "instance": enriched_instance_json(&reg, &existing)?,
                    })
                };
                return write_json_response(&mut write_half, status_line, &body).await;
            }

            let created = match spawn_or_rehydrate_instance(
                &state,
                &reg,
                instance_id,
                context_patch,
                None,
            )
            .await
            {
                Ok(snap) => snap,
                Err(err) => {
                    return write_json_response(
                        &mut write_half,
                        "500 Internal Server Error",
                        &json!({"error": format!("{err:#}")}),
                    )
                    .await;
                }
            };

            write_json_response(
                &mut write_half,
                "201 Created",
                &enriched_instance_json(&reg, &created)?,
            )
            .await?;
        }

        // GET /machines/:machine/instances — list instances
        ("GET", ["machines", machine_id, "instances"]) => {
            if !state.machines.contains_key(*machine_id) {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };

            let status_filter = match query_params.get("status") {
                Some(raw_status) => match InstanceStatus::parse_str(raw_status) {
                    Some(st) => Some(st),
                    None => {
                        return write_json_response(
                            &mut write_half,
                            "400 Bad Request",
                            &json!({"error": format!("unknown instance status filter `{raw_status}`")}),
                        )
                        .await;
                    }
                },
                None => None,
            };
            let limit = query_params
                .get("limit")
                .and_then(|s| s.parse::<usize>().ok());

            let instances = state
                .store
                .list_instances(machine_id, status_filter, limit)?;
            write_json_response(
                &mut write_half,
                "200 OK",
                &json!({
                    "machine": machine_id,
                    "instances": instances,
                }),
            )
            .await?;
        }

        // GET /machines/:machine/instances/:id — inspect instance snapshot
        ("GET", ["machines", machine_id, "instances", instance_id]) => {
            let Some(reg) = state.machines.get(*machine_id).cloned() else {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };
            if let Some(live) = state.get_live(machine_id, instance_id)
                && !live.finalized.load(Ordering::SeqCst)
            {
                let snap = sync_and_maybe_finalize(&state, &live, None).await?;
                return write_json_response(
                    &mut write_half,
                    "200 OK",
                    &enriched_instance_json(&reg, &snap)?,
                )
                .await;
            }
            match state.store.get_instance(machine_id, instance_id)? {
                Some(snap) => {
                    write_json_response(
                        &mut write_half,
                        "200 OK",
                        &enriched_instance_json(&reg, &snap)?,
                    )
                    .await?;
                }
                None => {
                    write_not_found(
                        &mut write_half,
                        format!("instance `{instance_id}` not found for machine `{machine_id}`"),
                    )
                    .await?;
                }
            }
        }

        // POST /machines/:machine/instances/:id/events[/:event]
        (
            "POST",
            ["machines", machine_id, "instances", instance_id, "events" | "event"]
            | ["machines", machine_id, "instances", instance_id, "events", _],
        ) => {
            let Some(reg) = state.machines.get(*machine_id).cloned() else {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            };

            let path_event = if seg_refs.len() == 6 {
                Some(seg_refs[5])
            } else {
                None
            };
            let (event_type, payload) = match parse_http_event(path_event, &body_bytes) {
                Ok(pair) => pair,
                Err(err) => {
                    return write_json_response(
                        &mut write_half,
                        "400 Bad Request",
                        &json!({ "error": format!("{err:#}") }),
                    )
                    .await;
                }
            };

            let live = match ensure_live_instance(&state, &reg, machine_id, instance_id).await? {
                EnsureLiveResult::Live(l) => l,
                EnsureLiveResult::Terminal(snap) => {
                    return write_json_response(
                        &mut write_half,
                        "409 Conflict",
                        &json!({
                            "error": format!(
                                "instance `{instance_id}` is `{}` and no longer accepts events",
                                snap.status.as_str()
                            ),
                            "instance": snap,
                        }),
                    )
                    .await;
                }
                EnsureLiveResult::NotFound => {
                    return write_not_found(
                        &mut write_half,
                        format!("instance `{instance_id}` not found for machine `{machine_id}`"),
                    )
                    .await;
                }
            };

            let _ev_guard = live.event_lock.lock().await;
            let send_res = if payload.is_null() {
                live.actor.send(&event_type).await
            } else {
                live.actor.send_with_payload(&event_type, payload).await
            };
            if let Err(err) = send_res {
                return write_json_response(
                    &mut write_half,
                    "500 Internal Server Error",
                    &json!({ "error": err.to_string() }),
                )
                .await;
            }

            let snap = sync_and_maybe_finalize(&state, &live, None).await?;
            write_json_response(&mut write_half, "200 OK", &enriched_instance_json(&reg, &snap)?)
                .await?;
        }

        // DELETE /machines/:machine/instances/:id (or POST .../cancel)
        ("DELETE", ["machines", machine_id, "instances", instance_id])
        | ("POST", ["machines", machine_id, "instances", instance_id, "cancel"]) => {
            if !state.machines.contains_key(*machine_id) {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            }
            let purge = method == "DELETE"
                && query_params
                    .get("purge")
                    .is_some_and(|v| v == "true" || v == "1");

            if let Some(live) = state.remove_live(machine_id, instance_id) {
                let _ev_guard = live.event_lock.lock().await;
                live.finalized.store(true, Ordering::SeqCst);
                let cancelled_snap = {
                    let _p_guard = live.persist_lock.lock().await;
                    let mut snap = live
                        .capture_snapshot(Some(InstanceStatus::Cancelled))
                        .await;
                    snap.status = InstanceStatus::Cancelled;
                    snap.done = false;
                    state.store.upsert_instance(&snap)?;
                    snap
                };
                live.actor.stop().await;
                live.actor.trace_hub().close();

                if purge {
                    state.store.delete_instance(machine_id, instance_id)?;
                    return write_json_response(
                        &mut write_half,
                        "200 OK",
                        &json!({
                            "deleted": true,
                            "machine": machine_id,
                            "id": instance_id,
                        }),
                    )
                    .await;
                }
                return write_json_response(
                    &mut write_half,
                    "200 OK",
                    &serde_json::to_value(&cancelled_snap)?,
                )
                .await;
            }

            let Some(mut existing) = state.store.get_instance(machine_id, instance_id)? else {
                return write_not_found(
                    &mut write_half,
                    format!("instance `{instance_id}` not found for machine `{machine_id}`"),
                )
                .await;
            };

            if purge {
                state.store.delete_instance(machine_id, instance_id)?;
                return write_json_response(
                    &mut write_half,
                    "200 OK",
                    &json!({
                        "deleted": true,
                        "machine": machine_id,
                        "id": instance_id,
                    }),
                )
                .await;
            }

            if !existing.status.is_terminal() {
                existing.status = InstanceStatus::Cancelled;
                existing.done = false;
                existing.updated_at_ms = now_epoch_ms();
                state.store.upsert_instance(&existing)?;
            }
            write_json_response(
                &mut write_half,
                "200 OK",
                &serde_json::to_value(&existing)?,
            )
            .await?;
        }

        // GET /machines/:machine/instances/:id/trace — per-instance SSE or JSONL stream
        ("GET", ["machines", machine_id, "instances", instance_id, "trace"]) => {
            if !state.machines.contains_key(*machine_id) {
                return write_not_found(
                    &mut write_half,
                    format!("machine `{machine_id}` is not registered"),
                )
                .await;
            }
            if state
                .store
                .get_instance(machine_id, instance_id)?
                .is_none()
            {
                return write_not_found(
                    &mut write_half,
                    format!("instance `{instance_id}` not found for machine `{machine_id}`"),
                )
                .await;
            }

            let wants_jsonl = query_params.get("format").is_some_and(|v| v == "jsonl")
                || accept_header.contains("application/x-ndjson");
            let after_seq = query_params
                .get("after")
                .and_then(|s| s.parse::<u64>().ok())
                .or(last_event_id)
                .unwrap_or(0);
            let follow = query_params
                .get("follow")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true);

            stream_instance_trace(
                &mut write_half,
                &state,
                machine_id,
                instance_id,
                after_seq,
                wants_jsonl,
                follow,
            )
            .await?;
        }

        _ => {
            write_not_found(
                &mut write_half,
                format!("route `{method} {raw_path}` not found"),
            )
            .await?;
        }
    }

    Ok(())
}

enum EnsureLiveResult {
    Live(Arc<LiveInstance>),
    Terminal(Box<InstanceSnapshot>),
    NotFound,
}

async fn ensure_live_instance(
    state: &Arc<ServerState>,
    reg: &Arc<RegisteredMachine>,
    machine_id: &str,
    instance_id: &str,
) -> anyhow::Result<EnsureLiveResult> {
    if let Some(live) = state.get_live(machine_id, instance_id)
        && !live.finalized.load(Ordering::SeqCst)
    {
        return Ok(EnsureLiveResult::Live(live));
    }

    let _create_guard = state.create_lock.lock().await;
    if let Some(live) = state.get_live(machine_id, instance_id)
        && !live.finalized.load(Ordering::SeqCst)
    {
        return Ok(EnsureLiveResult::Live(live));
    }

    let Some(saved) = state.store.get_instance(machine_id, instance_id)? else {
        return Ok(EnsureLiveResult::NotFound);
    };
    if saved.status.is_terminal() {
        return Ok(EnsureLiveResult::Terminal(Box::new(saved)));
    }

    // Rehydrate on demand if `saved` is `active` or `waiting` in SQLite without a live actor.
    let snap = spawn_or_rehydrate_instance(
        state,
        reg,
        instance_id.to_owned(),
        None,
        Some(&saved),
    )
    .await?;
    if snap.status.is_terminal() {
        return Ok(EnsureLiveResult::Terminal(Box::new(snap)));
    }
    match state.get_live(machine_id, instance_id) {
        Some(live) => Ok(EnsureLiveResult::Live(live)),
        None => Ok(EnsureLiveResult::Terminal(Box::new(snap))),
    }
}

/// Embedded code-split Web Studio assets built from `ui/` (`npm run build`).
#[derive(rust_embed::RustEmbed)]
#[folder = "ui_dist/"]
struct EmbeddedUiAssets;

/// Serves a static UI asset from `STARNESS_UI_DIR` (if set) or falls back to the embedded Web Studio.
async fn serve_web_ui<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    sub_path: &str,
) -> anyhow::Result<()> {
    let rel = sub_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    if let Ok(ui_dir) = std::env::var("STARNESS_UI_DIR")
        && !ui_dir.trim().is_empty()
        && !rel.contains("..")
    {
        let candidate = std::path::Path::new(ui_dir.trim()).join(rel);
        let target = if candidate.is_file() {
            candidate
        } else {
            std::path::Path::new(ui_dir.trim()).join("index.html")
        };
        if let Ok(bytes) = tokio::fs::read(&target).await {
            let ct = guess_content_type(&target);
            return write_bytes_response(writer, "200 OK", ct, &bytes).await;
        }
    }

    if !rel.contains("..")
        && let Some(file) = EmbeddedUiAssets::get(rel)
    {
        let ct = guess_content_type(std::path::Path::new(rel));
        return write_bytes_response(writer, "200 OK", ct, file.data.as_ref()).await;
    }

    if let Some(index) = EmbeddedUiAssets::get("index.html") {
        return write_bytes_response(
            writer,
            "200 OK",
            "text/html; charset=utf-8",
            index.data.as_ref(),
        )
        .await;
    }

    write_text_response(
        writer,
        "404 Not Found",
        "text/plain; charset=utf-8",
        "Embedded Web Studio assets not found (run `cd ui && npm run build`).",
    )
    .await
}

fn guess_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

async fn write_bytes_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Enriches an [`InstanceSnapshot`] JSON object with active `allowed_events` and resolved `ui` spec.
fn enriched_instance_json(
    reg: &RegisteredMachine,
    snap: &InstanceSnapshot,
) -> anyhow::Result<Value> {
    let mut val = serde_json::to_value(snap)?;
    if let Some(obj) = val.as_object_mut() {
        let (allowed_events, active_ui) = if snap.status.is_terminal() {
            (Vec::new(), None)
        } else {
            let events = reg
                .chart
                .ir
                .allowed_events_for_active_states(&snap.active_states);
            let ui = reg
                .ui_sheet
                .as_ref()
                .and_then(|sheet| {
                    sheet.resolve_for_active_states(&reg.chart.ir, &snap.active_states, &snap.context)
                });
            (events, ui)
        };
        obj.insert("allowed_events".to_owned(), json!(allowed_events));
        obj.insert("ui".to_owned(), active_ui.unwrap_or(Value::Null));
    }
    Ok(val)
}

fn collect_states_with_ui(
    states: &indexmap::IndexMap<StateId, starness_ir::StateNodeDef>,
    ui_sheet: Option<&UiViewSheet>,
    out: &mut Vec<String>,
) {
    let Some(sheet) = ui_sheet else {
        return;
    };
    for (sid, node) in states {
        let has_direct = sheet.views.contains_key(sid);
        let has_tag = node.tags.iter().any(|t| sheet.tag_views.contains_key(t));
        if has_direct || has_tag {
            out.push(sid.as_str().to_owned());
        }
        match &node.kind {
            StateKind::Compound { states: kids, .. } | StateKind::Parallel { states: kids } => {
                collect_states_with_ui(kids, ui_sheet, out);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

fn machine_summary_json(
    reg: &RegisteredMachine,
    counts: &HashMap<String, usize>,
) -> Value {
    let mut states_with_ui = Vec::new();
    collect_states_with_ui(&reg.chart.ir.states, reg.ui_sheet.as_ref(), &mut states_with_ui);
    let initial_active = vec![reg.chart.ir.initial.as_str().to_owned()];
    let initial_ui = reg.ui_sheet.as_ref().and_then(|sheet| {
        sheet.resolve_for_active_states(&reg.chart.ir, &initial_active, &reg.chart.ir.context)
    });
    let initial_allowed_events = reg
        .chart
        .ir
        .allowed_events_for_active_states(&initial_active);
    json!({
        "id": reg.chart.ir.id,
        "description": reg.chart.ir.description,
        "initial": reg.chart.ir.initial.as_str(),
        "completable": reg.completable,
        "default_context": reg.chart.ir.context,
        "instance_counts": counts,
        "states_with_ui": states_with_ui,
        "initial_ui": initial_ui,
        "initial_allowed_events": initial_allowed_events,
    })
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
            .context("invalid JSON payload in POST .../events/<event>")?;
        return Ok((ev_name.to_owned(), parsed));
    }

    if body_str.is_empty() {
        bail!("POST .../events requires a JSON object `{{\"type\": \"...\"}}` or plain event name");
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

async fn stream_instance_trace<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    state: &Arc<ServerState>,
    machine_id: &str,
    instance_id: &str,
    mut cursor_seq: u64,
    as_jsonl: bool,
    follow: bool,
) -> anyhow::Result<()> {
    write_stream_headers(writer, as_jsonl).await?;

    loop {
        // Register the notification future BEFORE reading SQLite so we never miss a wakeup!
        let live_opt = state.get_live(machine_id, instance_id);
        let notify_arc = live_opt.as_ref().map(|l| Arc::clone(&l.trace_notify));
        let notified_fut = notify_arc.as_ref().map(|n| n.notified());

        loop {
            let batch = state.store.read_instance_traces_after(
                machine_id,
                instance_id,
                cursor_seq,
                256,
            )?;
            if batch.is_empty() {
                break;
            }
            for (seq, json_line) in batch {
                write_raw_trace_frame(writer, seq, &json_line, as_jsonl).await?;
                cursor_seq = seq;
            }
        }

        if !follow {
            break;
        }

        match (live_opt, notified_fut) {
            (Some(live), Some(notified)) => {
                if live.trace_closed.load(Ordering::SeqCst) {
                    // Drain once more after `trace_closed` in case final records landed right before load.
                    for (seq, json_line) in state.store.read_instance_traces_after(
                        machine_id,
                        instance_id,
                        cursor_seq,
                        256,
                    )? {
                        write_raw_trace_frame(writer, seq, &json_line, as_jsonl).await?;
                    }
                    break;
                }
                tokio::select! {
                    _ = state.shutdown.notified() => break,
                    _ = notified => {}
                }
            }
            _ => {
                // Instance is no longer live (completed, failed, cancelled, or purged).
                // Perform one final read in case the trace task committed right after `get_live` returned `None`.
                for (seq, json_line) in state.store.read_instance_traces_after(
                    machine_id,
                    instance_id,
                    cursor_seq,
                    256,
                )? {
                    write_raw_trace_frame(writer, seq, &json_line, as_jsonl).await?;
                }
                break;
            }
        }
    }

    let _ = writer.shutdown().await;
    Ok(())
}

async fn stream_global_trace<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    state: &Arc<ServerState>,
    as_jsonl: bool,
    follow: bool,
) -> anyhow::Result<()> {
    write_stream_headers(writer, as_jsonl).await?;
    let mut cursor_rowid: i64 = 0;

    loop {
        let notified = state.global_trace_notify.notified();
        loop {
            let batch = state.store.read_global_traces_after(cursor_rowid, 256)?;
            if batch.is_empty() {
                break;
            }
            for (rowid, _seq, json_line) in batch {
                let sse_id = u64::try_from(rowid).unwrap_or(0);
                write_raw_trace_frame(writer, sse_id, &json_line, as_jsonl).await?;
                cursor_rowid = rowid;
            }
        }

        if !follow {
            break;
        }

        tokio::select! {
            _ = state.shutdown.notified() => break,
            _ = notified => {}
        }
    }

    let _ = writer.shutdown().await;
    Ok(())
}

async fn write_stream_headers<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    as_jsonl: bool,
) -> anyhow::Result<()> {
    let content_type = if as_jsonl {
        "application/x-ndjson; charset=utf-8"
    } else {
        "text/event-stream; charset=utf-8"
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
    );
    writer.write_all(header.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_raw_trace_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    id: u64,
    json_line: &str,
    as_jsonl: bool,
) -> anyhow::Result<()> {
    let frame = if as_jsonl {
        format!("{json_line}\n")
    } else {
        format!("id: {id}\ndata: {json_line}\n\n")
    };
    writer.write_all(frame.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_not_found<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: String,
) -> anyhow::Result<()> {
    write_json_response(writer, "404 Not Found", &json!({ "error": msg })).await
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

fn parse_query_params(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(percent_decode(k), percent_decode(v));
            }
            None => {
                map.insert(percent_decode(pair), "true".to_owned());
            }
        }
    }
    map
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ============================================================================
// Stateless Streamable HTTP MCP Adapter (`/mcp` & `/machines/:machine/mcp`)
// ============================================================================

struct ServeMcpBackend {
    server: Weak<ServerState>,
    machines: Vec<McpMachineEntry>,
}

impl ServeMcpBackend {
    fn upgrade_server(&self) -> anyhow::Result<Arc<ServerState>> {
        self.server
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("starness serve daemon has shut down"))
    }
}

fn instance_snapshot_to_mcp_view(reg: &RegisteredMachine, snap: InstanceSnapshot) -> McpInstanceView {
    let allowed_events = if snap.status.is_terminal() {
        Vec::new()
    } else {
        reg.chart
            .ir
            .allowed_events_for_active_states(&snap.active_states)
    };
    McpInstanceView {
        machine: snap.machine,
        instance_id: Some(snap.id),
        status: snap.status.as_str().to_owned(),
        done: snap.done,
        active_states: snap.active_states,
        allowed_events,
        context: snap.context,
        output: snap.output,
        error: snap.error,
    }
}

impl McpMachineBackend for ServeMcpBackend {
    fn machines(&self) -> &[McpMachineEntry] {
        &self.machines
    }

    fn start_instance<'a>(
        &'a self,
        machine_id: &'a str,
        requested_id: Option<String>,
        initial_context: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<McpInstanceView>> + Send + 'a>> {
        Box::pin(async move {
            let server = self.upgrade_server()?;
            let reg = server
                .machines
                .get(machine_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("machine `{machine_id}` is not registered"))?;
            let id = match requested_id {
                Some(explicit) => explicit,
                None => server.generate_instance_id(),
            };
            let ctx_patch = initial_context.as_ref().and_then(Value::as_object);
            let snap = {
                let _create_guard = server.create_lock.lock().await;
                spawn_or_rehydrate_instance(&server, &reg, id.clone(), ctx_patch, None).await?
            };
            if let Some(live) = server.get_live(machine_id, &id) {
                let _ = wait_actor_until_settled(
                    &live.actor,
                    &live.ir,
                    DEFAULT_MCP_SETTLE_TIMEOUT,
                )
                .await;
                let settled = sync_and_maybe_finalize(&server, &live, None).await?;
                return Ok(instance_snapshot_to_mcp_view(&reg, settled));
            }
            Ok(instance_snapshot_to_mcp_view(&reg, snap))
        })
    }

    fn get_state<'a>(
        &'a self,
        machine_id: &'a str,
        instance_id: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<McpInstanceView>> + Send + 'a>> {
        Box::pin(async move {
            let server = self.upgrade_server()?;
            let reg = server
                .machines
                .get(machine_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("machine `{machine_id}` is not registered"))?;
            let Some(inst_id) = instance_id else {
                let ir = &reg.chart.ir;
                let initial_path = format!("{}.{}", ir.id, ir.initial);
                let allowed_events =
                    ir.allowed_events_for_active_states(std::slice::from_ref(&initial_path));
                return Ok(McpInstanceView {
                    machine: ir.id.clone(),
                    instance_id: None,
                    status: "ephemeral".to_owned(),
                    done: false,
                    active_states: vec![initial_path],
                    allowed_events,
                    context: ir.context.clone(),
                    output: Value::Null,
                    error: None,
                });
            };
            if let Some(live) = server.get_live(machine_id, inst_id) {
                let snap = sync_and_maybe_finalize(&server, &live, None).await?;
                return Ok(instance_snapshot_to_mcp_view(&reg, snap));
            }
            let Some(snap) = server.store.get_instance(machine_id, inst_id)? else {
                bail!("instance `{inst_id}` not found for machine `{machine_id}`");
            };
            Ok(instance_snapshot_to_mcp_view(&reg, snap))
        })
    }

    fn dispatch_event<'a>(
        &'a self,
        machine_id: &'a str,
        instance_id: Option<&'a str>,
        event_type: &'a str,
        payload: Value,
        initial_context: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<McpInstanceView>> + Send + 'a>> {
        Box::pin(async move {
            let server = self.upgrade_server()?;
            let reg = server
                .machines
                .get(machine_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("machine `{machine_id}` is not registered"))?;

            // Stateless / ephemeral execution when `instance_id` is omitted.
            let Some(inst_id) = instance_id else {
                return run_ephemeral_event(
                    &reg.chart,
                    &reg.callbacks,
                    event_type,
                    payload,
                    initial_context,
                    DEFAULT_MCP_SETTLE_TIMEOUT,
                )
                .await;
            };

            let live = match ensure_live_instance(&server, &reg, machine_id, inst_id).await? {
                EnsureLiveResult::Live(l) => l,
                EnsureLiveResult::Terminal(snap) => {
                    bail!(
                        "instance `{inst_id}` is `{}` and no longer accepts events",
                        snap.status.as_str()
                    );
                }
                EnsureLiveResult::NotFound => {
                    let _create_guard = server.create_lock.lock().await;
                    let ctx_patch = initial_context.as_ref().and_then(Value::as_object);
                    let _ = spawn_or_rehydrate_instance(
                        &server,
                        &reg,
                        inst_id.to_owned(),
                        ctx_patch,
                        None,
                    )
                    .await?;
                    match ensure_live_instance(&server, &reg, machine_id, inst_id).await? {
                        EnsureLiveResult::Live(l) => l,
                        EnsureLiveResult::Terminal(snap) => {
                            return Ok(instance_snapshot_to_mcp_view(&reg, *snap));
                        }
                        EnsureLiveResult::NotFound => {
                            bail!("failed to initialize instance `{inst_id}`");
                        }
                    }
                }
            };

            let _ev_guard = live.event_lock.lock().await;
            if payload.is_null() {
                live.actor.send(event_type).await?;
            } else {
                live.actor.send_with_payload(event_type, payload).await?;
            }
            let _ = wait_actor_until_settled(&live.actor, &live.ir, DEFAULT_MCP_SETTLE_TIMEOUT).await;
            let snap = sync_and_maybe_finalize(&server, &live, None).await?;
            Ok(instance_snapshot_to_mcp_view(&reg, snap))
        })
    }
}

async fn handle_mcp_http_request(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    service: &HttpMcpService,
    method: &str,
    raw_target: &str,
    raw_headers: &[(String, String)],
    body_bytes: Vec<u8>,
) -> anyhow::Result<()> {
    let http_method = http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::POST);
    let mut req_builder = http::Request::builder()
        .method(http_method)
        .uri(raw_target);
    for (k, v) in raw_headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            http::header::HeaderValue::from_str(v),
        ) {
            req_builder = req_builder.header(name, val);
        }
    }
    let http_req = req_builder
        .body(Full::new(Bytes::from(body_bytes)))
        .context("failed constructing HTTP request for MCP service")?;

    let resp = service.handle(http_req).await;
    let status = resp.status();
    let status_line = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("OK")
    );
    let is_sse = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

    if is_sse {
        let mut head = format!(
            "HTTP/1.1 {status_line}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-cache\r\nConnection: close\r\n"
        );
        for (name, value) in resp.headers() {
            if name == http::header::CONNECTION
                || name == http::header::CONTENT_LENGTH
                || name == http::header::TRANSFER_ENCODING
            {
                continue;
            }
            if let Ok(val_str) = value.to_str() {
                head.push_str(&format!("{name}: {val_str}\r\n"));
            }
        }
        head.push_str("\r\n");
        writer.write_all(head.as_bytes()).await?;
        writer.flush().await?;

        let mut body = resp.into_body();
        while let Some(frame_res) = body.frame().await {
            if let Ok(frame) = frame_res
                && let Some(data) = frame.data_ref()
                && !data.is_empty()
            {
                if writer.write_all(data).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        }
    } else {
        let (parts, body) = resp.into_parts();
        let bytes = body
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        let mut head = format!(
            "HTTP/1.1 {status_line}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n",
            bytes.len()
        );
        for (name, value) in &parts.headers {
            if name == http::header::CONNECTION
                || name == http::header::CONTENT_LENGTH
                || name == http::header::TRANSFER_ENCODING
            {
                continue;
            }
            if let Ok(val_str) = value.to_str() {
                head.push_str(&format!("{name}: {val_str}\r\n"));
            }
        }
        head.push_str("\r\n");
        writer.write_all(head.as_bytes()).await?;
        if !bytes.is_empty() {
            writer.write_all(&bytes).await?;
        }
        writer.flush().await?;
    }

    Ok(())
}

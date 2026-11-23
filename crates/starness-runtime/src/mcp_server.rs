//! Model Context Protocol (MCP) server powered by `rmcp` (supporting `2026-07-28` stateless MCP
//! as well as legacy MCP clients).
//!
//! Exposes one or more compiled `.star` statecharts as MCP tools and resources over:
//! - `stdio` (`starness mcp <file.star>`)
//! - Stateless Streamable HTTP (`starness serve` at `/mcp` and `/machines/:machine/mcp`)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde::Serialize;
use serde_json::{Map, Value, json};
use starness_ir::{MachineDef, StateId, StateKind, StateNodeDef};
use starness_starlark::CompiledChart;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use xstate_statemachine::core::Status;

use crate::{HostCallbacks, StarnessActor, active_states_have_autonomous_work, render_mermaid};

/// Default maximum time an MCP tool call waits for autonomous statechart invocations to settle.
pub const DEFAULT_MCP_SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for standalone `starness mcp` servers.
#[derive(Debug, Clone)]
pub struct McpServerOptions {
    /// When `true`, every event `tools/call` without an explicit `instance_id` spawns an
    /// independent ephemeral `StarnessActor`, runs to quiescence, and drops it (100% concurrent).
    /// When `false` (default for stdio), calls without `instance_id` route to a `"default"`
    /// session instance so multi-step stateful workflows persist across turns.
    pub ephemeral: bool,
    /// Maximum duration to wait for invoked services/delays to settle after dispatching an event.
    pub settle_timeout: Duration,
}

impl Default for McpServerOptions {
    fn default() -> Self {
        Self {
            ephemeral: false,
            settle_timeout: DEFAULT_MCP_SETTLE_TIMEOUT,
        }
    }
}

/// Static metadata for a machine registered in an MCP server.
#[derive(Clone)]
pub struct McpMachineEntry {
    pub machine_id: String,
    pub chart: CompiledChart,
    pub diagram: String,
}

/// JSON snapshot returned by Starness MCP tools and resources.
#[derive(Debug, Clone, Serialize)]
pub struct McpInstanceView {
    pub machine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub status: String,
    pub done: bool,
    pub active_states: Vec<String>,
    pub allowed_events: Vec<String>,
    pub context: Value,
    pub output: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Backend abstraction allowing [`StarnessMcpHandler`] to run either standalone (`starness mcp`)
/// or directly on top of `starness serve`'s SQLite-backed multi-instance registry.
pub trait McpMachineBackend: Send + Sync + 'static {
    fn machines(&self) -> &[McpMachineEntry];

    fn start_instance<'a>(
        &'a self,
        machine_id: &'a str,
        requested_id: Option<String>,
        initial_context: Option<Value>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>>;

    fn get_state<'a>(
        &'a self,
        machine_id: &'a str,
        instance_id: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>>;

    fn dispatch_event<'a>(
        &'a self,
        machine_id: &'a str,
        instance_id: Option<&'a str>,
        event_type: &'a str,
        payload: Value,
        initial_context: Option<Value>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>>;
}

/// Waits reactively until `actor` either finishes/errors or reaches a quiescent waiting state
/// with no autonomous invocations/delays pending, bounded by `timeout`.
pub async fn wait_actor_until_settled(
    actor: &StarnessActor,
    ir: &MachineDef,
    timeout: Duration,
) -> Result<(), String> {
    let mut rx = actor.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        {
            let view = rx.borrow();
            if let Some(err) = &view.error {
                return Err(err.clone());
            }
            if actor.is_done() || view.status != Status::Running {
                return Ok(());
            }
            let active = actor.active_states();
            if !active_states_have_autonomous_work(ir, &active) {
                return Ok(());
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }

        match tokio::time::timeout(remaining, rx.changed()).await {
            Ok(Ok(())) => continue,
            Ok(Err(_)) | Err(_) => return Ok(()),
        }
    }
}

/// Builds a [`McpInstanceView`] from a live [`StarnessActor`].
pub async fn snapshot_actor_view(
    actor: &StarnessActor,
    ir: &MachineDef,
    instance_id: Option<String>,
) -> McpInstanceView {
    let view = actor.subscribe().borrow().clone();
    let active_states = actor.active_states();
    let done = actor.is_done() || view.status == Status::Done;
    let status = if view.error.is_some() {
        "failed"
    } else if done {
        "completed"
    } else if view.status == Status::Stopped {
        "cancelled"
    } else if active_states_have_autonomous_work(ir, &active_states) {
        "active"
    } else {
        "waiting"
    }
    .to_owned();

    let allowed_events = if done || view.error.is_some() || view.status == Status::Stopped {
        Vec::new()
    } else {
        ir.allowed_events_for_active_states(&active_states)
    };

    let context = actor.context().await.unwrap_or(Value::Null);
    let output = actor.output();

    McpInstanceView {
        machine: ir.id.clone(),
        instance_id,
        status,
        done,
        active_states,
        allowed_events,
        context,
        output,
        error: view.error,
    }
}

/// Spawns a fresh ephemeral [`StarnessActor`], optionally merges `initial_context`, dispatches
/// `event_type`, waits for autonomous work to settle, captures the view, and stops the actor.
pub async fn run_ephemeral_event(
    chart: &CompiledChart,
    callbacks: &HostCallbacks,
    event_type: &str,
    payload: Value,
    initial_context: Option<Value>,
    settle_timeout: Duration,
) -> anyhow::Result<McpInstanceView> {
    let mut chart_copy = chart.clone();
    if let Some(ctx_override) = initial_context {
        merge_context_override(&mut chart_copy.ir.context, ctx_override)?;
    }

    let actor = StarnessActor::spawn(&chart_copy, callbacks.clone())?;
    // Allow any initial autonomous work to settle before dispatching the external event.
    let _ = wait_actor_until_settled(&actor, &chart_copy.ir, settle_timeout).await;

    if payload.is_null() {
        actor.send(event_type).await?;
    } else {
        actor.send_with_payload(event_type, payload).await?;
    }

    let _ = wait_actor_until_settled(&actor, &chart_copy.ir, settle_timeout).await;
    let view = snapshot_actor_view(&actor, &chart_copy.ir, None).await;
    actor.stop().await;
    Ok(view)
}

fn merge_context_override(base: &mut Value, override_val: Value) -> anyhow::Result<()> {
    let Some(override_map) = override_val.as_object() else {
        bail!("`context` override must be a JSON object");
    };
    if let Some(base_map) = base.as_object_mut() {
        for (k, v) in override_map {
            base_map.insert(k.clone(), v.clone());
        }
    } else {
        *base = Value::Object(override_map.clone());
    }
    Ok(())
}

// ============================================================================
// Event Tool Discovery & Routing
// ============================================================================

#[derive(Debug, Clone)]
struct EventToolSpec {
    event_name: String,
    transitions: Vec<String>,
    descriptions: Vec<String>,
}

fn collect_machine_events(ir: &MachineDef) -> Vec<EventToolSpec> {
    let mut by_event: IndexMap<String, EventToolSpec> = IndexMap::new();
    collect_events_recursive(&ir.states, &mut by_event);
    by_event.into_values().collect()
}

fn collect_events_recursive(
    states: &IndexMap<StateId, StateNodeDef>,
    out: &mut IndexMap<String, EventToolSpec>,
) {
    for (sid, node) in states {
        for tr in &node.on {
            if tr.forbidden || tr.event == "*" || tr.event.starts_with("done.") || tr.event.starts_with("error.") {
                continue;
            }
            let entry = out
                .entry(tr.event.clone())
                .or_insert_with(|| EventToolSpec {
                    event_name: tr.event.clone(),
                    transitions: Vec::new(),
                    descriptions: Vec::new(),
                });
            let edge = match &tr.target {
                Some(tgt) => format!("{sid} -> {tgt}"),
                None => format!("{sid} (internal)"),
            };
            if !entry.transitions.contains(&edge) {
                entry.transitions.push(edge);
            }
            if let Some(desc) = &tr.description
                && !desc.trim().is_empty()
                && !entry.descriptions.contains(desc)
            {
                entry.descriptions.push(desc.clone());
            }
        }
        match &node.kind {
            StateKind::Compound { states: children, .. }
            | StateKind::Parallel { states: children } => {
                collect_events_recursive(children, out);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

#[derive(Debug, Clone)]
enum ToolRoute {
    StartInstance { machine_id: String },
    GetState { machine_id: String },
    DispatchEvent { machine_id: String, event_name: String },
}

fn build_tool_catalog(machines: &[McpMachineEntry]) -> (Vec<Tool>, HashMap<String, ToolRoute>) {
    let multi = machines.len() > 1;
    let mut tools = Vec::new();
    let mut routes = HashMap::new();

    for entry in machines {
        let mid = &entry.machine_id;
        let prefix = if multi {
            format!("{mid}__")
        } else {
            String::new()
        };

        // 1. One tool per external event declared in the statechart.
        for ev in collect_machine_events(&entry.chart.ir) {
            let tool_name = format!("{prefix}{}", ev.event_name);
            let mut desc = if !ev.descriptions.is_empty() {
                ev.descriptions.join(" | ")
            } else {
                format!("Dispatch event `{}` to machine `{mid}`", ev.event_name)
            };
            if !ev.transitions.is_empty() {
                desc.push_str(&format!(" (transitions: {})", ev.transitions.join(", ")));
            }
            let schema = json!({
                "type": "object",
                "properties": {
                    "instance_id": {
                        "type": "string",
                        "description": "Optional workflow instance ID for stateful execution. Omit for default/ephemeral execution."
                    },
                    "payload": {
                        "type": "object",
                        "description": "Optional event payload object. Top-level fields outside `instance_id`/`context` are also accepted as payload."
                    },
                    "context": {
                        "type": "object",
                        "description": "Optional initial context overrides (applied when starting a fresh/ephemeral instance)."
                    }
                },
                "additionalProperties": true
            });
            let tool = Tool::new(
                tool_name.clone(),
                desc,
                Arc::new(schema.as_object().cloned().unwrap_or_default()),
            );
            routes.insert(
                tool_name,
                ToolRoute::DispatchEvent {
                    machine_id: mid.clone(),
                    event_name: ev.event_name,
                },
            );
            tools.push(tool);
        }

        // 2. `get_state`
        let get_state_name = format!("{prefix}get_state");
        let get_state_schema = json!({
            "type": "object",
            "properties": {
                "instance_id": {
                    "type": "string",
                    "description": "Optional workflow instance ID."
                }
            }
        });
        tools.push(Tool::new(
            get_state_name.clone(),
            format!("Inspect current active states, allowed events, and context of machine `{mid}`."),
            Arc::new(get_state_schema.as_object().cloned().unwrap_or_default()),
        ));
        routes.insert(
            get_state_name,
            ToolRoute::GetState {
                machine_id: mid.clone(),
            },
        );

        // 3. `start_instance`
        let start_name = format!("{prefix}start_instance");
        let start_schema = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Optional explicit instance ID (auto-generated if omitted)."
                },
                "context": {
                    "type": "object",
                    "description": "Optional initial context overrides merged into the machine's default context."
                }
            }
        });
        tools.push(Tool::new(
            start_name.clone(),
            format!("Create and start a new stateful workflow instance of machine `{mid}`."),
            Arc::new(start_schema.as_object().cloned().unwrap_or_default()),
        ));
        routes.insert(
            start_name,
            ToolRoute::StartInstance {
                machine_id: mid.clone(),
            },
        );
    }

    (tools, routes)
}

fn extract_event_call_args(
    args: Option<&Map<String, Value>>,
) -> (Option<String>, Value, Option<Value>) {
    let Some(map) = args else {
        return (None, Value::Null, None);
    };

    let instance_id = map
        .get("instance_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let initial_context = map.get("context").filter(|v| !v.is_null()).cloned();

    let payload = if let Some(explicit) = map.get("payload") {
        explicit.clone()
    } else {
        let mut rest = Map::new();
        for (k, v) in map {
            if k != "instance_id" && k != "context" {
                rest.insert(k.clone(), v.clone());
            }
        }
        if rest.is_empty() {
            Value::Null
        } else {
            Value::Object(rest)
        }
    };

    (instance_id, payload, initial_context)
}

// ============================================================================
// `rmcp::ServerHandler` Implementation
// ============================================================================

/// `rmcp::ServerHandler` exposing Starness machines as MCP tools & resources.
#[derive(Clone)]
pub struct StarnessMcpHandler {
    backend: Arc<dyn McpMachineBackend>,
    tools: Arc<Vec<Tool>>,
    routes: Arc<HashMap<String, ToolRoute>>,
}

impl StarnessMcpHandler {
    pub fn new(backend: Arc<dyn McpMachineBackend>) -> Self {
        let (tools, routes) = build_tool_catalog(backend.machines());
        Self {
            backend,
            tools: Arc::new(tools),
            routes: Arc::new(routes),
        }
    }
}

impl ServerHandler for StarnessMcpHandler {
    fn get_info(&self) -> ServerConfig {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        let mut cfg = ServerConfig::new(caps);
        cfg.server_info = Implementation::new("starness", env!("CARGO_PKG_VERSION"));
        cfg
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items((*self.tools).clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(route) = self.routes.get(request.name.as_ref()).cloned() else {
            return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("Unknown tool `{}`", request.name)),
            ])));
        };

        let result = match route {
            ToolRoute::StartInstance { machine_id } => {
                let req_id = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let init_ctx = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get("context"))
                    .filter(|v| !v.is_null())
                    .cloned();
                self.backend.start_instance(&machine_id, req_id, init_ctx).await
            }
            ToolRoute::GetState { machine_id } => {
                let inst_id = request
                    .arguments
                    .as_ref()
                    .and_then(|m| m.get("instance_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                self.backend.get_state(&machine_id, inst_id).await
            }
            ToolRoute::DispatchEvent {
                machine_id,
                event_name,
            } => {
                let (inst_id, payload, init_ctx) =
                    extract_event_call_args(request.arguments.as_ref());
                self.backend
                    .dispatch_event(
                        &machine_id,
                        inst_id.as_deref(),
                        &event_name,
                        payload,
                        init_ctx,
                    )
                    .await
            }
        };

        match result {
            Ok(view) => {
                let val = serde_json::to_value(&view).unwrap_or_else(|_| json!({}));
                let text = serde_json::to_string_pretty(&val).unwrap_or_default();
                let mut call_res = CallToolResult::success(vec![ContentBlock::text(text)]);
                call_res.structured_content = Some(val);
                Ok(CallToolResponse::Complete(call_res))
            }
            Err(err) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!("{err:#}")),
            ]))),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources = Vec::with_capacity(self.backend.machines().len() * 2);
        for entry in self.backend.machines() {
            let mid = &entry.machine_id;
            let mut diag = Resource::new(
                format!("starness://{mid}/diagram"),
                format!("{mid} Mermaid diagram"),
            );
            diag.mime_type = Some("text/plain".to_owned());
            diag.description = Some(format!(
                "Harel stateDiagram-v2 Mermaid graph for machine `{mid}`."
            ));
            resources.push(diag);

            let mut state_res = Resource::new(
                format!("starness://{mid}/state"),
                format!("{mid} current state"),
            );
            state_res.mime_type = Some("application/json".to_owned());
            state_res.description = Some(format!(
                "Current state, context, and allowed events for machine `{mid}`."
            ));
            resources.push(state_res);
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri.as_str();
        let Some(rest) = uri.strip_prefix("starness://") else {
            return Err(McpError::invalid_params(
                format!("unsupported resource URI `{uri}`"),
                None,
            ));
        };
        let Some((machine_id, kind)) = rest.split_once('/') else {
            return Err(McpError::invalid_params(
                format!("invalid starness resource URI `{uri}`"),
                None,
            ));
        };

        let Some(entry) = self
            .backend
            .machines()
            .iter()
            .find(|m| m.machine_id == machine_id)
        else {
            return Err(McpError::invalid_params(
                format!("unknown machine `{machine_id}`"),
                None,
            ));
        };

        match kind {
            "diagram" => {
                let mut content = ResourceContents::text(entry.diagram.clone(), uri.to_owned());
                if let ResourceContents::TextResourceContents { mime_type, .. } = &mut content {
                    *mime_type = Some("text/plain".to_owned());
                }
                Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                    vec![content],
                )))
            }
            "state" => {
                let view = self
                    .backend
                    .get_state(machine_id, None)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
                let json_text = serde_json::to_string_pretty(&view).unwrap_or_default();
                let mut content = ResourceContents::text(json_text, uri.to_owned());
                if let ResourceContents::TextResourceContents { mime_type, .. } = &mut content {
                    *mime_type = Some("application/json".to_owned());
                }
                Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                    vec![content],
                )))
            }
            other => Err(McpError::invalid_params(
                format!("unknown starness resource `{other}` (expected `diagram` or `state`)"),
                None,
            )),
        }
    }
}

// ============================================================================
// Standalone Backend (`starness mcp <file.star>`)
// ============================================================================

struct StandaloneInstance {
    ir: MachineDef,
    actor: StarnessActor,
    event_lock: AsyncMutex<()>,
}

struct StandaloneMcpBackend {
    machines: Vec<McpMachineEntry>,
    callbacks: HostCallbacks,
    options: McpServerOptions,
    instances: Mutex<HashMap<String, Arc<StandaloneInstance>>>,
    next_id: AtomicU64,
}

impl StandaloneMcpBackend {
    fn new(
        chart: CompiledChart,
        callbacks: HostCallbacks,
        options: McpServerOptions,
    ) -> anyhow::Result<Self> {
        let diagram = render_mermaid(&chart)?;
        let entry = McpMachineEntry {
            machine_id: chart.ir.id.clone(),
            chart,
            diagram,
        };
        Ok(Self {
            machines: vec![entry],
            callbacks,
            options,
            instances: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    fn entry(&self) -> &McpMachineEntry {
        &self.machines[0]
    }

    async fn spawn_and_register(
        &self,
        instance_id: String,
        initial_context: Option<Value>,
    ) -> anyhow::Result<Arc<StandaloneInstance>> {
        let mut chart_copy = self.entry().chart.clone();
        if let Some(ctx_override) = initial_context {
            merge_context_override(&mut chart_copy.ir.context, ctx_override)?;
        }
        let actor = StarnessActor::spawn(&chart_copy, self.callbacks.clone())?;
        let _ = wait_actor_until_settled(&actor, &chart_copy.ir, self.options.settle_timeout).await;
        let inst = Arc::new(StandaloneInstance {
            ir: chart_copy.ir,
            actor,
            event_lock: AsyncMutex::new(()),
        });
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(instance_id, Arc::clone(&inst));
        Ok(inst)
    }

    async fn get_or_create_instance(
        &self,
        instance_id: &str,
        initial_context: Option<Value>,
    ) -> anyhow::Result<Arc<StandaloneInstance>> {
        if let Some(existing) = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(instance_id)
            .cloned()
        {
            return Ok(existing);
        }
        self.spawn_and_register(instance_id.to_owned(), initial_context)
            .await
    }
}

impl McpMachineBackend for StandaloneMcpBackend {
    fn machines(&self) -> &[McpMachineEntry] {
        &self.machines
    }

    fn start_instance<'a>(
        &'a self,
        _machine_id: &'a str,
        requested_id: Option<String>,
        initial_context: Option<Value>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>> {
        Box::pin(async move {
            let id = requested_id.unwrap_or_else(|| {
                let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
                format!("inst-{seq}")
            });
            let inst = self.spawn_and_register(id.clone(), initial_context).await?;
            Ok(snapshot_actor_view(&inst.actor, &inst.ir, Some(id)).await)
        })
    }

    fn get_state<'a>(
        &'a self,
        _machine_id: &'a str,
        instance_id: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>> {
        Box::pin(async move {
            if self.options.ephemeral && instance_id.is_none() {
                let ir = &self.entry().chart.ir;
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
            }
            let id = instance_id.unwrap_or("default");
            let inst = self.get_or_create_instance(id, None).await?;
            Ok(snapshot_actor_view(&inst.actor, &inst.ir, Some(id.to_owned())).await)
        })
    }

    fn dispatch_event<'a>(
        &'a self,
        _machine_id: &'a str,
        instance_id: Option<&'a str>,
        event_type: &'a str,
        payload: Value,
        initial_context: Option<Value>,
    ) -> BoxFuture<'a, anyhow::Result<McpInstanceView>> {
        Box::pin(async move {
            if self.options.ephemeral && instance_id.is_none() {
                return run_ephemeral_event(
                    &self.entry().chart,
                    &self.callbacks,
                    event_type,
                    payload,
                    initial_context,
                    self.options.settle_timeout,
                )
                .await;
            }

            let id = instance_id.unwrap_or("default");
            let inst = self.get_or_create_instance(id, initial_context).await?;
            let _guard = inst.event_lock.lock().await;
            if payload.is_null() {
                inst.actor.send(event_type).await?;
            } else {
                inst.actor.send_with_payload(event_type, payload).await?;
            }
            let _ = wait_actor_until_settled(&inst.actor, &inst.ir, self.options.settle_timeout).await;
            Ok(snapshot_actor_view(&inst.actor, &inst.ir, Some(id.to_owned())).await)
        })
    }
}

/// Exposes a compiled `.star` statechart over `stdio` as an MCP server (`starness mcp`).
pub async fn serve_chart_as_mcp_stdio(
    chart: CompiledChart,
    callbacks: HostCallbacks,
    options: McpServerOptions,
) -> anyhow::Result<()> {
    let backend = Arc::new(StandaloneMcpBackend::new(chart, callbacks, options)?);
    let handler = StarnessMcpHandler::new(backend);
    let service = handler
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to initialize stdio MCP server")?;
    service
        .waiting()
        .await
        .context("stdio MCP server exited with error")?;
    Ok(())
}

/// Exposes a compiled `.star` statechart over any async `Read + Write` stream (used in tests).
pub async fn serve_chart_as_mcp_io<T>(
    chart: CompiledChart,
    callbacks: HostCallbacks,
    options: McpServerOptions,
    io: T,
) -> anyhow::Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let backend = Arc::new(StandaloneMcpBackend::new(chart, callbacks, options)?);
    let handler = StarnessMcpHandler::new(backend);
    let service = handler
        .serve(io)
        .await
        .context("failed to initialize MCP server on IO stream")?;
    service
        .waiting()
        .await
        .context("MCP server on IO stream exited with error")?;
    Ok(())
}

/// Builds a stateless (`2026-07-28` MCP) [`StreamableHttpService`] for any [`McpMachineBackend`].
pub fn build_stateless_http_mcp_service(
    backend: Arc<dyn McpMachineBackend>,
    cancel_token: CancellationToken,
) -> StreamableHttpService<StarnessMcpHandler, LocalSessionManager> {
    let handler = StarnessMcpHandler::new(backend);
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancel_token)
        .disable_allowed_hosts();
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use starness_starlark::compile_starlark_str;

    const COUNTER_STAR: &str = r#"
Ctx = record(total = field(int, 0))
S = state_space("ready", "done")

chart = machine(
    id = "counter",
    initial = S.ready,
    context = Ctx(),
    states = {
        S.ready: state(
            on = [
                transition(
                    event = "ADD",
                    target = S.ready,
                    assign = lambda ctx, ev: Ctx(total = ctx.total + ev["amount"]),
                    description = "Add an amount to the running total",
                ),
                transition(
                    event = "FINISH",
                    target = S.done,
                    description = "Complete the counter workflow",
                ),
            ],
        ),
        S.done: final_state(),
    },
)
"#;

    #[tokio::test]
    async fn mcp_server_exposes_tools_resources_stateful_sessions_and_concurrent_ephemeral_calls() {
        let compiled = compile_starlark_str("counter.star", COUNTER_STAR, Some("chart"))
            .expect("compile counter.star");

        // 1. Stateful mode over duplex IO
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let chart_clone = compiled.clone();
        let server_task = tokio::spawn(async move {
            serve_chart_as_mcp_io(
                chart_clone,
                HostCallbacks::with_builtins(),
                McpServerOptions::default(),
                server_io,
            )
            .await
            .expect("mcp server");
        });

        let client = ().serve(client_io).await.expect("mcp client connect");
        let tools = client.list_all_tools().await.expect("list_all_tools");
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(tool_names.contains(&"ADD"), "missing ADD in {tool_names:?}");
        assert!(
            tool_names.contains(&"FINISH"),
            "missing FINISH in {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"get_state"),
            "missing get_state in {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"start_instance"),
            "missing start_instance in {tool_names:?}"
        );

        // Verify MCP resources (`diagram` + `state`)
        let resources = client
            .list_all_resources()
            .await
            .expect("list_all_resources");
        assert_eq!(resources.len(), 2);
        let diag = client
            .read_resource(ReadResourceRequestParams::new("starness://counter/diagram"))
            .await
            .expect("read diagram resource");
        let diag_json = serde_json::to_value(&diag).unwrap();
        assert!(
            diag_json.to_string().contains("stateDiagram-v2"),
            "expected Mermaid diagram in {diag_json}"
        );

        // Default session accumulates across calls
        let r1 = client
            .call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"amount": 3}).as_object().unwrap().clone(),
                ),
            )
            .await
            .expect("ADD 3");
        assert_eq!(r1.structured_content.unwrap()["context"]["total"], json!(3));

        let r2 = client
            .call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"amount": 4}).as_object().unwrap().clone(),
                ),
            )
            .await
            .expect("ADD 4");
        assert_eq!(r2.structured_content.unwrap()["context"]["total"], json!(7));

        // Explicit `instance_id`s stay isolated from each other and from default
        let (ra, rb) = tokio::join!(
            client.call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"instance_id": "a", "amount": 10})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
            client.call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"instance_id": "b", "amount": 25})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
        );
        assert_eq!(
            ra.unwrap().structured_content.unwrap()["context"]["total"],
            json!(10)
        );
        assert_eq!(
            rb.unwrap().structured_content.unwrap()["context"]["total"],
            json!(25)
        );

        client.cancel().await.expect("cancel stateful client");
        server_task.await.expect("join stateful server");

        // 2. Ephemeral mode: concurrent calls without `instance_id` never share state
        let (eph_client_io, eph_server_io) = tokio::io::duplex(64 * 1024);
        let eph_server = tokio::spawn(async move {
            serve_chart_as_mcp_io(
                compiled,
                HostCallbacks::with_builtins(),
                McpServerOptions {
                    ephemeral: true,
                    ..McpServerOptions::default()
                },
                eph_server_io,
            )
            .await
            .expect("ephemeral mcp server");
        });

        let eph_client = ().serve(eph_client_io).await.expect("ephemeral client");
        let (e1, e2, e3) = tokio::join!(
            eph_client.call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"amount": 100}).as_object().unwrap().clone(),
                ),
            ),
            eph_client.call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"amount": 200}).as_object().unwrap().clone(),
                ),
            ),
            eph_client.call_tool(
                CallToolRequestParams::new("ADD").with_arguments(
                    json!({"amount": 300}).as_object().unwrap().clone(),
                ),
            ),
        );
        assert_eq!(
            e1.unwrap().structured_content.unwrap()["context"]["total"],
            json!(100)
        );
        assert_eq!(
            e2.unwrap().structured_content.unwrap()["context"]["total"],
            json!(200)
        );
        assert_eq!(
            e3.unwrap().structured_content.unwrap()["context"]["total"],
            json!(300)
        );

        eph_client.cancel().await.expect("cancel eph client");
        eph_server.await.expect("join eph server");
    }
}

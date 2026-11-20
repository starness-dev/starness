//! Compiles [`CompiledChart`] into `xstate-statemachine` (`xstate-core` + `xstate-tokio`),
//! bridging pure frozen Starlark guards, delays & reducers with effectful Rust host callbacks.

pub mod builtins;
pub mod trace;

pub use trace::{
    ProgressKind, TraceEvent, TraceEventInfo, TraceHub, TraceReceiver, TraceRecord,
    progress_enabled, report_progress,
};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use starchart_ir::{
    ActionDef, DelayRef, GuardRef, InvokeDef, MachineDef, OutcomeDef, StateId, StateKind,
    StateNodeDef, TransitionDef,
};
use starchart_starlark::{
    BUILTIN_ACP_SERVICE, BUILTIN_EXEC_SERVICE, BUILTIN_HTTP_SERVICE, CompiledChart,
    INVOKE_INPUT_SLOT_KEY,
};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, OwnedFrozenValue, Value as StarlarkVal};
use thiserror::Error;
use xstate_statemachine::core::builder::{
    Action as BuilderAction, DelayKey, Guard as BuilderGuard, History as BuilderHistory,
    Invoke as BuilderInvoke, MachineBuilder, StateBuilder, Transition as BuilderTransition,
};
use xstate_statemachine::core::config::StateConfig;
use xstate_statemachine::core::export;
use xstate_statemachine::core::{
    ActionScope, DynContext, Error as XStateError, Event, Machine, MachineLogic, RealClock,
    SimulatedClock, Status, SyncInterpreter,
};
use xstate_statemachine::tokio::{
    Interpreter as AsyncInterpreter, Options as ActorOptions, spawn_with_observers,
};

/// Errors produced during machine compilation or runtime execution.
#[derive(Debug, Error)]
pub enum ChartRuntimeError {
    #[error("failed to compile statechart into runtime engine: {0}")]
    EngineCompile(String),

    #[error("runtime execution error: {0}")]
    Execution(String),
}

impl From<XStateError> for ChartRuntimeError {
    fn from(err: XStateError) -> Self {
        Self::Execution(err.to_string())
    }
}

/// Event view passed to Rust host callbacks.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartEvent {
    pub event_type: String,
    pub payload: Value,
}

impl ChartEvent {
    fn from_xstate(event: &Event) -> Self {
        Self {
            event_type: event.ty.to_string(),
            payload: event.payload.clone(),
        }
    }
}

type SyncActionHandler =
    Arc<dyn Fn(&mut Value, &ChartEvent) -> anyhow::Result<()> + Send + Sync + 'static>;
type SyncGuardHandler =
    Arc<dyn Fn(&Value, &ChartEvent) -> anyhow::Result<bool> + Send + Sync + 'static>;
type SyncServiceHandler =
    Arc<dyn Fn(&Value, &Value) -> anyhow::Result<Value> + Send + Sync + 'static>;
type SyncDelayHandler =
    Arc<dyn Fn(&Value, &ChartEvent) -> anyhow::Result<f64> + Send + Sync + 'static>;
type AsyncServiceHandler = Arc<
    dyn Fn(Value, Value) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Builder for registering effectful Rust actions, invoked services, delays, and named host guards.
#[derive(Clone, Default)]
pub struct HostCallbacks {
    actions: HashMap<String, SyncActionHandler>,
    guards: HashMap<String, SyncGuardHandler>,
    fixed_delays: HashMap<String, f64>,
    dynamic_delays: HashMap<String, SyncDelayHandler>,
    sync_services: HashMap<String, SyncServiceHandler>,
    async_services: HashMap<String, AsyncServiceHandler>,
    child_machines: HashMap<String, Machine>,
    strict_actions: bool,
}

impl std::fmt::Debug for HostCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCallbacks")
            .field("actions", &self.actions.keys().collect::<Vec<_>>())
            .field("guards", &self.guards.keys().collect::<Vec<_>>())
            .field(
                "fixed_delays",
                &self.fixed_delays.keys().collect::<Vec<_>>(),
            )
            .field(
                "dynamic_delays",
                &self.dynamic_delays.keys().collect::<Vec<_>>(),
            )
            .field(
                "sync_services",
                &self.sync_services.keys().collect::<Vec<_>>(),
            )
            .field(
                "async_services",
                &self.async_services.keys().collect::<Vec<_>>(),
            )
            .field(
                "child_machines",
                &self.child_machines.keys().collect::<Vec<_>>(),
            )
            .field("strict_actions", &self.strict_actions)
            .finish()
    }
}

impl HostCallbacks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a `HostCallbacks` preloaded with all built-in `@starchart/*` runtime drivers
    /// (`@starchart/http`, `@starchart/exec`, `@starchart/acp`).
    #[must_use]
    pub fn with_builtins() -> Self {
        builtins::register_builtin_drivers(Self::default())
    }

    /// Registers all built-in `@starchart/*` runtime drivers onto an existing `HostCallbacks`.
    #[must_use]
    pub fn register_builtins(self) -> Self {
        builtins::register_builtin_drivers(self)
    }

    /// Require every action declared in the Starlark statechart to have an explicit Rust handler.
    /// Defaults to `false` (unregistered actions default to no-ops).
    #[must_use]
    pub fn strict_actions(mut self, strict: bool) -> Self {
        self.strict_actions = strict;
        self
    }

    /// Registers a synchronous Rust action handler invoked by action name (e.g. `"CHARGE_CARD"`).
    #[must_use]
    pub fn on_action<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&mut Value, &ChartEvent) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        self.actions.insert(name.into(), Arc::new(handler));
        self
    }

    /// Registers a named Rust host guard predicate (e.g. `"IS_AUTHORIZED"`).
    #[must_use]
    pub fn on_guard<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Value, &ChartEvent) -> anyhow::Result<bool> + Send + Sync + 'static,
    {
        self.guards.insert(name.into(), Arc::new(handler));
        self
    }

    /// Registers a named fixed delay in milliseconds (referenced in Starlark via `after(delay = "NAME", ...)`).
    #[must_use]
    pub fn on_delay_ms(mut self, name: impl Into<String>, ms: f64) -> Self {
        self.fixed_delays.insert(name.into(), ms);
        self
    }

    /// Registers a named dynamic Rust delay `(ctx, event) -> Result<f64>` in milliseconds.
    #[must_use]
    pub fn on_delay_fn<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Value, &ChartEvent) -> anyhow::Result<f64> + Send + Sync + 'static,
    {
        self.dynamic_delays.insert(name.into(), Arc::new(handler));
        self
    }

    /// Registers a synchronous service `(ctx, input) -> Result<Value>` usable in both
    /// [`ChartSyncRunner`] and [`ChartActor`] via Starlark `invoke(src = ...)`.
    #[must_use]
    pub fn on_sync_service<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Value, &Value) -> anyhow::Result<Value> + Send + Sync + 'static,
    {
        self.sync_services.insert(name.into(), Arc::new(handler));
        self
    }

    /// Registers an asynchronous Tokio service `(ctx, input) -> Future<Result<Value>>`
    /// invoked via Starlark `invoke(src = ...)` and cancelled automatically if the state exits.
    #[must_use]
    pub fn on_async_service<F, Fut>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Value, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<Value>> + Send + 'static,
    {
        self.async_services
            .insert(name.into(), Arc::new(move |ctx, inp| Box::pin(handler(ctx, inp))));
        self
    }

    /// Registers another compiled statechart as an invokable child actor under `name`.
    pub fn on_child_machine(
        mut self,
        name: impl Into<String>,
        child: &CompiledChart,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&child.ir)?;
        self.child_machines.insert(name.into(), machine);
        Ok(self)
    }
}

/// Compiles a pure [`MachineDef`] into an `xstate_core::Machine` using explicit `#<state_id>` node IDs
/// across all state kinds (including `History` pseudo-states).
pub fn compile_ir_to_machine(ir: &MachineDef) -> Result<Machine, ChartRuntimeError> {
    let mut builder = MachineBuilder::new(&ir.id)
        .initial(ir.initial.as_str())
        .context(ir.context.clone());

    if let Some(out) = &ir.output {
        builder = builder.output(out.clone());
    }
    if let Some(desc) = &ir.description {
        builder = builder.description(desc);
    }

    for (state_id, node) in &ir.states {
        builder = add_root_child_state(builder, state_id, node);
    }

    let mut config = builder
        .build_config()
        .map_err(|e| ChartRuntimeError::EngineCompile(e.to_string()))?;

    // Ensure every child node (including History pseudo-states) carries `custom_id = Some(key)`
    // so `#<state_id>` resolves unambiguously from any depth in the hierarchy.
    for child in &mut config.root.states {
        assign_custom_ids_recursive(child);
    }

    Machine::from_config(config).map_err(|e| ChartRuntimeError::EngineCompile(e.to_string()))
}

fn assign_custom_ids_recursive(state: &mut StateConfig) {
    if state.custom_id.is_none() {
        state.custom_id = Some(state.key.clone());
    }
    for child in &mut state.states {
        assign_custom_ids_recursive(child);
    }
}

/// Renders a clean Harel `stateDiagram-v2` Mermaid diagram from a [`CompiledChart`].
pub fn render_mermaid(chart: &CompiledChart) -> Result<String, ChartRuntimeError> {
    let _machine = compile_ir_to_machine(&chart.ir)?;

    let guard_labels: HashMap<String, String> = chart
        .guards
        .iter()
        .map(|(slot_name, val)| (slot_name.clone(), extract_starlark_fn_name(val, slot_name)))
        .collect();

    let reducer_labels: HashMap<String, String> = chart
        .reducers
        .iter()
        .map(|(slot_name, val)| (slot_name.clone(), extract_starlark_fn_name(val, slot_name)))
        .collect();

    let delay_labels: HashMap<String, String> = chart
        .delays
        .iter()
        .map(|(slot_name, val)| (slot_name.clone(), extract_starlark_fn_name(val, slot_name)))
        .collect();

    Ok(render_harel_mermaid(
        &chart.ir,
        &guard_labels,
        &reducer_labels,
        &delay_labels,
    ))
}

/// Renders a Harel `stateDiagram-v2` Mermaid diagram directly from [`MachineDef`].
pub fn render_ir_mermaid(ir: &MachineDef) -> Result<String, ChartRuntimeError> {
    let _machine = compile_ir_to_machine(ir)?;
    Ok(render_harel_mermaid(
        ir,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    ))
}

/// Renders the raw `xstate_core::export::to_mermaid` output if needed for XState compatibility.
pub fn render_xstate_mermaid(chart: &CompiledChart) -> Result<String, ChartRuntimeError> {
    let machine = compile_ir_to_machine(&chart.ir)?;
    Ok(export::to_mermaid(&machine))
}

const MERMAID_KEYWORDS: &[&str] = &[
    "state",
    "end",
    "note",
    "direction",
    "class",
    "classDef",
    "style",
    "click",
    "default",
    "scale",
    "hide",
];

fn extract_starlark_fn_name(val: &OwnedFrozenValue, fallback: &str) -> String {
    let raw = val.value().to_str();
    let trimmed = raw
        .trim_start_matches("<function ")
        .trim_start_matches('<')
        .trim_end_matches('>');
    let candidate = trimmed.rsplit('.').next().unwrap_or(trimmed).trim();
    if candidate.is_empty() {
        fallback.to_owned()
    } else {
        candidate.to_owned()
    }
}

fn mermaid_safe_id(raw: &str) -> String {
    let is_ident = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if is_ident && !MERMAID_KEYWORDS.contains(&raw) {
        raw.to_owned()
    } else {
        let sanitized: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("s_{sanitized}")
    }
}

fn format_guard_label(guard: &GuardRef, guard_labels: &HashMap<String, String>) -> String {
    match guard {
        GuardRef::Named(name) => name.clone(),
        GuardRef::StarlarkSlot(slot) => {
            let key = GuardRef::starlark_slot_name(*slot);
            guard_labels.get(&key).cloned().unwrap_or(key)
        }
        GuardRef::StateIn(state_id) => format!("in {state_id}"),
        GuardRef::AllOf(items) => items
            .iter()
            .map(|g| format_guard_label(g, guard_labels))
            .collect::<Vec<_>>()
            .join(" && "),
        GuardRef::AnyOf(items) => items
            .iter()
            .map(|g| format_guard_label(g, guard_labels))
            .collect::<Vec<_>>()
            .join(" || "),
        GuardRef::Not(inner) => format!("!{}", format_guard_label(inner, guard_labels)),
    }
}

fn format_transition_label(
    tr: &TransitionDef,
    guard_labels: &HashMap<String, String>,
    reducer_labels: &HashMap<String, String>,
) -> String {
    let mut label = tr.event.replace([';', '\n'], " ");

    if let Some(guard) = &tr.guard {
        let g = format_guard_label(guard, guard_labels);
        label.push_str(&format!(" [{g}]"));
    }

    let mut effects: Vec<String> = Vec::new();
    if let Some(assign) = &tr.assign {
        let key = assign.runtime_name();
        let fn_name = reducer_labels.get(&key).cloned().unwrap_or(key);
        effects.push(format!("assign {fn_name}"));
    }
    effects.extend(tr.actions.iter().map(ActionDef::diagram_label));

    if !effects.is_empty() {
        label.push_str(" / ");
        label.push_str(&effects.join(", "));
    }

    label
}

fn render_harel_mermaid(
    ir: &MachineDef,
    guard_labels: &HashMap<String, String>,
    reducer_labels: &HashMap<String, String>,
    delay_labels: &HashMap<String, String>,
) -> String {
    let mut lines = vec![
        "stateDiagram-v2".to_owned(),
        format!("    [*] --> {}", mermaid_safe_id(ir.initial.as_str())),
    ];
    let mut edges = Vec::new();

    for (state_id, node) in &ir.states {
        emit_mermaid_node(
            state_id,
            node,
            1,
            guard_labels,
            reducer_labels,
            delay_labels,
            &mut lines,
            &mut edges,
        );
    }

    if !edges.is_empty() {
        lines.push(String::new());
        lines.extend(edges);
    }

    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
fn emit_mermaid_node(
    state_id: &StateId,
    node: &StateNodeDef,
    depth: usize,
    guard_labels: &HashMap<String, String>,
    reducer_labels: &HashMap<String, String>,
    delay_labels: &HashMap<String, String>,
    lines: &mut Vec<String>,
    edges: &mut Vec<String>,
) {
    let pad = "    ".repeat(depth);
    let mid = mermaid_safe_id(state_id.as_str());

    if let StateKind::History {
        deep,
        default_target,
    } = &node.kind
    {
        let hist_symbol = if *deep { "H*" } else { "H" };
        lines.push(format!("{pad}state \"{hist_symbol}\" as {mid}"));
        if let Some(target) = default_target {
            edges.push(format!(
                "    {mid} --> {} : default",
                mermaid_safe_id(target.as_str())
            ));
        }
        return;
    }

    if mid != state_id.as_str() {
        let escaped = state_id.as_str().replace('"', "#quot;");
        lines.push(format!("{pad}state \"{escaped}\" as {mid}"));
    }

    match &node.kind {
        StateKind::Compound { initial, states } => {
            if mid == state_id.as_str() {
                lines.push(format!("{pad}state {mid} {{"));
            } else {
                lines.pop();
                let escaped = state_id.as_str().replace('"', "#quot;");
                lines.push(format!("{pad}state \"{escaped}\" as {mid} {{"));
            }
            let inner_pad = "    ".repeat(depth + 1);
            lines.push(format!(
                "{inner_pad}[*] --> {}",
                mermaid_safe_id(initial.as_str())
            ));
            for (child_id, child_node) in states {
                emit_mermaid_node(
                    child_id,
                    child_node,
                    depth + 1,
                    guard_labels,
                    reducer_labels,
                    delay_labels,
                    lines,
                    edges,
                );
            }
            lines.push(format!("{pad}}}"));
        }
        StateKind::Parallel { states } => {
            if mid == state_id.as_str() {
                lines.push(format!("{pad}state {mid} {{"));
            } else {
                lines.pop();
                let escaped = state_id.as_str().replace('"', "#quot;");
                lines.push(format!("{pad}state \"{escaped}\" as {mid} {{"));
            }
            let inner_pad = "    ".repeat(depth + 1);
            for (idx, (region_id, region_node)) in states.iter().enumerate() {
                if idx > 0 {
                    lines.push(format!("{inner_pad}--"));
                }
                lines.push(format!(
                    "{inner_pad}[*] --> {}",
                    mermaid_safe_id(region_id.as_str())
                ));
                emit_mermaid_node(
                    region_id,
                    region_node,
                    depth + 1,
                    guard_labels,
                    reducer_labels,
                    delay_labels,
                    lines,
                    edges,
                );
            }
            lines.push(format!("{pad}}}"));
        }
        StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
    }

    if !node.tags.is_empty() {
        lines.push(format!("{pad}{mid} : tags [{}]", node.tags.join(", ")));
    }
    if !node.entry.is_empty() {
        let entry_labels: Vec<String> = node.entry.iter().map(ActionDef::diagram_label).collect();
        lines.push(format!("{pad}{mid} : entry / {}", entry_labels.join(", ")));
    }
    if !node.exit.is_empty() {
        let exit_labels: Vec<String> = node.exit.iter().map(ActionDef::diagram_label).collect();
        lines.push(format!("{pad}{mid} : exit / {}", exit_labels.join(", ")));
    }

    for inv in &node.invoke {
        let inv_name = format_invoke_id_label(inv);
        let inv_display = format_invoke_src_label(inv);
        lines.push(format!("{pad}{mid} : invoke / {inv_display}"));

        for outcome in &inv.on_done {
            emit_outcome_mermaid(
                &mid,
                &pad,
                &format!("done.invoke.{inv_name}"),
                outcome,
                guard_labels,
                reducer_labels,
                lines,
                edges,
            );
        }

        for outcome in &inv.on_error {
            emit_outcome_mermaid(
                &mid,
                &pad,
                &format!("error.invoke.{inv_name}"),
                outcome,
                guard_labels,
                reducer_labels,
                lines,
                edges,
            );
        }
    }

    for outcome in &node.on_done {
        emit_outcome_mermaid(
            &mid,
            &pad,
            "onDone",
            outcome,
            guard_labels,
            reducer_labels,
            lines,
            edges,
        );
    }

    for outcome in &node.always {
        emit_outcome_mermaid(
            &mid,
            &pad,
            "always",
            outcome,
            guard_labels,
            reducer_labels,
            lines,
            edges,
        );
    }

    for after_def in &node.after {
        let delay_display = match &after_def.delay {
            DelayRef::FixedMs(ms) => format!("{ms}ms"),
            DelayRef::Named(name) => name.clone(),
            DelayRef::StarlarkSlot(slot) => {
                let key = DelayRef::starlark_slot_name(*slot);
                delay_labels.get(&key).cloned().unwrap_or(key)
            }
        };
        for outcome in &after_def.outcomes {
            emit_outcome_mermaid(
                &mid,
                &pad,
                &format!("after {delay_display}"),
                outcome,
                guard_labels,
                reducer_labels,
                lines,
                edges,
            );
        }
    }

    for tr in &node.on {
        if tr.forbidden {
            lines.push(format!("{pad}{mid} : forbid {}", tr.event));
            continue;
        }
        let label = format_transition_label(tr, guard_labels, reducer_labels);
        match &tr.target {
            Some(target) => {
                edges.push(format!(
                    "    {mid} --> {} : {label}",
                    mermaid_safe_id(target.as_str())
                ));
            }
            None => {
                lines.push(format!("{pad}{mid} : internal {label}"));
            }
        }
    }

    if matches!(node.kind, StateKind::Final) {
        edges.push(format!("    {mid} --> [*]"));
    }
}

fn format_invoke_id_label(inv: &InvokeDef) -> &str {
    if let Some(id) = &inv.id {
        return id.as_str();
    }
    match inv.src.as_str() {
        BUILTIN_HTTP_SERVICE => "http",
        BUILTIN_EXEC_SERVICE => "exec",
        BUILTIN_ACP_SERVICE => "acp",
        other => other,
    }
}

fn format_invoke_src_label(inv: &InvokeDef) -> String {
    match inv.src.as_str() {
        BUILTIN_HTTP_SERVICE => {
            if let Some(obj) = inv.input.as_object()
                && !obj.contains_key(INVOKE_INPUT_SLOT_KEY)
            {
                let m = obj.get("method").and_then(Value::as_str).unwrap_or("GET");
                let u = obj.get("url").and_then(Value::as_str).unwrap_or("");
                return format!("http {m} {u}").trim().to_owned();
            }
            "http".to_owned()
        }
        BUILTIN_EXEC_SERVICE => {
            if let Some(cmd) = inv
                .input
                .get("cmd")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                return format!("exec {cmd}");
            }
            "exec".to_owned()
        }
        BUILTIN_ACP_SERVICE => {
            if let Some(agent) = inv.input.get("agent").and_then(Value::as_object) {
                let id = agent.get("id").and_then(Value::as_str).unwrap_or("agent");
                let cmd = agent.get("command").and_then(Value::as_str).unwrap_or("");
                return format!("acp {id} ({cmd})").trim().to_owned();
            }
            "acp".to_owned()
        }
        other => other.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_outcome_mermaid(
    mid: &str,
    pad: &str,
    trigger_label: &str,
    outcome: &OutcomeDef,
    guard_labels: &HashMap<String, String>,
    reducer_labels: &HashMap<String, String>,
    lines: &mut Vec<String>,
    edges: &mut Vec<String>,
) {
    let synthetic = TransitionDef {
        event: trigger_label.to_owned(),
        target: outcome.target.clone(),
        guard: outcome.guard.clone(),
        actions: outcome.actions.clone(),
        assign: outcome.assign.clone(),
        reenter: outcome.reenter,
        forbidden: false,
        description: outcome.description.clone(),
    };
    let label = format_transition_label(&synthetic, guard_labels, reducer_labels);
    match &outcome.target {
        Some(target) => edges.push(format!(
            "    {mid} --> {} : {label}",
            mermaid_safe_id(target.as_str())
        )),
        None => lines.push(format!("{pad}{mid} : internal {label}")),
    }
}

fn add_root_child_state(
    builder: MachineBuilder,
    state_id: &StateId,
    node: &StateNodeDef,
) -> MachineBuilder {
    match &node.kind {
        StateKind::Final => builder.final_state_with(state_id.as_str(), |sb| {
            configure_state_node(sb, state_id, node)
        }),
        StateKind::Parallel { .. } => builder.parallel_state(state_id.as_str(), |sb| {
            configure_state_node(sb, state_id, node)
        }),
        StateKind::History {
            deep,
            default_target,
        } => {
            let mut h = if *deep {
                BuilderHistory::deep()
            } else {
                BuilderHistory::shallow()
            };
            if let Some(target) = default_target {
                h = h.target(format!("#{}", target.as_str()));
            }
            builder.history(state_id.as_str(), h)
        }
        StateKind::Atomic | StateKind::Compound { .. } => {
            builder.state(state_id.as_str(), |sb| configure_state_node(sb, state_id, node))
        }
    }
}

fn add_nested_child_state(
    builder: StateBuilder,
    state_id: &StateId,
    node: &StateNodeDef,
) -> StateBuilder {
    match &node.kind {
        StateKind::Final => builder.final_state_with(state_id.as_str(), |sb| {
            configure_state_node(sb, state_id, node)
        }),
        StateKind::Parallel { .. } => builder.parallel_state(state_id.as_str(), |sb| {
            configure_state_node(sb, state_id, node)
        }),
        StateKind::History {
            deep,
            default_target,
        } => {
            let mut h = if *deep {
                BuilderHistory::deep()
            } else {
                BuilderHistory::shallow()
            };
            if let Some(target) = default_target {
                h = h.target(format!("#{}", target.as_str()));
            }
            builder.history(state_id.as_str(), h)
        }
        StateKind::Atomic | StateKind::Compound { .. } => {
            builder.state(state_id.as_str(), |sb| configure_state_node(sb, state_id, node))
        }
    }
}

fn configure_state_node(
    mut sb: StateBuilder,
    state_id: &StateId,
    node: &StateNodeDef,
) -> StateBuilder {
    sb = sb.id(state_id.as_str());

    if !node.tags.is_empty() {
        sb = sb.tags(node.tags.iter().cloned());
    }
    if let Some(meta) = &node.meta {
        sb = sb.meta(meta.clone());
    }
    if let Some(desc) = &node.description {
        sb = sb.description(desc);
    }
    if let Some(out) = &node.output {
        sb = sb.output(out.clone());
    }

    for entry_action in &node.entry {
        sb = sb.entry(lower_action_to_builder(entry_action));
    }
    for exit_action in &node.exit {
        sb = sb.exit(lower_action_to_builder(exit_action));
    }

    match &node.kind {
        StateKind::Compound { initial, states } => {
            sb = sb.initial(initial.as_str());
            for (child_id, child_node) in states {
                sb = add_nested_child_state(sb, child_id, child_node);
            }
        }
        StateKind::Parallel { states } => {
            for (child_id, child_node) in states {
                sb = add_nested_child_state(sb, child_id, child_node);
            }
        }
        StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
    }

    let mut grouped: IndexMap<&str, Vec<BuilderTransition>> = IndexMap::new();
    for tr in &node.on {
        grouped
            .entry(tr.event.as_str())
            .or_default()
            .push(lower_transition_to_builder(tr));
    }
    for (event_name, branches) in grouped {
        sb = sb.on(event_name, branches);
    }

    if !node.on_done.is_empty() {
        sb = sb.on_done(
            node.on_done
                .iter()
                .map(lower_outcome_to_builder)
                .collect::<Vec<_>>(),
        );
    }

    if !node.always.is_empty() {
        sb = sb.always(
            node.always
                .iter()
                .map(lower_outcome_to_builder)
                .collect::<Vec<_>>(),
        );
    }

    let mut after_grouped: IndexMap<String, (DelayKey, Vec<BuilderTransition>)> = IndexMap::new();
    for after_def in &node.after {
        let dk = lower_delay_ref_to_key(&after_def.delay);
        let key_str = dk.key();
        let entry = after_grouped
            .entry(key_str)
            .or_insert_with(|| (dk, Vec::new()));
        entry
            .1
            .extend(after_def.outcomes.iter().map(lower_outcome_to_builder));
    }
    for (_k, (delay_key, branches)) in after_grouped {
        sb = sb.after(delay_key, branches);
    }

    for inv in &node.invoke {
        sb = sb.invoke(lower_invoke_to_builder(inv, state_id));
    }

    sb
}

fn lower_delay_ref_to_key(delay: &DelayRef) -> DelayKey {
    match delay {
        DelayRef::FixedMs(ms) => DelayKey::Fixed(*ms),
        DelayRef::Named(name) => DelayKey::Named(name.clone()),
        DelayRef::StarlarkSlot(slot) => DelayKey::Named(DelayRef::starlark_slot_name(*slot)),
    }
}

fn build_event_object(event_type: &str, payload: &Value) -> Value {
    let mut map = match payload {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("payload".to_owned(), other.clone());
            m
        }
    };
    map.insert("type".to_owned(), Value::String(event_type.to_owned()));
    Value::Object(map)
}

fn lower_action_to_builder(action: &ActionDef) -> BuilderAction {
    match action {
        ActionDef::Named(name) => BuilderAction::named(name),
        ActionDef::Raise {
            event,
            payload,
            delay,
            id,
        } => {
            let mut ba = BuilderAction::raise_event(build_event_object(event, payload));
            if let Some(d) = delay {
                ba = ba.delay(lower_delay_ref_to_key(d));
            }
            if let Some(send_id) = id {
                ba = ba.id(send_id);
            }
            ba
        }
        ActionDef::Cancel { send_id } => BuilderAction::cancel(send_id),
        ActionDef::Emit { event, payload } => {
            BuilderAction::named("emit").param("event", build_event_object(event, payload))
        }
        ActionDef::SendTo {
            to,
            event,
            payload,
            delay,
            id,
        } => {
            let mut ba = BuilderAction::named("sendTo")
                .param("to", to.as_str())
                .param("event", build_event_object(event, payload));
            if let Some(d) = delay {
                ba = ba.delay(lower_delay_ref_to_key(d));
            }
            if let Some(send_id) = id {
                ba = ba.id(send_id);
            }
            ba
        }
        ActionDef::SendParent { event, payload } => {
            BuilderAction::named("sendParent").param("event", build_event_object(event, payload))
        }
        ActionDef::Log { expr } => BuilderAction::log(expr.clone()),
    }
}

fn lower_invoke_to_builder(inv: &InvokeDef, state_id: &StateId) -> BuilderInvoke {
    let mut bi = BuilderInvoke::new(&inv.src);
    if let Some(id) = &inv.id {
        bi = bi.id(id);
    }
    // Wrap the input in a metadata envelope so service wrappers know which invoke
    // (and owning state) they run for; `build_machine_logic` unwraps it before the
    // service sees its input.
    let trace_id = inv.id.clone().unwrap_or_else(|| state_id.as_str().to_owned());
    bi = bi.input(serde_json::json!({
        (trace::INVOKE_ENVELOPE_KEY): { "id": trace_id, "state": state_id.as_str() },
        "input": inv.input,
    }));
    if !inv.on_done.is_empty() {
        bi = bi.on_done(
            inv.on_done
                .iter()
                .map(lower_outcome_to_builder)
                .collect::<Vec<_>>(),
        );
    }
    if !inv.on_error.is_empty() {
        bi = bi.on_error(
            inv.on_error
                .iter()
                .map(lower_outcome_to_builder)
                .collect::<Vec<_>>(),
        );
    }
    bi
}

fn lower_outcome_to_builder(outcome: &OutcomeDef) -> BuilderTransition {
    let mut bt = match &outcome.target {
        Some(target) => BuilderTransition::to(format!("#{}", target.as_str())),
        None => BuilderTransition::targetless(),
    };
    if outcome.reenter {
        bt = bt.reenter();
    }
    if let Some(desc) = &outcome.description {
        bt = bt.description(desc);
    }
    if let Some(guard_ref) = &outcome.guard {
        bt = bt.guard(lower_guard_to_builder(guard_ref));
    }
    if let Some(assign_ref) = &outcome.assign {
        bt = bt.action(BuilderAction::named(assign_ref.runtime_name()));
    }
    for action in &outcome.actions {
        bt = bt.action(lower_action_to_builder(action));
    }
    bt
}

fn lower_transition_to_builder(tr: &TransitionDef) -> BuilderTransition {
    if tr.forbidden {
        let mut fb = BuilderTransition::forbidden();
        if let Some(desc) = &tr.description {
            fb = fb.description(desc);
        }
        return fb;
    }

    let mut bt = match &tr.target {
        Some(target) => BuilderTransition::to(format!("#{}", target.as_str())),
        None => BuilderTransition::targetless(),
    };
    if tr.reenter {
        bt = bt.reenter();
    }
    if let Some(desc) = &tr.description {
        bt = bt.description(desc);
    }
    if let Some(guard_ref) = &tr.guard {
        bt = bt.guard(lower_guard_to_builder(guard_ref));
    }
    if let Some(assign_ref) = &tr.assign {
        bt = bt.action(BuilderAction::named(assign_ref.runtime_name()));
    }
    for action in &tr.actions {
        bt = bt.action(lower_action_to_builder(action));
    }
    bt
}

fn lower_guard_to_builder(guard: &GuardRef) -> BuilderGuard {
    match guard {
        GuardRef::Named(name) => BuilderGuard::named(name),
        GuardRef::StarlarkSlot(slot) => BuilderGuard::named(GuardRef::starlark_slot_name(*slot)),
        GuardRef::StateIn(state_id) => BuilderGuard::state_in(format!("#{}", state_id.as_str())),
        GuardRef::AllOf(items) => {
            BuilderGuard::and(items.iter().map(lower_guard_to_builder).collect::<Vec<_>>())
        }
        GuardRef::AnyOf(items) => {
            BuilderGuard::or(items.iter().map(lower_guard_to_builder).collect::<Vec<_>>())
        }
        GuardRef::Not(inner) => BuilderGuard::not(lower_guard_to_builder(inner)),
    }
}

/// Builds a `MachineLogic<DynContext>` merging frozen Starlark guards, delays & reducers with Rust `HostCallbacks`.
pub fn build_machine_logic(
    chart: &CompiledChart,
    callbacks: &HostCallbacks,
) -> MachineLogic<DynContext> {
    build_machine_logic_traced(chart, callbacks, None)
}

/// Like [`build_machine_logic`], additionally emitting `service_*` records into `trace`.
pub fn build_machine_logic_traced(
    chart: &CompiledChart,
    callbacks: &HostCallbacks,
    trace: Option<Arc<TraceHub>>,
) -> MachineLogic<DynContext> {
    let mut logic = MachineLogic::dynamic();
    let record_ctor = chart.context_record_type.clone();
    let type_registry = Arc::new(chart.type_registry.clone());

    for (guard_name, frozen_guard) in &chart.guards {
        let guard_fn = frozen_guard.clone();
        let rec_ctor = record_ctor.clone();
        let reg = Arc::clone(&type_registry);
        let gname = guard_name.clone();
        logic = logic.guard(guard_name.clone(), move |ctx, event, _params| {
            eval_starlark_guard(&gname, &guard_fn, rec_ctor.as_ref(), &reg, ctx, event)
        });
    }

    for (reducer_name, frozen_reducer) in &chart.reducers {
        let reducer_fn = frozen_reducer.clone();
        let rec_ctor = record_ctor.clone();
        let reg = Arc::clone(&type_registry);
        let rname = reducer_name.clone();
        logic = logic.action(
            reducer_name.clone(),
            move |scope: &mut ActionScope<'_, DynContext>| {
                let next_ctx = eval_starlark_reducer(
                    &rname,
                    &reducer_fn,
                    rec_ctor.as_ref(),
                    &reg,
                    scope.context,
                    scope.event,
                )?;
                *scope.context = next_ctx;
                Ok(())
            },
        );
    }

    for (guard_name, handler) in &callbacks.guards {
        let handler = Arc::clone(handler);
        let gname = guard_name.clone();
        logic = logic.guard(guard_name.clone(), move |ctx, event, _params| {
            let ce = ChartEvent::from_xstate(event);
            handler(ctx, &ce).map_err(|e| XStateError::Logic(format!("guard `{gname}`: {e:#}")))
        });
    }

    let mut registered_actions: HashSet<String> = HashSet::new();
    for (action_name, handler) in &callbacks.actions {
        registered_actions.insert(action_name.clone());
        let handler = Arc::clone(handler);
        let aname = action_name.clone();
        logic = logic.action(
            action_name.clone(),
            move |scope: &mut ActionScope<'_, DynContext>| {
                let ce = ChartEvent::from_xstate(scope.event);
                handler(scope.context, &ce)
                    .map_err(|e| XStateError::Logic(format!("action `{aname}`: {e:#}")))
            },
        );
    }

    if !callbacks.strict_actions {
        let mut declared_actions = HashSet::new();
        collect_declared_named_actions(&chart.ir.states, &mut declared_actions);
        for action_name in declared_actions {
            if !registered_actions.contains(&action_name) {
                logic = logic.action(action_name, |_scope| Ok(()));
            }
        }
    }

    for (delay_name, frozen_delay) in &chart.delays {
        let delay_fn = frozen_delay.clone();
        let rec_ctor = record_ctor.clone();
        let reg = Arc::clone(&type_registry);
        let dname = delay_name.clone();
        logic = logic.delay_fn(delay_name.clone(), move |ctx, event| {
            eval_starlark_delay(&dname, &delay_fn, rec_ctor.as_ref(), &reg, ctx, event)
        });
    }

    for (delay_name, ms) in &callbacks.fixed_delays {
        logic = logic.delay(delay_name.clone(), *ms);
    }

    for (delay_name, handler) in &callbacks.dynamic_delays {
        let handler = Arc::clone(handler);
        let dname = delay_name.clone();
        logic = logic.delay_fn(delay_name.clone(), move |ctx, event| {
            let ce = ChartEvent::from_xstate(event);
            handler(ctx, &ce).map_err(|e| XStateError::Logic(format!("delay `{dname}`: {e:#}")))
        });
    }

    let input_slots = Arc::new(chart.inputs.clone());
    let tool_slots = Arc::new(chart.tools.clone());

    for (service_name, service_fn) in &callbacks.sync_services {
        let service_fn = Arc::clone(service_fn);
        let sname = service_name.clone();
        let input_slots = Arc::clone(&input_slots);
        let type_registry = Arc::clone(&type_registry);
        let rec_ctor = record_ctor.clone();
        let hub = trace.clone();
        logic = logic.service(service_name.clone(), move |ctx, raw_input| {
            let (meta, user_input) = trace::split_invoke_envelope(raw_input, &sname);
            let scope = hub
                .as_ref()
                .zip(meta)
                .map(|(hub, meta)| trace::ServiceScope { hub: Arc::clone(hub), meta });
            let started = std::time::Instant::now();
            let run = || -> Result<Value, XStateError> {
                let resolved_input = resolve_dynamic_service_input(
                    &sname,
                    &user_input,
                    &input_slots,
                    rec_ctor.as_ref(),
                    &type_registry,
                    ctx,
                )?;
                if let Some(scope) = &scope {
                    scope.started(&resolved_input);
                }
                service_fn(ctx, &resolved_input)
                    .map_err(|e| XStateError::Logic(format!("service `{sname}`: {e:#}")))
            };
            match &scope {
                Some(s) => {
                    let result = trace::with_sync_scope(s.clone(), run);
                    s.finished(result.as_ref().map_err(ToString::to_string), started);
                    result
                }
                None => run(),
            }
        });
    }

    for (service_name, service_fn) in &callbacks.async_services {
        let service_fn = Arc::clone(service_fn);
        let sname = service_name.clone();
        let input_slots = Arc::clone(&input_slots);
        let tool_slots = Arc::clone(&tool_slots);
        let type_registry = Arc::clone(&type_registry);
        let rec_ctor = record_ctor.clone();
        let hub = trace.clone();
        logic = logic.async_service(service_name.clone(), move |ctx, raw_input| {
            let (meta, user_input) = trace::split_invoke_envelope(&raw_input, &sname);
            let resolved = resolve_dynamic_service_input(
                &sname,
                &user_input,
                &input_slots,
                rec_ctor.as_ref(),
                &type_registry,
                &ctx,
            );
            let scope = hub
                .as_ref()
                .zip(meta)
                .map(|(hub, meta)| trace::ServiceScope { hub: Arc::clone(hub), meta });
            let tool_scope = (!tool_slots.is_empty()).then(|| builtins::mcp::ToolRuntimeScope {
                tools: Arc::clone(&tool_slots),
                type_registry: Arc::clone(&type_registry),
                context_record_type: rec_ctor.clone(),
                ctx_snapshot: ctx.clone(),
            });
            let service_fn = Arc::clone(&service_fn);
            let sname = sname.clone();
            Box::pin(async move {
                let resolved_input = resolved?;
                let started = std::time::Instant::now();
                let inner_fut = service_fn(ctx, resolved_input.clone());
                let scoped_fut = async move {
                    match tool_scope {
                        Some(ts) => builtins::mcp::with_tool_runtime_scope(ts, inner_fut).await,
                        None => inner_fut.await,
                    }
                };
                let result = match &scope {
                    Some(scope) => {
                        scope.started(&resolved_input);
                        trace::with_async_scope(scope.clone(), scoped_fut).await
                    }
                    None => scoped_fut.await,
                }
                .map_err(|e| XStateError::Logic(format!("service `{sname}`: {e:#}")));
                if let Some(scope) = &scope {
                    scope.finished(result.as_ref().map_err(ToString::to_string), started);
                }
                result
            })
        });
    }

    for (service_name, child_machine) in &callbacks.child_machines {
        logic = logic.machine_service(service_name.clone(), child_machine.clone());
    }

    logic
}

fn push_named_actions(actions: &[ActionDef], out: &mut HashSet<String>) {
    for act in actions {
        if let ActionDef::Named(name) = act {
            out.insert(name.clone());
        }
    }
}

fn collect_declared_named_actions(
    states: &IndexMap<StateId, StateNodeDef>,
    out: &mut HashSet<String>,
) {
    for node in states.values() {
        push_named_actions(&node.entry, out);
        push_named_actions(&node.exit, out);
        for tr in &node.on {
            push_named_actions(&tr.actions, out);
        }
        for outcome in node.always.iter().chain(node.on_done.iter()) {
            push_named_actions(&outcome.actions, out);
        }
        for after_def in &node.after {
            for outcome in &after_def.outcomes {
                push_named_actions(&outcome.actions, out);
            }
        }
        for inv in &node.invoke {
            for outcome in inv.on_done.iter().chain(inv.on_error.iter()) {
                push_named_actions(&outcome.actions, out);
            }
        }
        match &node.kind {
            StateKind::Compound {
                states: children, ..
            }
            | StateKind::Parallel { states: children } => {
                collect_declared_named_actions(children, out);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

fn eval_starlark_guard(
    guard_name: &str,
    guard_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: &starchart_starlark::TypeRegistry,
    ctx: &Value,
    event: &Event,
) -> Result<bool, XStateError> {
    starchart_starlark::with_active_type_registry(type_registry, || {
        Module::with_temp_heap(|module| {
            let ctx_val = alloc_context_on_heap(&module, record_ctor, Some(type_registry), ctx)
                .map_err(|e| XStateError::Logic(format!("guard `{guard_name}`: {e}")))?;
            let event_json = event_to_starlark_json(event);
            let event_val = json_to_starlark_value(&event_json, module.heap());

            // SAFETY: `module.frozen_heap()` retains a reference to `guard_fn.owner()` for `'v`.
            let callable_val =
                unsafe { guard_fn.owned_frozen_value(module.frozen_heap()) }.to_value();

            let mut eval = Evaluator::new(&module);
            let result = eval
                .eval_function(callable_val, &[ctx_val, event_val], &[])
                .map_err(|e| XStateError::Logic(format!("guard `{guard_name}`: {e:#}")))?;

            result.unpack_bool().ok_or_else(|| {
                XStateError::Logic(format!(
                    "guard `{guard_name}` returned `{}`, expected `bool`",
                    result.get_type()
                ))
            })
        })
    })
}

fn eval_starlark_reducer(
    reducer_name: &str,
    reducer_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: &starchart_starlark::TypeRegistry,
    ctx: &Value,
    event: &Event,
) -> Result<Value, XStateError> {
    starchart_starlark::with_active_type_registry(type_registry, || {
        Module::with_temp_heap(|module| {
            let ctx_val = alloc_context_on_heap(&module, record_ctor, Some(type_registry), ctx)
                .map_err(|e| XStateError::Logic(format!("reducer `{reducer_name}`: {e}")))?;
            let event_json = event_to_starlark_json(event);
            let event_val = json_to_starlark_value(&event_json, module.heap());

            // SAFETY: `module.frozen_heap()` retains a reference to `reducer_fn.owner()` for `'v`.
            let callable_val =
                unsafe { reducer_fn.owned_frozen_value(module.frozen_heap()) }.to_value();

            let mut eval = Evaluator::new(&module);
            let result = eval
                .eval_function(callable_val, &[ctx_val, event_val], &[])
                .map_err(|e| XStateError::Logic(format!("reducer `{reducer_name}`: {e:#}")))?;

            let next_json = result
                .to_json_value()
                .map_err(|e| XStateError::Logic(format!("reducer `{reducer_name}`: {e:#}")))?;

            if !next_json.is_object() {
                return Err(XStateError::Logic(format!(
                    "assign reducer `{reducer_name}` must return a dict or record, got `{}`",
                    result.get_type()
                )));
            }

            Ok(next_json)
        })
    })
}

fn eval_starlark_delay(
    delay_name: &str,
    delay_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: &starchart_starlark::TypeRegistry,
    ctx: &Value,
    event: &Event,
) -> Result<f64, XStateError> {
    starchart_starlark::with_active_type_registry(type_registry, || {
        Module::with_temp_heap(|module| {
            let ctx_val = alloc_context_on_heap(&module, record_ctor, Some(type_registry), ctx)
                .map_err(|e| XStateError::Logic(format!("delay `{delay_name}`: {e}")))?;
            let event_json = event_to_starlark_json(event);
            let event_val = json_to_starlark_value(&event_json, module.heap());

            // SAFETY: `module.frozen_heap()` retains a reference to `delay_fn.owner()` for `'v`.
            let callable_val =
                unsafe { delay_fn.owned_frozen_value(module.frozen_heap()) }.to_value();

            let mut eval = Evaluator::new(&module);
            let result = eval
                .eval_function(callable_val, &[ctx_val, event_val], &[])
                .map_err(|e| XStateError::Logic(format!("delay `{delay_name}`: {e:#}")))?;

            let json_num = result
                .to_json_value()
                .map_err(|e| XStateError::Logic(format!("delay `{delay_name}`: {e:#}")))?;
            let ms = json_num.as_f64().ok_or_else(|| {
                XStateError::Logic(format!(
                    "delay `{delay_name}` returned `{}`, expected non-negative int/float ms",
                    result.get_type()
                ))
            })?;
            if !ms.is_finite() || ms < 0.0 {
                return Err(XStateError::Logic(format!(
                    "delay `{delay_name}` must return finite ms >= 0, got {ms}"
                )));
            }
            Ok(ms)
        })
    })
}

fn resolve_dynamic_service_input(
    service_name: &str,
    raw_input: &Value,
    input_slots: &HashMap<String, OwnedFrozenValue>,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: &starchart_starlark::TypeRegistry,
    ctx: &Value,
) -> Result<Value, XStateError> {
    match raw_input {
        Value::Object(map) => {
            if map.len() == 1
                && let Some(slot_name) = map.get(INVOKE_INPUT_SLOT_KEY).and_then(Value::as_str)
            {
                let slot_fn = input_slots.get(slot_name).ok_or_else(|| {
                    XStateError::Logic(format!(
                        "service `{service_name}` missing input slot `{slot_name}`"
                    ))
                })?;
                return eval_starlark_input_fn(
                    service_name,
                    slot_fn,
                    record_ctor,
                    type_registry,
                    ctx,
                );
            }
            let mut resolved_map = Map::with_capacity(map.len());
            for (k, v) in map {
                resolved_map.insert(
                    k.clone(),
                    resolve_dynamic_service_input(
                        service_name,
                        v,
                        input_slots,
                        record_ctor,
                        type_registry,
                        ctx,
                    )?,
                );
            }
            Ok(Value::Object(resolved_map))
        }
        Value::Array(items) => {
            let mut resolved_arr = Vec::with_capacity(items.len());
            for item in items {
                resolved_arr.push(resolve_dynamic_service_input(
                    service_name,
                    item,
                    input_slots,
                    record_ctor,
                    type_registry,
                    ctx,
                )?);
            }
            Ok(Value::Array(resolved_arr))
        }
        other => Ok(other.clone()),
    }
}

fn eval_starlark_input_fn(
    service_name: &str,
    input_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: &starchart_starlark::TypeRegistry,
    ctx: &Value,
) -> Result<Value, XStateError> {
    starchart_starlark::with_active_type_registry(type_registry, || {
        Module::with_temp_heap(|module| {
            let ctx_val = alloc_context_on_heap(&module, record_ctor, Some(type_registry), ctx)
                .map_err(|e| XStateError::Logic(format!("service `{service_name}` input: {e}")))?;

            // SAFETY: `module.frozen_heap()` retains a reference to `input_fn.owner()` for `'v`.
            let callable_val =
                unsafe { input_fn.owned_frozen_value(module.frozen_heap()) }.to_value();

            let mut eval = Evaluator::new(&module);
            let result = eval
                .eval_function(callable_val, &[ctx_val], &[])
                .map_err(|e| XStateError::Logic(format!("service `{service_name}` input: {e:#}")))?;

            result
                .to_json_value()
                .map_err(|e| XStateError::Logic(format!("service `{service_name}` input: {e:#}")))
        })
    })
}

pub(crate) fn alloc_context_on_heap<'v>(
    module: &Module<'v>,
    record_ctor: Option<&OwnedFrozenValue>,
    type_registry: Option<&starchart_starlark::TypeRegistry>,
    ctx: &Value,
) -> Result<StarlarkVal<'v>, String> {
    if let (Some(ctor), Value::Object(map)) = (record_ctor, ctx) {
        if let Some(reg) = type_registry
            && let Ok(shape) = starchart_starlark::parse_type_expr(&ctor.value().to_string())
        {
            return starchart_starlark::instantiate_record_with_registry(
                module, ctor, &shape, reg, ctx,
            )
            .map_err(|e| format!("{e:#}"));
        }
        // SAFETY: `module.frozen_heap()` keeps `ctor`'s owner heap alive for `'v`.
        let ctor_val = unsafe { ctor.owned_frozen_value(module.frozen_heap()) }.to_value();
        let mut named: Vec<(&str, StarlarkVal<'v>)> = Vec::with_capacity(map.len());
        for (k, v) in map {
            named.push((k.as_str(), json_to_starlark_value(v, module.heap())));
        }
        let mut eval = Evaluator::new(module);
        return eval
            .eval_function(ctor_val, &[], &named)
            .map_err(|e| format!("{e:#}"));
    }
    Ok(json_to_starlark_value(ctx, module.heap()))
}

fn event_to_starlark_json(event: &Event) -> Value {
    let mut obj = match &event.payload {
        Value::Object(map) => map.clone(),
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("payload".to_owned(), other.clone());
            m
        }
    };
    obj.entry("type".to_owned())
        .or_insert_with(|| Value::String(event.ty.to_string()));
    Value::Object(obj)
}

fn json_to_starlark_value<'v>(json: &Value, heap: Heap<'v>) -> StarlarkVal<'v> {
    match json {
        Value::Null => StarlarkVal::new_none(),
        Value::Bool(b) => StarlarkVal::new_bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i)
            } else if let Some(u) = n.as_u64() {
                heap.alloc(u)
            } else {
                heap.alloc(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => heap.alloc_str(s).to_value(),
        Value::Array(items) => {
            heap.alloc(AllocList(items.iter().map(|v| json_to_starlark_value(v, heap))))
        }
        Value::Object(map) => heap.alloc(AllocDict(
            map.iter()
                .map(|(k, v)| (k.as_str(), json_to_starlark_value(v, heap))),
        )),
    }
}

fn state_matches_query(machine_id: &str, active_ids: &[String], query: &str) -> bool {
    let prefixed = format!("{machine_id}.{query}");
    active_ids.iter().any(|active| {
        active == query
            || active == &prefixed
            || active.starts_with(&format!("{prefixed}."))
            || active.ends_with(&format!(".{query}"))
    })
}

/// Readable labels for internal action names (Starlark reducer slots -> `assign <fn>`).
fn trace_action_labels(chart: &CompiledChart) -> HashMap<String, String> {
    chart
        .reducers
        .iter()
        .map(|(slot, val)| {
            (
                slot.clone(),
                format!("assign {}", extract_starlark_fn_name(val, slot)),
            )
        })
        .collect()
}

fn new_trace_hub(chart: &CompiledChart) -> Arc<TraceHub> {
    TraceHub::with_final_outputs(
        chart.ir.id.clone(),
        trace_action_labels(chart),
        collect_top_level_final_outputs(&chart.ir),
        trace::DEFAULT_HISTORY_CAPACITY,
    )
}

fn collect_top_level_final_outputs(ir: &MachineDef) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    for (sid, node) in &ir.states {
        if matches!(node.kind, StateKind::Final)
            && let Some(out) = &node.output
        {
            map.insert(sid.as_str().to_owned(), out.clone());
        }
    }
    map
}

/// Synchronous statechart runner backed by `xstate_statemachine::SyncInterpreter`.
pub struct StarchartSyncRunner {
    machine_id: String,
    final_outputs: HashMap<String, Value>,
    sim_clock: Option<SimulatedClock>,
    trace: Arc<TraceHub>,
    inner: SyncInterpreter<DynContext>,
}

/// Alias preserved for compatibility.
pub type ChartSyncRunner = StarchartSyncRunner;

impl StarchartSyncRunner {
    /// Compiles and starts a synchronous statechart interpreter with a deterministic simulated clock.
    pub fn start(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let clock = SimulatedClock::new();
        Self::start_with_clock(chart, callbacks, Arc::new(clock.clone()), Some(clock))
    }

    /// Compiles and starts a synchronous statechart interpreter backed by wall-clock [`RealClock`].
    pub fn start_real_clock(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        Self::start_with_clock(chart, callbacks, Arc::new(RealClock::new()), None)
    }

    fn start_with_clock(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
        clock: Arc<dyn xstate_statemachine::core::Clock>,
        sim_clock: Option<SimulatedClock>,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let hub = new_trace_hub(chart);
        let logic = build_machine_logic_traced(chart, &callbacks, Some(Arc::clone(&hub)));
        let mut inner = SyncInterpreter::with_clock(machine, logic, clock);
        inner.observe(trace::TraceObserver::new(Arc::clone(&hub)));
        inner.start()?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            final_outputs: collect_top_level_final_outputs(&chart.ir),
            sim_clock,
            trace: hub,
            inner,
        })
    }

    /// Subscribes to this run's lossless trace (history replayed first, then live records).
    pub fn subscribe_traces(&self) -> TraceReceiver {
        self.trace.subscribe()
    }

    /// Returns this run's [`TraceHub`] (e.g. to [`TraceHub::close`] it when done).
    pub fn trace_hub(&self) -> Arc<TraceHub> {
        Arc::clone(&self.trace)
    }

    /// Advances simulated time by `ms` milliseconds and fires any due `after(...)` or delayed `raise` timers.
    pub fn advance_ms(&mut self, ms: f64) -> Result<(), ChartRuntimeError> {
        match &self.sim_clock {
            Some(clock) => {
                self.inner.advance(clock, ms)?;
                Ok(())
            }
            None => {
                self.inner.tick()?;
                Ok(())
            }
        }
    }

    /// Sends an event without payload.
    pub fn send(&mut self, event_type: &str) -> Result<(), ChartRuntimeError> {
        self.inner.send(Event::new(event_type))?;
        Ok(())
    }

    /// Sends an event with a JSON payload object.
    pub fn send_with_payload(
        &mut self,
        event_type: &str,
        payload: Value,
    ) -> Result<(), ChartRuntimeError> {
        self.inner.send(Event::with_payload(event_type, payload))?;
        Ok(())
    }

    /// Returns the active leaf state IDs.
    pub fn state_ids(&self) -> Vec<String> {
        self.inner.state_ids()
    }

    /// Returns active relative state paths stripped of `<machine_id>.` prefix.
    pub fn active_states(&self) -> Vec<String> {
        let prefix = format!("{}.", self.machine_id);
        self.inner
            .state_ids()
            .into_iter()
            .map(|s| s.strip_prefix(&prefix).unwrap_or(&s).to_owned())
            .collect()
    }

    /// Checks whether `state` is currently active.
    pub fn is_in(&self, state: &str) -> bool {
        if self.inner.matches(state) || self.inner.matches(&format!("{}.{state}", self.machine_id))
        {
            return true;
        }
        state_matches_query(&self.machine_id, &self.inner.state_ids(), state)
    }

    /// Returns the union of `tags` across all currently active states (including active ancestors).
    pub fn active_tags(&self) -> Vec<String> {
        let Some(state) = self.inner.state() else {
            return Vec::new();
        };
        let machine = self.inner.machine();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for sid in state.configuration() {
            for tag in &machine.node(sid).tags {
                if seen.insert(tag.as_str()) {
                    out.push(tag.to_string());
                }
            }
        }
        out
    }

    /// Returns `true` if any currently active state carries `tag`.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.active_tags().iter().any(|t| t == tag)
    }

    /// Returns the machine's final `output` (`Value::Null` until `Done` or when unset).
    pub fn output(&self) -> &Value {
        if let Some(active) = self.inner.state() {
            let root_out = active.output();
            if !root_out.is_null() {
                return root_out;
            }
            if self.is_done() {
                for st in self.active_states() {
                    if let Some(leaf_out) = self.final_outputs.get(&st) {
                        return leaf_out;
                    }
                }
            }
        }
        &Value::Null
    }

    /// Drains events emitted by `emit_event(...)` actions.
    pub fn take_emitted(&mut self) -> Vec<Value> {
        self.inner.take_emitted()
    }

    /// Drains values written by `log_action(...)` actions.
    pub fn take_logs(&mut self) -> Vec<(Option<String>, Value)> {
        self.inner.take_logs()
    }

    /// Returns the current extended state context.
    pub fn context(&self) -> &Value {
        self.inner.context().expect("started runner always has context")
    }

    /// Returns `true` once the root machine has reached a top-level `final_state`.
    pub fn is_done(&self) -> bool {
        self.inner.status() == Status::Done
    }
}

/// Asynchronous Tokio actor runner backed by `xstate_statemachine::tokio::spawn`.
#[derive(Clone)]
pub struct StarchartActor {
    machine_id: String,
    final_outputs: Arc<HashMap<String, Value>>,
    trace: Arc<TraceHub>,
    inner: AsyncInterpreter<DynContext>,
}

/// Alias preserved for compatibility.
pub type ChartActor = StarchartActor;

impl StarchartActor {
    /// Spawns the compiled statechart onto the current Tokio runtime.
    pub fn spawn(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let hub = new_trace_hub(chart);
        let logic = build_machine_logic_traced(chart, &callbacks, Some(Arc::clone(&hub)));
        let observer: Arc<dyn xstate_statemachine::core::Observer<DynContext>> =
            trace::TraceObserver::new(Arc::clone(&hub));
        let inner = spawn_with_observers(machine, logic, ActorOptions::default(), vec![observer])?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            final_outputs: Arc::new(collect_top_level_final_outputs(&chart.ir)),
            trace: hub,
            inner,
        })
    }

    /// Subscribes to this run's lossless trace (history replayed first, then live records).
    /// The receiver can be moved to any task or thread.
    pub fn subscribe_traces(&self) -> TraceReceiver {
        self.trace.subscribe()
    }

    /// Returns this run's [`TraceHub`] (e.g. to [`TraceHub::close`] it when done).
    pub fn trace_hub(&self) -> Arc<TraceHub> {
        Arc::clone(&self.trace)
    }

    /// Spawns the compiled statechart with all built-in `@starchart/*` runtime drivers enabled.
    pub fn spawn_with_builtins(chart: &CompiledChart) -> Result<Self, ChartRuntimeError> {
        Self::spawn(chart, HostCallbacks::with_builtins())
    }

    /// Subscribes to live state/status updates from the actor.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<xstate_statemachine::tokio::View> {
        self.inner.subscribe()
    }

    /// Sends an event and waits until the macrostep completes.
    pub async fn send(&self, event_type: &str) -> Result<(), ChartRuntimeError> {
        self.inner.send_and_wait(Event::new(event_type)).await?;
        Ok(())
    }

    /// Sends an event with a JSON payload and waits until the macrostep completes.
    pub async fn send_with_payload(
        &self,
        event_type: &str,
        payload: Value,
    ) -> Result<(), ChartRuntimeError> {
        self.inner
            .send_and_wait(Event::with_payload(event_type, payload))
            .await?;
        Ok(())
    }

    /// Returns active relative state paths stripped of `<machine_id>.` prefix.
    pub fn active_states(&self) -> Vec<String> {
        let prefix = format!("{}.", self.machine_id);
        self.inner
            .view()
            .state_ids
            .into_iter()
            .map(|s| s.strip_prefix(&prefix).unwrap_or(&s).to_owned())
            .collect()
    }

    /// Checks whether `state` is currently active in the actor's live view.
    pub fn is_in(&self, state: &str) -> bool {
        state_matches_query(&self.machine_id, &self.inner.view().state_ids, state)
    }

    /// Returns `true` once the actor has reached a top-level `final_state`.
    pub fn is_done(&self) -> bool {
        self.inner.view().status == Status::Done
    }

    /// Returns the actor's final `output` once completed.
    pub fn output(&self) -> Value {
        let root_out = self.inner.view().output;
        if !root_out.is_null() {
            return root_out;
        }
        if self.is_done() {
            for st in self.active_states() {
                if let Some(leaf_out) = self.final_outputs.get(&st) {
                    return leaf_out.clone();
                }
            }
        }
        Value::Null
    }

    /// Fetches a snapshot of the actor's current extended state context.
    pub async fn context(&self) -> Result<Value, ChartRuntimeError> {
        let snap = self.inner.snapshot().await?;
        Ok(snap.context)
    }

    /// Waits reactively until the actor finishes (`Status::Done`) or stops/errors.
    pub async fn wait_done(&self) -> Result<(), ChartRuntimeError> {
        let view = self.inner.wait_done().await;
        if let Some(err) = view.error {
            return Err(ChartRuntimeError::Execution(err));
        }
        Ok(())
    }

    /// Waits reactively until `state` becomes active (or fails if the actor terminates elsewhere).
    pub async fn wait_for_state(&self, state: &str) -> Result<(), ChartRuntimeError> {
        let mut rx = self.inner.subscribe();
        loop {
            {
                let view = rx.borrow();
                if state_matches_query(&self.machine_id, &view.state_ids, state) {
                    return Ok(());
                }
                if let Some(err) = &view.error {
                    return Err(ChartRuntimeError::Execution(err.clone()));
                }
                if view.status != Status::Running {
                    return Err(ChartRuntimeError::Execution(format!(
                        "actor stopped in {:?} before reaching `{state}`",
                        view.state_ids
                    )));
                }
            }
            if rx.changed().await.is_err() {
                return Err(ChartRuntimeError::Execution(format!(
                    "actor channel closed before reaching `{state}`"
                )));
            }
        }
    }

    /// Gracefully stops the actor task.
    pub async fn stop(&self) {
        self.inner.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use starchart_starlark::{compile_starlark_file, compile_starlark_str};

    use super::*;

    fn load_order_workflow() -> CompiledChart {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/order_workflow.star");
        compile_starlark_file(path, None).expect("order_workflow.star must compile")
    }

    #[test]
    fn guard_false_path_keeps_machine_in_validating() {
        let chart = load_order_workflow();
        let mut runner =
            ChartSyncRunner::start(&chart, HostCallbacks::new()).expect("runner should start");

        assert!(runner.is_in("validating"));
        runner
            .send_with_payload("APPROVED", json!({"amount": 2500}))
            .expect("send APPROVED");

        assert!(runner.is_in("validating"));
        assert!(!runner.is_in("processing"));
    }

    #[test]
    fn guard_true_enters_compound_state_and_invokes_charge_card_once() {
        let chart = load_order_workflow();
        let charge_calls = Arc::new(AtomicUsize::new(0));
        let charge_calls_clone = Arc::clone(&charge_calls);

        let callbacks = HostCallbacks::new().on_action("CHARGE_CARD", move |_ctx, _ev| {
            charge_calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let mut runner = ChartSyncRunner::start(&chart, callbacks).expect("runner should start");
        runner
            .send_with_payload("APPROVED", json!({"amount": 250}))
            .expect("send APPROVED");

        assert!(
            runner.is_in("processing.charging") || runner.is_in("charging"),
            "active states: {:?}",
            runner.state_ids()
        );
        assert_eq!(charge_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pure_starlark_reducer_updates_attempts_and_transitions_to_failed() {
        let chart = load_order_workflow();
        let mut runner =
            ChartSyncRunner::start(&chart, HostCallbacks::new()).expect("runner should start");

        runner
            .send_with_payload("APPROVED", json!({"amount": 250}))
            .expect("enter charging");
        runner.send("CHARGE_ERR").expect("send CHARGE_ERR");

        assert!(runner.is_in("failed"));
        assert!(runner.is_done());
        assert_eq!(runner.context()["attempts"], json!(1));
    }

    #[test]
    fn parent_superstate_handles_cancel_from_nested_child() {
        let chart = load_order_workflow();
        let mut runner =
            ChartSyncRunner::start(&chart, HostCallbacks::new()).expect("runner should start");

        runner
            .send_with_payload("APPROVED", json!({"amount": 250}))
            .expect("enter processing.charging");
        assert!(runner.is_in("charging"));

        runner.send("CANCEL").expect("parent processing handles CANCEL");
        assert!(runner.is_in("rejected"));
        assert!(runner.is_done());
    }

    #[tokio::test]
    async fn tokio_async_actor_reaches_fulfilled_and_invokes_notify_user() {
        let chart = load_order_workflow();
        let notify_calls = Arc::new(AtomicUsize::new(0));
        let notify_clone = Arc::clone(&notify_calls);

        let callbacks = HostCallbacks::new().on_action("NOTIFY_USER", move |_ctx, _ev| {
            notify_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let actor = ChartActor::spawn(&chart, callbacks).expect("spawn actor");
        actor
            .send_with_payload("APPROVED", json!({"amount": 400}))
            .await
            .expect("send APPROVED");
        assert!(actor.is_in("charging"));

        actor.send("CHARGE_OK").await.expect("send CHARGE_OK");
        assert!(actor.is_in("fulfilled"));
        assert!(actor.is_done());
        assert_eq!(notify_calls.load(Ordering::SeqCst), 1);

        actor.stop().await;
    }

    #[test]
    fn record_context_state_space_internal_transition_and_guard_combinators_execute() {
        let src = r#"
OrderCtx = record(limit = int, attempts = int)
S = state_space("validating", "fulfilled", "rejected")

def under_limit(ctx: OrderCtx, event: dict) -> bool:
    return event.get("amount", 0) <= ctx.limit

def bump_attempts(ctx: OrderCtx, event: dict) -> OrderCtx:
    return OrderCtx(limit = ctx.limit, attempts = ctx.attempts + 1)

chart = machine(
    id = "enhanced_dsl",
    initial = S.validating,
    context = OrderCtx(limit = 500, attempts = 0),
    states = {
        S.validating: state(
            on = [
                internal_transition(event = "PING", assign = bump_attempts),
                transition(
                    event = "SUBMIT",
                    target = S.fulfilled,
                    guard = all_of(under_limit, not_(named_guard("IS_FRAUD"))),
                ),
                transition(event = "SUBMIT", target = S.rejected),
            ],
        ),
        S.fulfilled: final_state(),
        S.rejected: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("enhanced.star", src, None).expect("compile enhanced DSL");
        let callbacks = HostCallbacks::new().on_guard("IS_FRAUD", |_ctx, ev| {
            Ok(ev.payload.get("fraud").and_then(Value::as_bool).unwrap_or(false))
        });

        let mut runner = ChartSyncRunner::start(&chart, callbacks).expect("start runner");
        runner.send("PING").expect("internal transition 1");
        runner.send("PING").expect("internal transition 2");
        assert!(runner.is_in("validating"));
        assert_eq!(runner.context()["attempts"], json!(2));

        runner
            .send_with_payload("SUBMIT", json!({"amount": 300, "fraud": false}))
            .expect("submit valid order");
        assert!(runner.is_in("fulfilled"));
        assert!(runner.is_done());
    }

    #[test]
    fn sync_invoke_passes_output_to_starlark_reducer_and_transitions() {
        let src = r#"
CheckoutCtx = record(amount = int, receipt_id = str, error = str)
S = state_space("idle", "charging", "paid", "declined")
Service = enum("STRIPE_CHARGE")

def store_receipt(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    out = event.get("output", {})
    return CheckoutCtx(
        amount = ctx.amount,
        receipt_id = out.get("receipt_id", ""),
        error = "",
    )

def store_error(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    return CheckoutCtx(
        amount = ctx.amount,
        receipt_id = "",
        error = str(event.get("error", "unknown")),
    )

chart = machine(
    id = "checkout_sync_invoke",
    initial = S.idle,
    context = CheckoutCtx(amount = 4900, receipt_id = "", error = ""),
    states = {
        S.idle: state(
            on = [transition(event = "PAY", target = S.charging)],
        ),
        S.charging: state(
            invoke = invoke(
                src = Service.STRIPE_CHARGE,
                id = "stripe_charge",
                input = {"currency": "USD"},
                on_done = outcome(target = S.paid, assign = store_receipt),
                on_error = outcome(target = S.declined, assign = store_error),
            ),
        ),
        S.paid: final_state(),
        S.declined: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("checkout_invoke.star", src, None).expect("compile");

        let ok_callbacks = HostCallbacks::new().on_sync_service("STRIPE_CHARGE", |ctx, input| {
            assert_eq!(ctx["amount"], json!(4900));
            assert_eq!(input["currency"], json!("USD"));
            Ok(json!({"receipt_id": "rcpt_9988"}))
        });

        let mut runner = ChartSyncRunner::start(&chart, ok_callbacks).expect("start");
        runner.send("PAY").expect("PAY triggers sync invoke");
        assert!(runner.is_in("paid"));
        assert!(runner.is_done());
        assert_eq!(runner.context()["receipt_id"], json!("rcpt_9988"));

        let err_callbacks = HostCallbacks::new().on_sync_service("STRIPE_CHARGE", |_ctx, _inp| {
            anyhow::bail!("card_declined")
        });

        let mut err_runner = ChartSyncRunner::start(&chart, err_callbacks).expect("start");
        err_runner.send("PAY").expect("PAY triggers failing invoke");
        assert!(err_runner.is_in("declined"));
        assert!(err_runner.is_done());
        assert!(
            err_runner.context()["error"]
                .as_str()
                .unwrap_or_default()
                .contains("card_declined")
        );
    }

    #[tokio::test]
    async fn async_tokio_invoke_executes_and_cancels_cleanly_on_state_exit() {
        let src = r#"
S = state_space("idle", "authorizing", "authorized", "cancelled", "failed")

def save_token(ctx: dict, event: dict) -> dict:
    next_ctx = dict(ctx)
    next_ctx["token"] = event.get("output", {}).get("token", "")
    return next_ctx

chart = machine(
    id = "async_payment",
    initial = S.idle,
    context = {"token": ""},
    states = {
        S.idle: state(
            on = [transition(event = "BEGIN", target = S.authorizing)],
        ),
        S.authorizing: state(
            invoke = invoke(
                src = "AUTHORIZE_PAYMENT",
                on_done = outcome(target = S.authorized, assign = save_token),
                on_error = S.failed,
            ),
            on = [
                transition(event = "CANCEL", target = S.cancelled),
            ],
        ),
        S.authorized: final_state(),
        S.cancelled: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("async_payment.star", src, None).expect("compile");

        let ok_callbacks =
            HostCallbacks::new().on_async_service("AUTHORIZE_PAYMENT", |_ctx, _input| async move {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                Ok(json!({"token": "tok_live_abc"}))
            });

        let actor = ChartActor::spawn(&chart, ok_callbacks).expect("spawn");
        actor.send("BEGIN").await.expect("BEGIN");
        actor.wait_done().await.expect("wait_done");
        assert!(actor.is_in("authorized"));
        assert_eq!(actor.context().await.unwrap()["token"], json!("tok_live_abc"));
        actor.stop().await;

        let slow_callbacks =
            HostCallbacks::new().on_async_service("AUTHORIZE_PAYMENT", |_ctx, _input| async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(json!({"token": "should_never_arrive"}))
            });

        let cancel_actor = ChartActor::spawn(&chart, slow_callbacks).expect("spawn");
        cancel_actor.send("BEGIN").await.expect("BEGIN");
        assert!(cancel_actor.is_in("authorizing"));
        cancel_actor.send("CANCEL").await.expect("CANCEL");
        assert!(cancel_actor.is_in("cancelled"));
        assert!(cancel_actor.is_done());
        assert_eq!(cancel_actor.context().await.unwrap()["token"], json!(""));
        cancel_actor.stop().await;
    }

    #[test]
    fn always_and_after_with_dynamic_starlark_backoff_execute_deterministically() {
        let src = r#"
RetryCtx = record(attempts = int, max_attempts = int)
S = state_space("connecting", "decide_retry", "backoff_wait", "connected", "exhausted")

def can_retry(ctx: RetryCtx, event: dict) -> bool:
    return ctx.attempts < ctx.max_attempts

def inc_attempts(ctx: RetryCtx, event: dict) -> RetryCtx:
    return RetryCtx(attempts = ctx.attempts + 1, max_attempts = ctx.max_attempts)

def backoff_ms(ctx: RetryCtx, event: dict) -> int:
    return 100 * (1 << (ctx.attempts - 1))

chart = machine(
    id = "retry_backoff",
    initial = S.connecting,
    context = RetryCtx(attempts = 0, max_attempts = 2),
    states = {
        S.connecting: state(
            on = [
                transition(event = "OK", target = S.connected),
                transition(event = "FAIL", target = S.decide_retry, assign = inc_attempts),
            ],
            after = after(delay = 1000, target = S.decide_retry, assign = inc_attempts),
        ),
        S.decide_retry: state(
            always = [
                outcome(target = S.backoff_wait, guard = can_retry),
                S.exhausted,
            ],
        ),
        S.backoff_wait: state(
            after = after(delay = backoff_ms, target = S.connecting),
        ),
        S.connected: final_state(),
        S.exhausted: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("retry_backoff.star", src, None).expect("compile");
        let mut runner = ChartSyncRunner::start(&chart, HostCallbacks::new()).expect("start");

        assert!(runner.is_in("connecting"));
        runner.advance_ms(999.0).expect("not yet");
        assert!(runner.is_in("connecting"));
        runner.advance_ms(1.0).expect("1000ms elapsed");
        assert!(runner.is_in("backoff_wait"));
        assert_eq!(runner.context()["attempts"], json!(1));

        runner.advance_ms(99.0).expect("99ms < 100ms");
        assert!(runner.is_in("backoff_wait"));
        runner.advance_ms(1.0).expect("100ms elapsed -> back to connecting");
        assert!(runner.is_in("connecting"));

        runner.send("FAIL").expect("second failure");
        assert!(runner.is_in("exhausted"));
        assert!(runner.is_done());
        assert_eq!(runner.context()["attempts"], json!(2));
    }

    #[tokio::test]
    async fn tokio_actor_fires_real_after_timer() {
        let src = r#"
S = state_space("waiting", "timed_out")
chart = machine(
    id = "tokio_timer",
    initial = S.waiting,
    states = {
        S.waiting: state(
            after = after(delay = 20, target = S.timed_out),
        ),
        S.timed_out: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("tokio_timer.star", src, None).expect("compile");
        let actor = ChartActor::spawn(&chart, HostCallbacks::new()).expect("spawn");
        actor.wait_done().await.expect("timer should fire and finish");
        assert!(actor.is_in("timed_out"));
        actor.stop().await;
    }

    #[test]
    fn parallel_on_done_state_in_history_forbid_raise_emit_tags_and_output_all_work() {
        let src = r#"
S = state_space(
    "booting",
    "audio_region",
    "audio_init",
    "audio_ready",
    "video_region",
    "video_init",
    "video_ready",
    "playback",
    "playing",
    "buffering",
    "playback_hist",
    "paused",
    "finished",
)

chart = machine(
    id = "full_harel_showcase",
    initial = S.booting,
    output = {"status": "playback_complete"},
    states = {
        # 1. Parallel fork-join with `state_in(...)` guard + `on_done`!
        S.booting: parallel_state(
            tags = ["loading"],
            on_done = outcome(
                target = S.playback,
                actions = [emit_event(event = "BOOT_READY", payload = {"ok": True})],
            ),
            states = {
                S.audio_region: compound_state(
                    initial = S.audio_init,
                    states = {
                        S.audio_init: state(
                            on = [transition(event = "AUDIO_OK", target = S.audio_ready)],
                        ),
                        S.audio_ready: final_state(),
                    },
                ),
                S.video_region: compound_state(
                    initial = S.video_init,
                    states = {
                        S.video_init: state(
                            on = [
                                # Only allow video to finalize once audio is already ready!
                                transition(
                                    event = "VIDEO_OK",
                                    target = S.video_ready,
                                    guard = state_in(S.audio_ready),
                                ),
                            ],
                        ),
                        S.video_ready: final_state(),
                    },
                ),
            },
        ),
        # 2. Compound state with `shallow_history`, `forbid`, delayed `raise_event`, `cancel_send`!
        S.playback: compound_state(
            initial = S.playing,
            tags = ["active"],
            on = [
                transition(event = "PAUSE", target = S.paused),
                transition(event = "STOP", target = S.finished),
            ],
            states = {
                S.playback_hist: shallow_history(target = S.playing),
                S.playing: state(
                    entry = [log_action("entered_playing")],
                    on = [
                        transition(event = "STALL", target = S.buffering),
                        # Re-entering self-transition triggers `entry` log again!
                        transition(event = "SEEK", target = S.playing, reenter = True),
                    ],
                ),
                S.buffering: state(
                    tags = ["loading"],
                    entry = [
                        raise_event(event = "BUFFER_TIMEOUT", delay = 500, id = "buf_timer"),
                    ],
                    exit = [cancel_send(id = "buf_timer")],
                    on = [
                        # Forbid `STOP` while buffering so parent `playback` doesn't handle it!
                        forbid(event = "STOP"),
                        transition(event = "BUFFERED", target = S.playing),
                        transition(event = "BUFFER_TIMEOUT", target = S.finished),
                    ],
                ),
            },
        ),
        S.paused: state(
            on = [transition(event = "RESUME", target = S.playback_hist)],
        ),
        S.finished: final_state(
            output = {"status": "playback_complete"},
        ),
    },
)
"#;
        let chart = compile_starlark_str("full_harel.star", src, None).expect("compile full DSL");
        let mut runner = ChartSyncRunner::start(&chart, HostCallbacks::new()).expect("start");

        // Parallel boot + tags + `state_in` cross-region guard:
        assert!(runner.has_tag("loading"));
        runner.send("VIDEO_OK").expect("blocked because audio not ready");
        assert!(runner.is_in("video_init"));

        runner.send("AUDIO_OK").expect("audio finishes");
        assert!(runner.is_in("audio_ready"));
        assert!(!runner.is_in("playback"));

        runner.send("VIDEO_OK").expect("both regions final -> parallel on_done fires!");
        assert!(runner.is_in("playing"));
        assert!(runner.has_tag("active"));
        assert!(!runner.has_tag("loading"));
        assert_eq!(
            runner.take_emitted(),
            vec![json!({"type": "BOOT_READY", "ok": true})]
        );
        assert_eq!(runner.take_logs().len(), 1);

        // `reenter = True` on SEEK re-runs `playing`'s entry `log_action`:
        runner.send("SEEK").expect("reenter playing");
        assert_eq!(runner.take_logs().len(), 1);

        // Enter `buffering`: `forbid(event = "STOP")` blocks parent `STOP`,
        // then `PAUSE` -> `RESUME` via `shallow_history` restores `buffering`!
        runner.send("STALL").expect("enter buffering");
        assert!(runner.is_in("buffering"));
        assert!(runner.has_tag("loading"));

        runner.send("STOP").expect("forbidden in buffering");
        assert!(runner.is_in("buffering"));
        assert!(!runner.is_done());

        runner.send("PAUSE").expect("pause exits buffering and cancels buf_timer");
        assert!(runner.is_in("paused"));
        runner.advance_ms(1000.0).expect("cancelled timer must not fire");
        assert!(runner.is_in("paused"));

        runner.send("RESUME").expect("shallow_history restores buffering!");
        assert!(runner.is_in("buffering"));

        // Now let `raise_event(..., delay = 500)` fire!
        runner.advance_ms(500.0).expect("delayed raise BUFFER_TIMEOUT fires");
        assert!(runner.is_in("finished"));
        assert!(runner.is_done());
        assert_eq!(runner.output(), &json!({"status": "playback_complete"}));
    }

    #[tokio::test]
    async fn builtin_http_driver_executes_checkout_http_with_retry_and_webhook() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        let charge_hits = Arc::new(AtomicUsize::new(0));
        let webhook_hits = Arc::new(AtomicUsize::new(0));
        let charge_hits_srv = Arc::clone(&charge_hits);
        let webhook_hits_srv = Arc::clone(&webhook_hits);

        tokio::spawn(async move {
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                let (status_line, resp_body) = if req_str.starts_with("POST /v1/charges ") {
                    let hit = charge_hits_srv.fetch_add(1, Ordering::SeqCst);
                    if hit == 0 {
                        // First attempt returns 503 Service Unavailable -> triggers Starlark retry!
                        ("503 Service Unavailable", r#"{"error":"overloaded"}"#)
                    } else {
                        ("200 OK", r#"{"paid":true,"charge_id":"ch_live_99"}"#)
                    }
                } else {
                    webhook_hits_srv.fetch_add(1, Ordering::SeqCst);
                    ("200 OK", r#"{"received":true}"#)
                };

                let http_resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = sock.write_all(http_resp.as_bytes()).await;
            }
        });

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/checkout_http.star");
        let mut chart = compile_starlark_file(path, None).expect("checkout_http.star must compile");
        chart.ir.context["api_base"] = json!(format!("http://127.0.0.1:{port}"));

        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn checkout_http");
        actor.wait_done().await.expect("actor should finish");

        assert!(actor.is_in("completed"));
        assert_eq!(actor.output(), json!({"status": "paid_and_notified"}));
        let final_ctx = actor.context().await.expect("final context");
        assert_eq!(final_ctx["charge_id"], json!("ch_live_99"));
        assert_eq!(final_ctx["attempts"], json!(1));
        assert_eq!(charge_hits.load(Ordering::SeqCst), 2);
        assert_eq!(webhook_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn builtin_exec_driver_captures_stdout_and_handles_nonzero_exit() {
        let src = r#"
load("@starchart/exec", "invoke_cmd")

Ctx = record(greeting = str, err_msg = str)
S = state_space("step_fail", "step_ok", "done", "failed")

def save_err(ctx: Ctx, event: dict) -> Ctx:
    return Ctx(greeting = ctx.greeting, err_msg = str(event.get("error", "")))

def save_out(ctx: Ctx, event: dict) -> Ctx:
    return Ctx(greeting = event["output"]["stdout"].strip(), err_msg = ctx.err_msg)

chart = machine(
    id = "exec_demo",
    initial = S.step_fail,
    context = Ctx(greeting = "", err_msg = ""),
    states = {
        S.step_fail: state(
            invoke = invoke_cmd(
                cmd = ["sh", "-c", "echo boom >&2; exit 7"],
                on_done = S.failed,
                on_error = outcome(target = S.step_ok, assign = save_err),
            ),
        ),
        S.step_ok: state(
            invoke = invoke_cmd(
                cmd = ["sh", "-c", "printf 'hello-starchart'"],
                on_done = outcome(target = S.done, assign = save_out),
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("exec_demo.star", src, None).expect("compile exec_demo");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn exec_demo");
        actor.wait_done().await.expect("wait done");

        assert!(actor.is_in("done"));
        let ctx = actor.context().await.expect("context");
        assert_eq!(ctx["greeting"], json!("hello-starchart"));
        assert!(
            ctx["err_msg"].as_str().unwrap_or("").contains("code 7"),
            "expected exit code 7 in err_msg: {ctx}"
        );
    }

    #[tokio::test]
    async fn claude_and_codex_cli_drivers_execute_in_parallel_state() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("starchart_cli_agents_{uniq}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let mock_claude = dir.join("mock_claude.sh");
        std::fs::write(
            &mock_claude,
            "#!/bin/sh\nprompt=$(cat)\nprintf '{\"result\":\"claude_saw:%s\",\"session_id\":\"sess_c1\",\"is_error\":false}\\n' \"$prompt\"\n",
        )
        .expect("write mock claude");

        let mock_codex = dir.join("mock_codex.sh");
        std::fs::write(
            &mock_codex,
            "#!/bin/sh\nout_file=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"-o\" ]; then out_file=\"$2\"; shift 2; else shift; fi\ndone\nprompt=$(cat)\nprintf 'codex_saw:%s' \"$prompt\" > \"$out_file\"\n",
        )
        .expect("write mock codex");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&mock_claude, perm.clone()).unwrap();
            std::fs::set_permissions(&mock_codex, perm).unwrap();
        }

        let src = format!(
            r#"
load("@starchart/agents", "claude_agent", "codex_agent", "invoke_agent")

Ctx = record(claude_out = str, codex_out = str)
S = state_space("reviewing", "c_branch", "c_run", "c_done", "x_branch", "x_run", "x_done", "done", "err")

claude = claude_agent(id = "claude_rev", command = {claude_cmd:?}, permission_mode = "plan")
codex = codex_agent(id = "codex_rev", command = {codex_cmd:?}, sandbox = "read-only")

def save_claude(ctx: Ctx, event: dict) -> Ctx:
    return Ctx(claude_out = event["output"]["text"], codex_out = ctx.codex_out)

def save_codex(ctx: Ctx, event: dict) -> Ctx:
    return Ctx(claude_out = ctx.claude_out, codex_out = event["output"]["text"])

chart = machine(
    id = "parallel_cli_agents",
    initial = S.reviewing,
    context = Ctx(claude_out = "", codex_out = ""),
    states = {{
        S.reviewing: parallel_state(
            on_done = S.done,
            states = {{
                S.c_branch: compound_state(
                    initial = S.c_run,
                    states = {{
                        S.c_run: state(
                            invoke = invoke_agent(
                                agent = claude,
                                prompt = "check-ergonomics",
                                on_done = outcome(target = S.c_done, assign = save_claude),
                                on_error = S.err,
                            ),
                        ),
                        S.c_done: final_state(),
                    }},
                ),
                S.x_branch: compound_state(
                    initial = S.x_run,
                    states = {{
                        S.x_run: state(
                            invoke = invoke_agent(
                                agent = codex,
                                prompt = "check-edge-cases",
                                on_done = outcome(target = S.x_done, assign = save_codex),
                                on_error = S.err,
                            ),
                        ),
                        S.x_done: final_state(),
                    }},
                ),
            }},
        ),
        S.done: final_state(),
        S.err: final_state(),
    }},
)
"#,
            claude_cmd = mock_claude.to_string_lossy(),
            codex_cmd = mock_codex.to_string_lossy(),
        );

        let chart = compile_starlark_str("parallel_cli_agents.star", &src, None).expect("compile");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn");
        actor.wait_done().await.expect("wait done");

        let _ = std::fs::remove_dir_all(&dir);

        assert!(actor.is_in("done"));
        let ctx = actor.context().await.expect("context");
        assert_eq!(ctx["claude_out"], json!("claude_saw:check-ergonomics"));
        assert_eq!(ctx["codex_out"], json!("codex_saw:check-edge-cases"));
    }

    // ---------------------------------------------------------------------------------------
    // Trace stream
    // ---------------------------------------------------------------------------------------

    fn assert_contiguous(records: &[TraceRecord]) {
        for (i, r) in records.iter().enumerate() {
            assert_eq!(r.seq, i as u64 + 1, "gap or reorder at index {i}: {records:#?}");
        }
    }

    #[test]
    fn sync_runner_trace_records_started_and_every_transition_in_order() {
        let chart = load_order_workflow();
        let mut runner = StarchartSyncRunner::start(&chart, HostCallbacks::new()).expect("start");
        let mut rx = runner.subscribe_traces();

        runner
            .send_with_payload("APPROVED", json!({"amount": 250}))
            .expect("APPROVED");
        runner.send("CHARGE_ERR").expect("CHARGE_ERR");

        let records = rx.drain();
        assert_contiguous(&records);
        assert_eq!(records.len(), 3, "{records:#?}");

        match &records[0].event {
            TraceEvent::Started { states, context, .. } => {
                assert_eq!(states, &vec!["validating".to_owned()]);
                assert_eq!(context["attempts"], json!(0));
            }
            other => panic!("expected Started, got {other:?}"),
        }
        match &records[1].event {
            TraceEvent::Transition { event, from, to, actions, .. } => {
                assert_eq!(event.event_type, "APPROVED");
                assert_eq!(event.payload["amount"], json!(250));
                assert_eq!(from, &vec!["validating".to_owned()]);
                assert_eq!(to, &vec!["processing.charging".to_owned()]);
                assert!(actions.contains(&"CHARGE_CARD".to_owned()), "{actions:?}");
            }
            other => panic!("expected Transition, got {other:?}"),
        }
        match &records[2].event {
            TraceEvent::Transition { from, to, actions, context, status, .. } => {
                assert_eq!(from, &vec!["processing.charging".to_owned()]);
                assert_eq!(to, &vec!["failed".to_owned()]);
                // Starlark reducer slot names are mapped back to their function names.
                assert!(
                    actions.contains(&"assign increment_attempts".to_owned()),
                    "{actions:?}"
                );
                assert_eq!(context["attempts"], json!(1));
                assert_eq!(status, "done");
            }
            other => panic!("expected Transition, got {other:?}"),
        }

        let line = records[2].to_json_line();
        let parsed: Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["kind"], json!("transition"));
        assert_eq!(parsed["seq"], json!(3));
        assert_eq!(parsed["machine"], json!("order_workflow"));
        assert_eq!(parsed["event"]["type"], json!("CHARGE_ERR"));
    }

    #[test]
    fn late_subscriber_gets_full_history_then_live_records() {
        let chart = load_order_workflow();
        let mut runner = StarchartSyncRunner::start(&chart, HostCallbacks::new()).expect("start");
        runner
            .send_with_payload("APPROVED", json!({"amount": 250}))
            .expect("APPROVED");

        // Attach after two records already happened.
        let mut late = runner.subscribe_traces();
        runner.send("CHARGE_OK").expect("CHARGE_OK");

        let records = late.drain();
        assert_contiguous(&records);
        assert_eq!(records.len(), 3);
        assert!(matches!(records[0].event, TraceEvent::Started { .. }));
        assert!(matches!(
            &records[2].event,
            TraceEvent::Transition { to, .. } if to == &vec!["fulfilled".to_owned()]
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_subscriber_on_another_thread_receives_every_transition() {
        let src = r#"
S = state_space("counting", "done")

def bump(ctx: dict, event: dict) -> dict:
    next_ctx = dict(ctx)
    next_ctx["n"] = ctx["n"] + 1
    return next_ctx

chart = machine(
    id = "burst",
    initial = S.counting,
    context = {"n": 0},
    states = {
        S.counting: state(
            on = [
                internal_transition(event = "TICK", assign = bump),
                transition(event = "FINISH", target = S.done),
            ],
        ),
        S.done: final_state(),
    },
)
"#;
        const TICKS: usize = 200;
        let chart = compile_starlark_str("burst.star", src, None).expect("compile");
        let actor = StarchartActor::spawn(&chart, HostCallbacks::new()).expect("spawn");
        let mut rx = actor.subscribe_traces();

        // Deliberately slow consumer on a plain OS thread.
        let consumer = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while let Some(record) = rx.blocking_recv() {
                std::thread::sleep(std::time::Duration::from_micros(300));
                seen.push(record);
            }
            seen
        });

        for _ in 0..TICKS {
            actor.send("TICK").await.expect("TICK");
        }
        actor.send("FINISH").await.expect("FINISH");
        actor.wait_done().await.expect("done");
        actor.stop().await;
        actor.trace_hub().close();

        let seen = tokio::task::spawn_blocking(move || consumer.join().expect("consumer thread"))
            .await
            .expect("join");
        assert_contiguous(&seen);

        let transitions: Vec<_> = seen
            .iter()
            .filter_map(|r| match &r.event {
                TraceEvent::Transition { event, context, .. } => {
                    Some((event.event_type.clone(), context["n"].clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(transitions.len(), TICKS + 1, "every transition must arrive");
        for (i, (ty, n)) in transitions.iter().take(TICKS).enumerate() {
            assert_eq!(ty, "TICK");
            assert_eq!(n, &json!(i + 1), "context snapshot per step");
        }
        assert_eq!(transitions[TICKS].0, "FINISH");
    }

    #[tokio::test]
    async fn exec_service_streams_stdout_and_stderr_progress_with_lifecycle() {
        let src = r#"
load("@starchart/exec", "invoke_cmd")

S = state_space("building", "ok", "failed")

chart = machine(
    id = "build",
    initial = S.building,
    states = {
        S.building: state(
            invoke = invoke_cmd(
                id = "compile",
                cmd = ["sh", "-c", "echo one; echo two >&2; echo three"],
                on_done = S.ok,
                on_error = S.failed,
            ),
        ),
        S.ok: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("build.star", src, None).expect("compile");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn");
        actor.wait_done().await.expect("done");
        assert!(actor.is_in("ok"));
        actor.stop().await;

        let records = actor.trace_hub().history();
        assert_contiguous(&records);

        let started = records
            .iter()
            .find_map(|r| match &r.event {
                TraceEvent::ServiceStarted { invoke_id, state, input, .. } => {
                    Some((invoke_id.clone(), state.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("service_started");
        assert_eq!(started.0, "compile");
        assert_eq!(started.1, "building");
        assert_eq!(started.2["cmd"][0], json!("sh"), "unwrapped input: {}", started.2);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for r in &records {
            if let TraceEvent::ServiceProgress { invoke_id, stream, chunk, .. } = &r.event {
                assert_eq!(invoke_id, "compile");
                match stream {
                    ProgressKind::Stdout => stdout.push(chunk.clone()),
                    ProgressKind::Stderr => stderr.push(chunk.clone()),
                    other => panic!("unexpected stream {other:?}"),
                }
            }
        }
        assert_eq!(stdout, vec!["one", "three"]);
        assert_eq!(stderr, vec!["two"]);

        let finished = records
            .iter()
            .find_map(|r| match &r.event {
                TraceEvent::ServiceFinished { ok, output, .. } => Some((*ok, output.clone())),
                _ => None,
            })
            .expect("service_finished");
        assert!(finished.0);
        assert_eq!(finished.1.expect("output")["stdout"], json!("one\nthree\n"));

        // The completion transition follows the service's progress.
        let finish_seq = records
            .iter()
            .find(|r| matches!(r.event, TraceEvent::ServiceFinished { .. }))
            .map(|r| r.seq)
            .expect("finish seq");
        let done_transition = records
            .iter()
            .find(|r| matches!(&r.event, TraceEvent::Transition { to, .. } if to == &vec!["ok".to_owned()]))
            .expect("transition to ok");
        assert!(done_transition.seq > finish_seq);
    }

    #[tokio::test]
    async fn custom_async_service_can_report_progress_and_failures_are_traced() {
        let src = r#"
S = state_space("working", "ok", "failed")
chart = machine(
    id = "custom",
    initial = S.working,
    states = {
        S.working: state(
            invoke = invoke(src = "JOB", input = {"n": 2}, on_done = S.ok, on_error = S.failed),
        ),
        S.ok: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let chart = compile_starlark_str("custom.star", src, None).expect("compile");
        let callbacks = HostCallbacks::new().on_async_service("JOB", |_ctx, input| async move {
            for i in 0..input["n"].as_u64().unwrap_or(0) {
                report_progress(ProgressKind::Log, format!("step {i}"));
            }
            anyhow::bail!("job exploded")
        });
        let actor = StarchartActor::spawn(&chart, callbacks).expect("spawn");
        actor.wait_done().await.expect("done");
        assert!(actor.is_in("failed"));
        actor.stop().await;

        let records = actor.trace_hub().history();
        let progress: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.event {
                TraceEvent::ServiceProgress { invoke_id, state, chunk, .. } => {
                    Some((invoke_id.clone(), state.clone(), chunk.clone()))
                }
                _ => None,
            })
            .collect();
        // Unnamed invokes are identified by their owning state.
        assert_eq!(
            progress,
            vec![
                ("working".to_owned(), "working".to_owned(), "step 0".to_owned()),
                ("working".to_owned(), "working".to_owned(), "step 1".to_owned()),
            ]
        );
        let failure = records
            .iter()
            .find_map(|r| match &r.event {
                TraceEvent::ServiceFinished { ok: false, error, .. } => error.clone(),
                _ => None,
            })
            .expect("failed service_finished");
        assert!(failure.contains("job exploded"), "{failure}");
        assert!(matches!(records.last().map(|r| &r.event), Some(TraceEvent::Stopped { .. })));
    }

    #[tokio::test]
    async fn acp_turn_mcp_tools_read_context_repair_invalid_artifact_and_record_modes() {
        // Mock ACP v1 agent subprocess that launches the `mcpServers[0]` stdio MCP bridge
        // provided in `session/new`, calls `tools/list` + `tools/call` (`read_tool`, invalid
        // `artifact_tool` -> self-repair -> `replace`, `append`, and `once`), and completes turn.
        let mock_acp_agent_py = r#"
import json, subprocess, sys

def send_acp(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def mcp_rpc(proc, req_id, method, params=None):
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode("utf-8"))
    proc.stdin.flush()
    line = proc.stdout.readline().decode("utf-8")
    return json.loads(line)

mcp_proc = None
for raw in sys.stdin:
    req = json.loads(raw)
    m = req.get("method")
    rid = req.get("id")
    if m == "initialize":
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif m == "session/new":
        servers = req["params"]["mcpServers"]
        assert len(servers) == 1, f"expected 1 mcpServer, got {servers}"
        srv = servers[0]
        mcp_proc = subprocess.Popen(
            [srv["command"]] + srv["args"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
        )
        init_res = mcp_rpc(mcp_proc, 1, "initialize", {"protocolVersion": "2024-11-05"})
        assert init_res["result"]["serverInfo"]["name"] == "starchart"
        mcp_proc.stdin.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n").encode("utf-8"))
        mcp_proc.stdin.flush()
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "sess-mcp-1"}})
    elif m == "session/prompt":
        tools_res = mcp_rpc(mcp_proc, 2, "tools/list")
        names = [t["name"] for t in tools_res["result"]["tools"]]
        assert names == ["get_failure_context", "submit_plan", "report_finding", "seal_turn"], f"tools={names}"

        # 1. Call parameterized read_tool
        read_res = mcp_rpc(mcp_proc, 3, "tools/call", {
            "name": "get_failure_context",
            "arguments": {"section": "tests"}
        })
        assert read_res["result"]["isError"] is False, read_res
        read_payload = json.loads(read_res["result"]["content"][0]["text"])
        assert read_payload == {"section": "tests", "errors": "assertion failed on line 42", "attempt": 1}

        # 2. Call submit_plan with empty steps -> rejected by Starlark validator (`isError: true`)
        bad_plan = mcp_rpc(mcp_proc, 4, "tools/call", {
            "name": "submit_plan",
            "arguments": {"summary": "Empty plan", "steps": []}
        })
        assert bad_plan["result"]["isError"] is True
        assert "at least one step" in bad_plan["result"]["content"][0]["text"]

        # 3. Call submit_plan with wrong field type -> rejected by Starlark Record constructor
        bad_type = mcp_rpc(mcp_proc, 5, "tools/call", {
            "name": "submit_plan",
            "arguments": {"summary": "Wrong type", "steps": "not-a-list"}
        })
        assert bad_type["result"]["isError"] is True

        # 4. Call submit_plan with valid v1, then revise with v2 (`mode = "replace"` -> v2 wins, default `risk` filled)
        ok_v1 = mcp_rpc(mcp_proc, 6, "tools/call", {
            "name": "submit_plan",
            "arguments": {"summary": "Draft v1", "steps": ["step 1"]}
        })
        assert ok_v1["result"]["isError"] is False, ok_v1
        ok_v2 = mcp_rpc(mcp_proc, 7, "tools/call", {
            "name": "submit_plan",
            "arguments": {"summary": "Final v2", "steps": ["fix line 42", "rerun cargo test"]}
        })
        assert ok_v2["result"]["isError"] is False, ok_v2

        # 5. Call report_finding twice (`mode = "append"`)
        f1 = mcp_rpc(mcp_proc, 8, "tools/call", {
            "name": "report_finding",
            "arguments": {"severity": "HIGH", "message": "off-by-one"}
        })
        f2 = mcp_rpc(mcp_proc, 9, "tools/call", {
            "name": "report_finding",
            "arguments": {"severity": "LOW", "message": "missing doc comment"}
        })
        assert f1["result"]["isError"] is False and f2["result"]["isError"] is False

        # 6. Call seal_turn twice (`mode = "once"` -> second call rejected, first preserved)
        s1 = mcp_rpc(mcp_proc, 10, "tools/call", {
            "name": "seal_turn",
            "arguments": {"signature": "sig-first"}
        })
        s2 = mcp_rpc(mcp_proc, 11, "tools/call", {
            "name": "seal_turn",
            "arguments": {"signature": "sig-second"}
        })
        assert s1["result"]["isError"] is False
        assert s2["result"]["isError"] is True

        mcp_proc.stdin.close()
        mcp_proc.wait(timeout=5)

        send_acp({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Plan submitted."}}}
        })
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        break
"#;

        let src = format!(
            r#"
load("@starchart/acp", "ArtifactMode", "acp_agent", "artifact_tool", "invoke_agent", "read_tool")

Severity = enum("LOW", "HIGH")

Ctx = record(
    test_errors = str,
    attempt = int,
    plan_summary = str,
    step_count = int,
    plan_risk = str,
    finding_count = int,
    seal_sig = str,
)

Query = record(section = str)

Plan = record(
    summary = str,
    steps = list[str],
    risk = field(str, "low"),
)

Finding = record(
    severity = Severity,
    message = str,
)

Seal = record(signature = str)

def read_ctx(ctx: Ctx, q: Query) -> dict:
    return {{
        "section": q.section,
        "errors": ctx.test_errors,
        "attempt": ctx.attempt,
    }}

def validate_plan(ctx: Ctx, plan: Plan) -> str | None:
    if len(plan.steps) == 0:
        return "Plan must contain at least one step"
    return None

def has_two_steps(ctx: Ctx, event: dict) -> bool:
    plan = event.get("output", {{}}).get("artifacts", {{}}).get("submit_plan", {{}})
    return len(plan.get("steps", [])) == 2

def save_turn_artifacts(ctx: Ctx, event: dict) -> Ctx:
    artifacts = event.get("output", {{}}).get("artifacts", {{}})
    plan = artifacts["submit_plan"]
    findings = artifacts["report_finding"]
    seal = artifacts["seal_turn"]
    return Ctx(
        test_errors = ctx.test_errors,
        attempt = ctx.attempt,
        plan_summary = plan["summary"],
        step_count = len(plan["steps"]),
        plan_risk = plan["risk"],
        finding_count = len(findings),
        seal_sig = seal["signature"],
    )

planner = acp_agent(
    id = "planner",
    command = "python3",
    args = ["-u", "-c", {py_script}],
)

S = state_space("planning", "done", "failed")

chart = machine(
    id = "mcp_turn_test",
    initial = S.planning,
    context = Ctx(
        test_errors = "assertion failed on line 42",
        attempt = 1,
        plan_summary = "",
        step_count = 0,
        plan_risk = "",
        finding_count = 0,
        seal_sig = "",
    ),
    states = {{
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = "Create and submit plan",
                tools = [
                    read_tool(
                        "get_failure_context",
                        description = "Read failure details",
                        schema = Query,
                        handler = read_ctx,
                    ),
                    artifact_tool(
                        "submit_plan",
                        description = "Submit implementation plan",
                        schema = Plan,
                        mode = ArtifactMode.replace,
                        required = True,
                        validate = validate_plan,
                    ),
                    artifact_tool(
                        "report_finding",
                        description = "Append finding",
                        schema = Finding,
                        mode = ArtifactMode.append,
                        required = False,
                    ),
                    artifact_tool(
                        "seal_turn",
                        description = "Seal the turn once",
                        schema = Seal,
                        mode = ArtifactMode.once,
                        required = True,
                    ),
                ],
                on_done = [
                    outcome(target = S.done, guard = has_two_steps, assign = save_turn_artifacts),
                    outcome(target = S.failed),
                ],
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    }},
)
"#,
            py_script = serde_json::to_string(mock_acp_agent_py).unwrap(),
        );

        let chart = compile_starlark_str("mcp_turn_test.star", &src, None).expect("compile");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn");
        actor.wait_done().await.expect("wait_done");
        let final_ctx = actor.context().await.expect("context");
        assert!(
            actor.is_in("done"),
            "expected `done`, active={:?}, ctx={final_ctx}",
            actor.active_states(),
        );
        assert_eq!(final_ctx["plan_summary"], json!("Final v2"));
        assert_eq!(final_ctx["step_count"], json!(2));
        assert_eq!(final_ctx["plan_risk"], json!("low"));
        assert_eq!(final_ctx["finding_count"], json!(2));
        assert_eq!(final_ctx["seal_sig"], json!("sig-first"));

        actor.stop().await;
        let records = actor.trace_hub().history();
        let artifact_chunks: Vec<Value> = records
            .iter()
            .filter_map(|r| match &r.event {
                TraceEvent::ServiceProgress {
                    stream: ProgressKind::Artifact,
                    chunk,
                    ..
                } => Some(serde_json::from_str(chunk).expect("artifact progress JSON")),
                _ => None,
            })
            .collect();
        // 2 valid submit_plan calls + 2 report_finding calls + 1 valid seal_turn call = 5 artifact events!
        assert_eq!(artifact_chunks.len(), 5, "artifact_chunks={artifact_chunks:?}");
    }

    #[tokio::test]
    async fn acp_turn_missing_required_artifact_routes_to_on_error() {
        let silent_acp_agent_py = r#"
import json, sys
def send_acp(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for raw in sys.stdin:
    req = json.loads(raw)
    m, rid = req.get("method"), req.get("id")
    if m == "initialize":
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif m == "session/new":
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "sess-2"}})
    elif m == "session/prompt":
        # Finishes turn WITHOUT calling required `submit_plan`!
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        break
"#;

        let src = format!(
            r#"
load("@starchart/acp", "acp_agent", "artifact_tool", "invoke_agent")

Ctx = record(err = str)
Plan = record(summary = str)

def record_err(ctx: Ctx, event: dict) -> Ctx:
    return Ctx(err = str(event.get("error", "")))

planner = acp_agent(
    id = "planner",
    command = "python3",
    args = ["-u", "-c", {py_script}],
)

S = state_space("planning", "done", "escalated")

chart = machine(
    id = "missing_artifact",
    initial = S.planning,
    context = Ctx(err = ""),
    states = {{
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = "Submit plan",
                tools = [
                    artifact_tool("submit_plan", schema = Plan, required = True),
                ],
                on_done = S.done,
                on_error = outcome(target = S.escalated, assign = record_err),
            ),
        ),
        S.done: final_state(),
        S.escalated: final_state(),
    }},
)
"#,
            py_script = serde_json::to_string(silent_acp_agent_py).unwrap(),
        );

        let chart = compile_starlark_str("missing_artifact.star", &src, None).expect("compile");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn");
        actor.wait_done().await.expect("wait_done");
        assert!(actor.is_in("escalated"));
        let final_ctx = actor.context().await.expect("context");
        let err_msg = final_ctx["err"].as_str().unwrap_or("").to_owned();
        assert!(
            err_msg.contains("submit_plan"),
            "expected missing `submit_plan` error in context, got: {err_msg}"
        );
        actor.stop().await;
    }

    #[test]
    fn nested_record_fields_in_machine_context_roundtrip_across_transitions_and_guards() {
        let src = r##"
PlanArtifact = record(
    markdown = str,
    files_to_modify = field(list[str], []),
)

ReviewVerdict = record(
    approved = bool,
    notes = str,
)

Finding = record(
    severity = str,
    message = str,
)

PipelineCtx = record(
    task = str,
    plan = field(PlanArtifact | None, None),
    agent1_review = field(ReviewVerdict | None, None),
    agent2_review = field(ReviewVerdict | None, None),
    agent2_findings = field(list[Finding], []),
)

def store_plan(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return PipelineCtx(
        task = ctx.task,
        plan = PlanArtifact(markdown = "# Plan\n- Update src/lib.rs", files_to_modify = ["src/lib.rs"]),
        agent1_review = None,
        agent2_review = None,
        agent2_findings = [],
    )

def store_reviews(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    # Verify dot-access on nested `ctx.plan` works after JSON roundtrip!
    note = "Reviewed: " + (ctx.plan.markdown.splitlines()[0] if ctx.plan != None else "")
    return PipelineCtx(
        task = ctx.task,
        plan = ctx.plan,
        agent1_review = ReviewVerdict(approved = True, notes = note),
        agent2_review = ReviewVerdict(approved = True, notes = "LGTM"),
        agent2_findings = [Finding(severity = "LOW", message = "nit")],
    )

def both_approved(ctx: PipelineCtx, event: dict) -> bool:
    return (
        ctx.agent1_review != None
        and ctx.agent1_review.approved
        and ctx.agent2_review != None
        and ctx.agent2_review.approved
        and len(ctx.agent2_findings) == 1
        and ctx.agent2_findings[0].severity == "LOW"
    )

S = state_space("planning", "reviewing", "implementing", "rejected")

chart = machine(
    id = "nested_records",
    initial = S.planning,
    context = PipelineCtx(task = "demo"),
    states = {
        S.planning: state(
            on = [transition(event = "PLAN_READY", target = S.reviewing, assign = store_plan)],
        ),
        S.reviewing: state(
            on = [
                internal_transition(event = "REVIEWS_IN", assign = store_reviews),
                transition(event = "DECIDE", target = S.implementing, guard = both_approved),
                transition(event = "DECIDE", target = S.rejected),
            ],
        ),
        S.implementing: final_state(),
        S.rejected: final_state(),
    },
)
"##;
        let chart = compile_starlark_str("nested_records.star", src, None).expect("compile");
        let mut runner =
            StarchartSyncRunner::start(&chart, HostCallbacks::new()).expect("runner");
        runner.send("PLAN_READY").expect("PLAN_READY");
        runner.send("REVIEWS_IN").expect("REVIEWS_IN");
        runner.send("DECIDE").expect("DECIDE");
        assert!(runner.is_in("implementing"));
        assert_eq!(
            runner.context()["agent1_review"]["notes"],
            json!("Reviewed: # Plan")
        );
    }

    #[tokio::test]
    async fn invoke_agent_state_context_and_update_and_artifact_helpers_work_end_to_end() {
        let mock_agent_py = r#"
import json, subprocess, sys

def send_acp(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def mcp_rpc(proc, req_id, method, params=None):
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode("utf-8"))
    proc.stdin.flush()
    return json.loads(proc.stdout.readline().decode("utf-8"))

mcp_proc = None
for raw in sys.stdin:
    req = json.loads(raw)
    m, rid = req.get("method"), req.get("id")
    if m == "initialize":
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif m == "session/new":
        srv = req["params"]["mcpServers"][0]
        mcp_proc = subprocess.Popen([srv["command"]] + srv["args"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        mcp_rpc(mcp_proc, 1, "initialize", {"protocolVersion": "2024-11-05"})
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "ctx-sess-1"}})
    elif m == "session/prompt":
        blocks = req["params"]["prompt"]
        assert len(blocks) == 2, f"expected [<context>, prompt], got {blocks}"
        assert "<context>" in blocks[0]["text"] and "oauth-pkce" in blocks[0]["text"], blocks
        res = mcp_rpc(mcp_proc, 2, "tools/call", {
            "name": "submit_review",
            "arguments": {"approved": True, "notes": "Verified from pushed context"}
        })
        assert res["result"]["isError"] is False, res
        f_res = mcp_rpc(mcp_proc, 3, "tools/call", {
            "name": "report_finding",
            "arguments": {"severity": "LOW", "message": "minor style note"}
        })
        assert f_res["result"]["isError"] is False, f_res
        mcp_proc.stdin.close()
        mcp_proc.wait(timeout=5)
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        break
"#;

        let src = format!(
            r#"
load("@starchart/agents", "ArtifactMode", "acp_agent", "artifact", "artifact_list", "artifact_tool", "invoke_agent")

ReviewVerdict = record(approved = bool, notes = str)
Finding = record(severity = str, message = str)

PipelineCtx = record(
    task = str,
    unchanged_counter = field(int, 42),
    agent1_review = field(ReviewVerdict | None, None),
    findings = field(list[Finding], []),
)

def select_ctx(ctx: PipelineCtx) -> dict:
    return {{"task": ctx.task}}

def save_review(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(
        ctx,
        agent1_review = artifact(event, "submit_review", ReviewVerdict),
        findings = artifact_list(event, "report_finding", Finding),
    )

def is_approved(ctx: PipelineCtx, event: dict) -> bool:
    return ctx.agent1_review != None and ctx.agent1_review.approved

agent1 = acp_agent(
    id = "agent1",
    command = "python3",
    args = ["-u", "-c", {py_script}],
)

S = state_space("reviewing", "done", "rejected")

chart = machine(
    id = "ctx_and_update",
    initial = S.reviewing,
    context = PipelineCtx(task = "oauth-pkce"),
    states = {{
        S.reviewing: state(
            invoke = invoke_agent(
                agent = agent1,
                context = select_ctx,
                prompt = "Review the task in `<context>`.",
                tools = [
                    artifact_tool("submit_review", schema = ReviewVerdict),
                    artifact_tool("report_finding", schema = Finding, mode = ArtifactMode.append, required = False),
                ],
                on_done = outcome(target = S.done, assign = save_review),
                on_error = S.rejected,
            ),
        ),
        S.done: final_state(),
        S.rejected: final_state(),
    }},
)
"#,
            py_script = serde_json::to_string(mock_agent_py).unwrap(),
        );

        let chart = compile_starlark_str("ctx_and_update.star", &src, None).expect("compile");
        let actor = StarchartActor::spawn_with_builtins(&chart).expect("spawn");
        actor.wait_done().await.expect("wait_done");
        assert!(actor.is_in("done"));
        let final_ctx = actor.context().await.expect("context");
        assert_eq!(final_ctx["unchanged_counter"], json!(42));
        assert_eq!(
            final_ctx["agent1_review"]["notes"],
            json!("Verified from pushed context")
        );
        assert_eq!(final_ctx["findings"][0]["severity"], json!("LOW"));
        actor.stop().await;
    }
}

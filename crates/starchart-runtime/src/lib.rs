//! Compiles [`CompiledChart`] into `xstate-statemachine` (`xstate-core` + `xstate-tokio`),
//! bridging pure frozen Starlark guards, delays & reducers with effectful Rust host callbacks.

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
use starchart_starlark::CompiledChart;
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
use xstate_statemachine::tokio::{Interpreter as AsyncInterpreter, Options as ActorOptions, spawn};

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
        let inv_name = inv.id.as_deref().unwrap_or(inv.src.as_str());
        lines.push(format!("{pad}{mid} : invoke / {}", inv.src));

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
        sb = sb.invoke(lower_invoke_to_builder(inv));
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

fn lower_invoke_to_builder(inv: &InvokeDef) -> BuilderInvoke {
    let mut bi = BuilderInvoke::new(&inv.src);
    if let Some(id) = &inv.id {
        bi = bi.id(id);
    }
    if !inv.input.is_null() {
        bi = bi.input(inv.input.clone());
    }
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
    let mut logic = MachineLogic::dynamic();
    let record_ctor = chart.context_record_type.clone();

    for (guard_name, frozen_guard) in &chart.guards {
        let guard_fn = frozen_guard.clone();
        let rec_ctor = record_ctor.clone();
        let gname = guard_name.clone();
        logic = logic.guard(guard_name.clone(), move |ctx, event, _params| {
            eval_starlark_guard(&gname, &guard_fn, rec_ctor.as_ref(), ctx, event)
        });
    }

    for (reducer_name, frozen_reducer) in &chart.reducers {
        let reducer_fn = frozen_reducer.clone();
        let rec_ctor = record_ctor.clone();
        let rname = reducer_name.clone();
        logic = logic.action(
            reducer_name.clone(),
            move |scope: &mut ActionScope<'_, DynContext>| {
                let next_ctx = eval_starlark_reducer(
                    &rname,
                    &reducer_fn,
                    rec_ctor.as_ref(),
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
        let dname = delay_name.clone();
        logic = logic.delay_fn(delay_name.clone(), move |ctx, event| {
            eval_starlark_delay(&dname, &delay_fn, rec_ctor.as_ref(), ctx, event)
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

    for (service_name, service_fn) in &callbacks.sync_services {
        let service_fn = Arc::clone(service_fn);
        let sname = service_name.clone();
        logic = logic.service(service_name.clone(), move |ctx, input| {
            service_fn(ctx, input)
                .map_err(|e| XStateError::Logic(format!("service `{sname}`: {e:#}")))
        });
    }

    for (service_name, service_fn) in &callbacks.async_services {
        let service_fn = Arc::clone(service_fn);
        let sname = service_name.clone();
        logic = logic.async_service(service_name.clone(), move |ctx, input| {
            let fut = service_fn(ctx, input);
            let sname = sname.clone();
            Box::pin(async move {
                fut.await
                    .map_err(|e| XStateError::Logic(format!("service `{sname}`: {e:#}")))
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
    ctx: &Value,
    event: &Event,
) -> Result<bool, XStateError> {
    Module::with_temp_heap(|module| {
        let ctx_val = alloc_context_on_heap(&module, record_ctor, ctx)
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
}

fn eval_starlark_reducer(
    reducer_name: &str,
    reducer_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    ctx: &Value,
    event: &Event,
) -> Result<Value, XStateError> {
    Module::with_temp_heap(|module| {
        let ctx_val = alloc_context_on_heap(&module, record_ctor, ctx)
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
}

fn eval_starlark_delay(
    delay_name: &str,
    delay_fn: &OwnedFrozenValue,
    record_ctor: Option<&OwnedFrozenValue>,
    ctx: &Value,
    event: &Event,
) -> Result<f64, XStateError> {
    Module::with_temp_heap(|module| {
        let ctx_val = alloc_context_on_heap(&module, record_ctor, ctx)
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
}

fn alloc_context_on_heap<'v>(
    module: &Module<'v>,
    record_ctor: Option<&OwnedFrozenValue>,
    ctx: &Value,
) -> Result<StarlarkVal<'v>, String> {
    if let (Some(ctor), Value::Object(map)) = (record_ctor, ctx) {
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

/// Synchronous statechart runner backed by `xstate_statemachine::SyncInterpreter`.
pub struct ChartSyncRunner {
    machine_id: String,
    sim_clock: Option<SimulatedClock>,
    inner: SyncInterpreter<DynContext>,
}

impl ChartSyncRunner {
    /// Compiles and starts a synchronous statechart interpreter with a deterministic simulated clock.
    pub fn start(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let logic = build_machine_logic(chart, &callbacks);
        let clock = SimulatedClock::new();
        let mut inner = SyncInterpreter::with_clock(machine, logic, Arc::new(clock.clone()));
        inner.start()?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            sim_clock: Some(clock),
            inner,
        })
    }

    /// Compiles and starts a synchronous statechart interpreter backed by wall-clock [`RealClock`].
    pub fn start_real_clock(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let logic = build_machine_logic(chart, &callbacks);
        let mut inner = SyncInterpreter::with_clock(machine, logic, Arc::new(RealClock::new()));
        inner.start()?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            sim_clock: None,
            inner,
        })
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
        self.inner
            .state()
            .map_or(&Value::Null, |active| active.output())
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
pub struct ChartActor {
    machine_id: String,
    inner: AsyncInterpreter<DynContext>,
}

impl ChartActor {
    /// Spawns the compiled statechart onto the current Tokio runtime.
    pub fn spawn(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let logic = build_machine_logic(chart, &callbacks);
        let inner = spawn(machine, logic, ActorOptions::default())?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            inner,
        })
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
        self.inner.view().output
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
            .join("../../examples/order_workflow.star");
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
}

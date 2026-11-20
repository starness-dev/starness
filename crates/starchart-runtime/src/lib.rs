//! Compiles [`CompiledChart`] into `xstate-statemachine` (`xstate-core` + `xstate-tokio`),
//! bridging pure frozen Starlark guards & reducers with effectful Rust host callbacks.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use starchart_ir::{GuardRef, MachineDef, StateId, StateKind, StateNodeDef, TransitionDef};
use starchart_starlark::CompiledChart;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, OwnedFrozenValue, Value as StarlarkVal};
use thiserror::Error;
use xstate_statemachine::core::builder::{
    Action as BuilderAction, Guard as BuilderGuard, MachineBuilder, StateBuilder,
    Transition as BuilderTransition,
};
use xstate_statemachine::core::export;
use xstate_statemachine::core::{
    ActionScope, DynContext, Error as XStateError, Event, Machine, MachineLogic, Status,
    SyncInterpreter,
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
type AsyncServiceHandler = Arc<
    dyn Fn(Value, Value) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Builder for registering effectful Rust actions, async services, and named host guards.
#[derive(Clone, Default)]
pub struct HostCallbacks {
    actions: HashMap<String, SyncActionHandler>,
    guards: HashMap<String, SyncGuardHandler>,
    async_services: HashMap<String, AsyncServiceHandler>,
    strict_actions: bool,
}

impl std::fmt::Debug for HostCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCallbacks")
            .field("actions", &self.actions.keys().collect::<Vec<_>>())
            .field("guards", &self.guards.keys().collect::<Vec<_>>())
            .field(
                "async_services",
                &self.async_services.keys().collect::<Vec<_>>(),
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

    /// Registers an asynchronous Tokio service factory `(ctx, input) -> Future<Result<Value>>`.
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
}

/// Compiles a pure [`MachineDef`] into an `xstate_core::Machine` using explicit `#<state_id>` node IDs.
pub fn compile_ir_to_machine(ir: &MachineDef) -> Result<Machine, ChartRuntimeError> {
    let mut builder = MachineBuilder::new(&ir.id)
        .initial(ir.initial.as_str())
        .context(ir.context.clone());

    for (state_id, node) in &ir.states {
        builder = add_root_child_state(builder, state_id, node);
    }

    builder
        .build()
        .map_err(|e| ChartRuntimeError::EngineCompile(e.to_string()))
}

/// Renders a clean Harel `stateDiagram-v2` Mermaid diagram from a [`CompiledChart`],
/// including guards, `entry`/`exit` actions, transition actions, `assign` reducers,
/// internal transitions, and compound/parallel hierarchy without a redundant root wrapper box.
pub fn render_mermaid(chart: &CompiledChart) -> Result<String, ChartRuntimeError> {
    // Ensure the IR still compiles cleanly through `xstate_core::Machine`.
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

    Ok(render_harel_mermaid(
        &chart.ir,
        &guard_labels,
        &reducer_labels,
    ))
}

/// Renders a Harel `stateDiagram-v2` Mermaid diagram directly from [`MachineDef`].
pub fn render_ir_mermaid(ir: &MachineDef) -> Result<String, ChartRuntimeError> {
    let _machine = compile_ir_to_machine(ir)?;
    Ok(render_harel_mermaid(ir, &HashMap::new(), &HashMap::new()))
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
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
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
    effects.extend(tr.actions.iter().cloned());

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

fn emit_mermaid_node(
    state_id: &StateId,
    node: &StateNodeDef,
    depth: usize,
    guard_labels: &HashMap<String, String>,
    reducer_labels: &HashMap<String, String>,
    lines: &mut Vec<String>,
    edges: &mut Vec<String>,
) {
    let pad = "    ".repeat(depth);
    let mid = mermaid_safe_id(state_id.as_str());

    // Emit alias only if the ID required sanitization.
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
                    lines,
                    edges,
                );
            }
            lines.push(format!("{pad}}}"));
        }
        StateKind::Atomic | StateKind::Final => {}
    }

    if !node.entry.is_empty() {
        lines.push(format!("{pad}{mid} : entry / {}", node.entry.join(", ")));
    }
    if !node.exit.is_empty() {
        lines.push(format!("{pad}{mid} : exit / {}", node.exit.join(", ")));
    }

    // Internal transitions (`target: None`) execute in-place without exiting the state;
    // display them inside the state's compartment, and emit target transitions as edges.
    for tr in &node.on {
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
    // Explicit node ID so any transition anywhere in the hierarchy can target `#<state_id>`.
    sb = sb.id(state_id.as_str());

    for entry_action in &node.entry {
        sb = sb.entry(BuilderAction::named(entry_action));
    }
    for exit_action in &node.exit {
        sb = sb.exit(BuilderAction::named(exit_action));
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
        StateKind::Atomic | StateKind::Final => {}
    }

    // Group transitions by event name while preserving branch order.
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

    sb
}

fn lower_transition_to_builder(tr: &TransitionDef) -> BuilderTransition {
    let mut bt = match &tr.target {
        Some(target) => BuilderTransition::to(format!("#{}", target.as_str())),
        None => BuilderTransition::targetless(),
    };

    if let Some(guard_ref) = &tr.guard {
        bt = bt.guard(lower_guard_to_builder(guard_ref));
    }

    if let Some(assign_ref) = &tr.assign {
        bt = bt.action(BuilderAction::named(assign_ref.runtime_name()));
    }

    for action_name in &tr.actions {
        bt = bt.action(BuilderAction::named(action_name));
    }

    bt
}

fn lower_guard_to_builder(guard: &GuardRef) -> BuilderGuard {
    match guard {
        GuardRef::Named(name) => BuilderGuard::named(name),
        GuardRef::StarlarkSlot(slot) => BuilderGuard::named(GuardRef::starlark_slot_name(*slot)),
        GuardRef::AllOf(items) => {
            BuilderGuard::and(items.iter().map(lower_guard_to_builder).collect::<Vec<_>>())
        }
        GuardRef::AnyOf(items) => {
            BuilderGuard::or(items.iter().map(lower_guard_to_builder).collect::<Vec<_>>())
        }
        GuardRef::Not(inner) => BuilderGuard::not(lower_guard_to_builder(inner)),
    }
}

/// Builds a `MachineLogic<DynContext>` merging frozen Starlark guards & reducers with Rust `HostCallbacks`.
pub fn build_machine_logic(
    chart: &CompiledChart,
    callbacks: &HostCallbacks,
) -> MachineLogic<DynContext> {
    let mut logic = MachineLogic::dynamic();
    let record_ctor = chart.context_record_type.clone();

    // 1. Bind pure frozen Starlark guards.
    for (guard_name, frozen_guard) in &chart.guards {
        let guard_fn = frozen_guard.clone();
        let rec_ctor = record_ctor.clone();
        let gname = guard_name.clone();
        logic = logic.guard(guard_name.clone(), move |ctx, event, _params| {
            eval_starlark_guard(&gname, &guard_fn, rec_ctor.as_ref(), ctx, event)
        });
    }

    // 2. Bind pure frozen Starlark `assign` context reducers.
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

    // 3. Bind named host guards from Rust.
    for (guard_name, handler) in &callbacks.guards {
        let handler = Arc::clone(handler);
        let gname = guard_name.clone();
        logic = logic.guard(guard_name.clone(), move |ctx, event, _params| {
            let ce = ChartEvent::from_xstate(event);
            handler(ctx, &ce).map_err(|e| XStateError::Logic(format!("guard `{gname}`: {e:#}")))
        });
    }

    // 4. Bind explicit Rust host actions.
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

    // 5. Default unregistered declared actions to no-ops unless `strict_actions` is enabled.
    if !callbacks.strict_actions {
        let mut declared_actions = HashSet::new();
        collect_declared_actions(&chart.ir.states, &mut declared_actions);
        for action_name in declared_actions {
            if !registered_actions.contains(&action_name) {
                logic = logic.action(action_name, |_scope| Ok(()));
            }
        }
    }

    // 6. Bind async Tokio services.
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

    logic
}

fn collect_declared_actions(
    states: &IndexMap<StateId, StateNodeDef>,
    out: &mut HashSet<String>,
) {
    for node in states.values() {
        out.extend(node.entry.iter().cloned());
        out.extend(node.exit.iter().cloned());
        for tr in &node.on {
            out.extend(tr.actions.iter().cloned());
        }
        match &node.kind {
            StateKind::Compound { states: children, .. }
            | StateKind::Parallel { states: children } => {
                collect_declared_actions(children, out);
            }
            StateKind::Atomic | StateKind::Final => {}
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
    inner: SyncInterpreter<DynContext>,
}

impl ChartSyncRunner {
    /// Compiles and starts a synchronous statechart interpreter.
    pub fn start(
        chart: &CompiledChart,
        callbacks: HostCallbacks,
    ) -> Result<Self, ChartRuntimeError> {
        let machine = compile_ir_to_machine(&chart.ir)?;
        let logic = build_machine_logic(chart, &callbacks);
        let mut inner = SyncInterpreter::new(machine, logic);
        inner.start()?;
        Ok(Self {
            machine_id: chart.ir.id.clone(),
            inner,
        })
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

    /// Returns the active leaf state IDs (e.g. `["order_workflow.processing.charging"]` or custom IDs).
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

    /// Checks whether `state` (e.g. `"validating"`, `"processing.charging"`, or `"charging"`) is currently active.
    pub fn is_in(&self, state: &str) -> bool {
        if self.inner.matches(state) || self.inner.matches(&format!("{}.{state}", self.machine_id))
        {
            return true;
        }
        state_matches_query(&self.machine_id, &self.inner.state_ids(), state)
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

    /// Fetches a snapshot of the actor's current extended state context.
    pub async fn context(&self) -> Result<Value, ChartRuntimeError> {
        let snap = self.inner.snapshot().await?;
        Ok(snap.context)
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

    use starchart_starlark::{compile_starlark_file, compile_starlark_str};
    use serde_json::json;

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
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Strongly typed state identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StateId(pub String);

impl StateId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for StateId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for StateId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for StateId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Reference to a transition guard predicate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GuardRef {
    /// Named host-provided guard.
    Named(String),
    /// Index into the compiled Starlark module's frozen guard callable table.
    StarlarkSlot(usize),
    /// Built-in Harel `In(state)` predicate checking whether another state is currently active.
    StateIn(StateId),
    /// Logical AND over child guards (short-circuiting).
    AllOf(Vec<GuardRef>),
    /// Logical OR over child guards (short-circuiting).
    AnyOf(Vec<GuardRef>),
    /// Logical negation of a child guard.
    Not(Box<GuardRef>),
}

impl GuardRef {
    /// Deterministic generated name for a Starlark guard slot.
    pub fn starlark_slot_name(slot: usize) -> String {
        format!("__starlark_guard_{slot}")
    }

    /// Deterministic name used to bind this guard expression in the runtime logic table.
    pub fn runtime_name(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::StarlarkSlot(slot) => Self::starlark_slot_name(*slot),
            Self::StateIn(state_id) => format!("stateIn(#{state_id})"),
            Self::AllOf(children) => {
                let inner: Vec<String> = children.iter().map(Self::runtime_name).collect();
                format!("__guard_all_of({})", inner.join(","))
            }
            Self::AnyOf(children) => {
                let inner: Vec<String> = children.iter().map(Self::runtime_name).collect();
                format!("__guard_any_of({})", inner.join(","))
            }
            Self::Not(inner) => format!("__guard_not({})", inner.runtime_name()),
        }
    }
}

/// Reference to a pure `(ctx, event) -> ctx` Starlark context reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssignRef {
    /// Index into the compiled Starlark module's frozen reducer callable table.
    StarlarkSlot(usize),
}

impl AssignRef {
    /// Deterministic generated name for a Starlark assign reducer slot.
    pub fn starlark_slot_name(slot: usize) -> String {
        format!("__starlark_assign_{slot}")
    }

    /// Name used to bind this assign action in the runtime logic table.
    pub fn runtime_name(&self) -> String {
        match self {
            Self::StarlarkSlot(slot) => Self::starlark_slot_name(*slot),
        }
    }
}

/// Reference to a transition timer delay (`after` or delayed `raise` / `sendTo`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DelayRef {
    /// Fixed duration in milliseconds.
    FixedMs(u64),
    /// Named host-configured delay.
    Named(String),
    /// Index into the compiled Starlark module's frozen delay callable table `(ctx, event) -> ms`.
    StarlarkSlot(usize),
}

impl DelayRef {
    /// Deterministic generated name for a Starlark delay callable slot.
    pub fn starlark_slot_name(slot: usize) -> String {
        format!("__starlark_delay_{slot}")
    }
}

/// Pure IR representation of an action: either a named host callback or an XState built-in action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ActionDef {
    /// Host-registered named action (e.g. `"CHARGE_CARD"`).
    Named(String),
    /// Queues an internal event onto the machine's internal queue (optionally delayed / cancellable).
    Raise {
        event: String,
        payload: serde_json::Value,
        delay: Option<DelayRef>,
        id: Option<String>,
    },
    /// Cancels a pending delayed `raise_event` or `send_to` by its `id`.
    Cancel { send_id: String },
    /// Emits an external event to observers / `take_emitted()`.
    Emit {
        event: String,
        payload: serde_json::Value,
    },
    /// Sends an event to another actor (`"parent"`, `"self"`, child id, or system id).
    SendTo {
        to: String,
        event: String,
        payload: serde_json::Value,
        delay: Option<DelayRef>,
        id: Option<String>,
    },
    /// Sends an event to the parent actor when running as an invoked child machine.
    SendParent {
        event: String,
        payload: serde_json::Value,
    },
    /// Appends a static JSON value to the interpreter's log buffer (`take_logs()`).
    Log { expr: serde_json::Value },
}

impl ActionDef {
    /// Short label for diagrams.
    pub fn diagram_label(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::Raise { event, .. } => format!("raise {event}"),
            Self::Cancel { send_id } => format!("cancel {send_id}"),
            Self::Emit { event, .. } => format!("emit {event}"),
            Self::SendTo { to, event, .. } => format!("sendTo {to} {event}"),
            Self::SendParent { event, .. } => format!("sendParent {event}"),
            Self::Log { .. } => "log".to_owned(),
        }
    }
}

impl From<&str> for ActionDef {
    fn from(value: &str) -> Self {
        Self::Named(value.to_owned())
    }
}

impl From<String> for ActionDef {
    fn from(value: String) -> Self {
        Self::Named(value)
    }
}

/// Transition definition attached to a state node (`on`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionDef {
    pub event: String,
    /// `None` indicates an internal self-transition without exiting/re-entering the state.
    pub target: Option<StateId>,
    pub guard: Option<GuardRef>,
    pub actions: Vec<ActionDef>,
    pub assign: Option<AssignRef>,
    /// When `true`, forces exiting and re-entering the source state on self/descendant transitions.
    #[serde(default)]
    pub reenter: bool,
    /// When `true`, consumes `event` and blocks ancestor handlers (`forbid(event = ...)`).
    #[serde(default)]
    pub forbidden: bool,
    #[serde(default)]
    pub description: Option<String>,
}

/// Transition branch used by `always`, `after`, `on_done`, and `invoke` (`on_done` / `on_error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeDef {
    /// `None` executes actions/reducers without exiting the state.
    pub target: Option<StateId>,
    pub guard: Option<GuardRef>,
    pub actions: Vec<ActionDef>,
    pub assign: Option<AssignRef>,
    #[serde(default)]
    pub reenter: bool,
    #[serde(default)]
    pub description: Option<String>,
}

/// Delayed (`after`) timer transition attached to a state node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AfterDef {
    pub delay: DelayRef,
    pub outcomes: Vec<OutcomeDef>,
}

/// Service or child actor invoked on entry to a state and cancelled on exit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeDef {
    /// Optional invocation identifier (defaults to `src` / owning state id).
    pub id: Option<String>,
    /// Registered host service name.
    pub src: String,
    /// Static JSON input passed to the service (`Null` when omitted).
    pub input: serde_json::Value,
    /// Candidate transitions evaluated when the service resolves (`event["output"]`).
    pub on_done: Vec<OutcomeDef>,
    /// Candidate transitions evaluated when the service fails (`event["error"]`).
    pub on_error: Vec<OutcomeDef>,
}

/// Structural classification of a state node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateKind {
    Atomic,
    Compound {
        initial: StateId,
        states: IndexMap<StateId, StateNodeDef>,
    },
    Parallel {
        states: IndexMap<StateId, StateNodeDef>,
    },
    Final,
    History {
        deep: bool,
        default_target: Option<StateId>,
    },
}

/// Definition of a single state node in the hierarchy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateNodeDef {
    pub kind: StateKind,
    pub entry: Vec<ActionDef>,
    pub exit: Vec<ActionDef>,
    pub on: Vec<TransitionDef>,
    #[serde(default)]
    pub always: Vec<OutcomeDef>,
    #[serde(default)]
    pub after: Vec<AfterDef>,
    #[serde(default)]
    pub invoke: Vec<InvokeDef>,
    /// Completion transitions for `Compound` (child reached `Final`) or `Parallel` (all regions reached `Final`).
    #[serde(default)]
    pub on_done: Vec<OutcomeDef>,
    /// Output payload produced when a `Final` state is entered.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    /// State tags (e.g. `["loading", "modal"]`) queryable at runtime.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Free-form metadata JSON object.
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

impl StateNodeDef {
    pub fn atomic() -> Self {
        Self {
            kind: StateKind::Atomic,
            entry: Vec::new(),
            exit: Vec::new(),
            on: Vec::new(),
            always: Vec::new(),
            after: Vec::new(),
            invoke: Vec::new(),
            on_done: Vec::new(),
            output: None,
            tags: Vec::new(),
            meta: None,
            description: None,
        }
    }

    pub fn final_state() -> Self {
        Self {
            kind: StateKind::Final,
            entry: Vec::new(),
            exit: Vec::new(),
            on: Vec::new(),
            always: Vec::new(),
            after: Vec::new(),
            invoke: Vec::new(),
            on_done: Vec::new(),
            output: None,
            tags: Vec::new(),
            meta: None,
            description: None,
        }
    }
}

/// Complete, pure-Rust intermediate representation of a Harel statechart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineDef {
    pub id: String,
    pub initial: StateId,
    pub context: serde_json::Value,
    pub states: IndexMap<StateId, StateNodeDef>,
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Graph and structural integrity errors reported by [`MachineDef::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IrValidationError {
    #[error("duplicate state id `{state}` declared in machine `{machine}`")]
    DuplicateStateId { machine: String, state: StateId },

    #[error("compound state `{state}` must declare at least one non-history child state")]
    EmptyCompoundState { state: StateId },

    #[error("parallel state `{state}` must declare at least one region state")]
    EmptyParallelState { state: StateId },

    #[error("{}", format_missing_initial(.parent.as_ref(), .initial))]
    MissingInitialState {
        parent: Option<StateId>,
        initial: StateId,
    },

    #[error("final state `{state}` cannot declare outgoing transitions")]
    FinalStateHasTransitions { state: StateId },

    #[error("final state `{state}` cannot declare `invoke` services")]
    FinalStateHasInvocations { state: StateId },

    #[error("history pseudo-state `{state}` must be nested inside a compound state")]
    InvalidHistoryPlacement { state: StateId },

    #[error("history pseudo-state `{state}` cannot declare entry/exit actions, transitions, or invocations")]
    HistoryStateHasBehavior { state: StateId },

    #[error("state `{state}` declares `on_done` but is neither a compound nor parallel state")]
    InvalidOnDoneState { state: StateId },

    #[error("transition on `{event}` from state `{from}` targets undeclared state `{target}`")]
    UnknownTargetState {
        from: StateId,
        event: String,
        target: StateId,
    },

    #[error("`state_in({target})` guard in state `{from}` references undeclared state `{target}`")]
    UnknownStateInGuard { from: StateId, target: StateId },

    #[error("state `{state}` is unreachable from initial state `{initial}`")]
    UnreachableState { state: StateId, initial: StateId },

    #[error("secret descriptor at `{path}` cannot be stored in machine `context` (pass `secret(...)` only to outbound service inputs)")]
    SecretInContext { path: String },

    #[error("UI spec on state `{state}` references unhandled or forbidden event `{event}` (allowed: {allowed})")]
    UnknownUiEvent {
        state: StateId,
        event: String,
        allowed: String,
    },
}

/// JSON object key marking a late-bound `@starness/secrets` descriptor (`{"$starness_secret": {...}}`).
pub const SECRET_MARKER_KEY: &str = "$starness_secret";

/// Recursively inspects `val` for any object containing [`SECRET_MARKER_KEY`], returning its JSON path.
pub fn find_secret_marker_path(val: &serde_json::Value) -> Option<String> {
    find_secret_marker_inner(val, "context".to_owned())
}

fn find_secret_marker_inner(val: &serde_json::Value, current_path: String) -> Option<String> {
    match val {
        serde_json::Value::Object(map) => {
            if map.contains_key(SECRET_MARKER_KEY) {
                return Some(current_path);
            }
            for (k, child) in map {
                if let Some(found) = find_secret_marker_inner(child, format!("{current_path}.{k}")) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for (idx, item) in items.iter().enumerate() {
                if let Some(found) =
                    find_secret_marker_inner(item, format!("{current_path}[{idx}]"))
                {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn format_missing_initial(parent: Option<&StateId>, initial: &StateId) -> String {
    match parent {
        Some(p) => format!(
            "compound state `{p}` initial state `{initial}` does not exist in its immediate `states`"
        ),
        None => format!("machine root initial state `{initial}` does not exist in root `states`"),
    }
}

struct NodeRecord<'a> {
    node: &'a StateNodeDef,
    parent: Option<StateId>,
}

impl MachineDef {
    /// Validates graph integrity of the statechart:
    /// - Unique `StateId`s across the entire state hierarchy
    /// - Valid root and compound `initial` state references
    /// - Non-empty compound and parallel child state maps
    /// - Constraints on `Final` and `History` states + `on_done` placement
    /// - All transition, `always`, `after`, `invoke`, `on_done`, `History.default_target`,
    ///   and `GuardRef::StateIn` references resolve to declared `StateId`s
    /// - Full reachability from root `initial` (excluding passive `History` pseudo-states
    ///   only when never targeted? No—if a `History` pseudo-state is declared in a machine
    ///   and NO transition ever targets it, reporting `UnreachableState` catches unused dead history nodes too!)
    pub fn validate(&self) -> Result<(), Vec<IrValidationError>> {
        let mut errors = Vec::new();
        let mut registry: IndexMap<StateId, NodeRecord<'_>> = IndexMap::new();
        let mut has_duplicates = false;

        if let Some(path) = find_secret_marker_path(&self.context) {
            errors.push(IrValidationError::SecretInContext { path });
        }

        Self::collect_states(
            &self.id,
            None,
            &self.states,
            &mut registry,
            &mut errors,
            &mut has_duplicates,
        );

        let root_initial_valid = self.states.contains_key(&self.initial);
        if !root_initial_valid {
            errors.push(IrValidationError::MissingInitialState {
                parent: None,
                initial: self.initial.clone(),
            });
        }

        // Validate node-local constraints, targets, and `state_in` guards in declaration order.
        for (state_id, record) in &registry {
            match &record.node.kind {
                StateKind::Compound { initial, states } => {
                    let has_non_history = states
                        .values()
                        .any(|child| !matches!(child.kind, StateKind::History { .. }));
                    if !has_non_history {
                        errors.push(IrValidationError::EmptyCompoundState {
                            state: state_id.clone(),
                        });
                    }
                    if !states.contains_key(initial) {
                        errors.push(IrValidationError::MissingInitialState {
                            parent: Some(state_id.clone()),
                            initial: initial.clone(),
                        });
                    }
                }
                StateKind::Parallel { states } => {
                    if states.is_empty() {
                        errors.push(IrValidationError::EmptyParallelState {
                            state: state_id.clone(),
                        });
                    }
                    if !record.node.on_done.is_empty() {
                        // Valid on Parallel.
                    }
                }
                StateKind::Final => {
                    if !record.node.on.is_empty()
                        || !record.node.always.is_empty()
                        || !record.node.after.is_empty()
                        || !record.node.on_done.is_empty()
                    {
                        errors.push(IrValidationError::FinalStateHasTransitions {
                            state: state_id.clone(),
                        });
                    }
                    if !record.node.invoke.is_empty() {
                        errors.push(IrValidationError::FinalStateHasInvocations {
                            state: state_id.clone(),
                        });
                    }
                }
                StateKind::History { default_target, .. } => {
                    let valid_parent = record
                        .parent
                        .as_ref()
                        .and_then(|p| registry.get(p))
                        .is_some_and(|prec| matches!(prec.node.kind, StateKind::Compound { .. }));
                    if !valid_parent {
                        errors.push(IrValidationError::InvalidHistoryPlacement {
                            state: state_id.clone(),
                        });
                    }
                    if !record.node.entry.is_empty()
                        || !record.node.exit.is_empty()
                        || !record.node.on.is_empty()
                        || !record.node.always.is_empty()
                        || !record.node.after.is_empty()
                        || !record.node.invoke.is_empty()
                        || !record.node.on_done.is_empty()
                    {
                        errors.push(IrValidationError::HistoryStateHasBehavior {
                            state: state_id.clone(),
                        });
                    }
                    if let Some(target) = default_target
                        && !registry.contains_key(target)
                    {
                        errors.push(IrValidationError::UnknownTargetState {
                            from: state_id.clone(),
                            event: "history.target".to_owned(),
                            target: target.clone(),
                        });
                    }
                }
                StateKind::Atomic => {
                    if !record.node.on_done.is_empty() {
                        errors.push(IrValidationError::InvalidOnDoneState {
                            state: state_id.clone(),
                        });
                    }
                }
            }

            for transition in &record.node.on {
                if let Some(target) = &transition.target
                    && !registry.contains_key(target)
                {
                    errors.push(IrValidationError::UnknownTargetState {
                        from: state_id.clone(),
                        event: transition.event.clone(),
                        target: target.clone(),
                    });
                }
                if let Some(g) = &transition.guard {
                    Self::validate_guard_refs(state_id, g, &registry, &mut errors);
                }
            }

            for (label, outcomes) in [
                ("always", record.node.always.as_slice()),
                ("on_done", record.node.on_done.as_slice()),
            ] {
                for outcome in outcomes {
                    if let Some(target) = &outcome.target
                        && !registry.contains_key(target)
                    {
                        errors.push(IrValidationError::UnknownTargetState {
                            from: state_id.clone(),
                            event: label.to_owned(),
                            target: target.clone(),
                        });
                    }
                    if let Some(g) = &outcome.guard {
                        Self::validate_guard_refs(state_id, g, &registry, &mut errors);
                    }
                }
            }

            for after_def in &record.node.after {
                for outcome in &after_def.outcomes {
                    if let Some(target) = &outcome.target
                        && !registry.contains_key(target)
                    {
                        errors.push(IrValidationError::UnknownTargetState {
                            from: state_id.clone(),
                            event: "after".to_owned(),
                            target: target.clone(),
                        });
                    }
                    if let Some(g) = &outcome.guard {
                        Self::validate_guard_refs(state_id, g, &registry, &mut errors);
                    }
                }
            }

            for inv in &record.node.invoke {
                for (suffix, outcomes) in [
                    ("on_done", inv.on_done.as_slice()),
                    ("on_error", inv.on_error.as_slice()),
                ] {
                    for outcome in outcomes {
                        if let Some(target) = &outcome.target
                            && !registry.contains_key(target)
                        {
                            errors.push(IrValidationError::UnknownTargetState {
                                from: state_id.clone(),
                                event: format!("invoke({}).{suffix}", inv.src),
                                target: target.clone(),
                            });
                        }
                        if let Some(g) = &outcome.guard {
                            Self::validate_guard_refs(state_id, g, &registry, &mut errors);
                        }
                    }
                }
            }

            if let Some(ui_val) = record.node.meta.as_ref().and_then(|m| m.get("ui")) {
                let ui_events = collect_ui_spec_events(ui_val);
                if !ui_events.is_empty() {
                    let allowed = Self::allowed_events_from_registry(state_id, &registry);
                    for ev in ui_events {
                        if !allowed.iter().any(|a| a == &ev) {
                            let allowed_str = if allowed.is_empty() {
                                "(none)".to_owned()
                            } else {
                                allowed.join(", ")
                            };
                            errors.push(IrValidationError::UnknownUiEvent {
                                state: state_id.clone(),
                                event: ev,
                                allowed: allowed_str,
                            });
                        }
                    }
                }
            }
        }

        if root_initial_valid && !has_duplicates {
            let reachable = self.compute_reachable_states(&registry);
            for state_id in registry.keys() {
                if !reachable.contains(state_id) {
                    errors.push(IrValidationError::UnreachableState {
                        state: state_id.clone(),
                        initial: self.initial.clone(),
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_guard_refs(
        from: &StateId,
        guard: &GuardRef,
        registry: &IndexMap<StateId, NodeRecord<'_>>,
        errors: &mut Vec<IrValidationError>,
    ) {
        match guard {
            GuardRef::StateIn(target) => {
                if !registry.contains_key(target) {
                    errors.push(IrValidationError::UnknownStateInGuard {
                        from: from.clone(),
                        target: target.clone(),
                    });
                }
            }
            GuardRef::AllOf(items) | GuardRef::AnyOf(items) => {
                for item in items {
                    Self::validate_guard_refs(from, item, registry, errors);
                }
            }
            GuardRef::Not(inner) => Self::validate_guard_refs(from, inner, registry, errors),
            GuardRef::Named(_) | GuardRef::StarlarkSlot(_) => {}
        }
    }

    fn collect_states<'a>(
        machine_id: &str,
        parent: Option<&StateId>,
        states: &'a IndexMap<StateId, StateNodeDef>,
        registry: &mut IndexMap<StateId, NodeRecord<'a>>,
        errors: &mut Vec<IrValidationError>,
        has_duplicates: &mut bool,
    ) {
        for (state_id, node) in states {
            if registry.contains_key(state_id) {
                *has_duplicates = true;
                errors.push(IrValidationError::DuplicateStateId {
                    machine: machine_id.to_owned(),
                    state: state_id.clone(),
                });
            } else {
                registry.insert(
                    state_id.clone(),
                    NodeRecord {
                        node,
                        parent: parent.cloned(),
                    },
                );
            }

            match &node.kind {
                StateKind::Compound {
                    states: children, ..
                }
                | StateKind::Parallel { states: children } => {
                    Self::collect_states(
                        machine_id,
                        Some(state_id),
                        children,
                        registry,
                        errors,
                        has_duplicates,
                    );
                }
                StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
            }
        }
    }

    fn compute_reachable_states(
        &self,
        registry: &IndexMap<StateId, NodeRecord<'_>>,
    ) -> HashSet<StateId> {
        let mut active: HashSet<StateId> = HashSet::new();
        let mut default_entered: HashSet<StateId> = HashSet::new();

        #[derive(Debug, Clone)]
        enum WorkItem {
            EnterDefault(StateId),
            Activate(StateId),
        }

        let mut queue: VecDeque<WorkItem> = VecDeque::new();
        queue.push_back(WorkItem::EnterDefault(self.initial.clone()));

        let mut parallel_regions: HashMap<&StateId, Vec<&StateId>> = HashMap::new();
        for (id, rec) in registry {
            if let StateKind::Parallel { states } = &rec.node.kind {
                parallel_regions.insert(
                    id,
                    states
                        .iter()
                        .filter_map(|(cid, cnode)| {
                            (!matches!(cnode.kind, StateKind::History { .. })).then_some(cid)
                        })
                        .collect(),
                );
            }
        }

        while let Some(item) = queue.pop_front() {
            match item {
                WorkItem::EnterDefault(state_id) => {
                    let Some(rec) = registry.get(&state_id) else {
                        continue;
                    };
                    queue.push_back(WorkItem::Activate(state_id.clone()));
                    if default_entered.insert(state_id.clone()) {
                        match &rec.node.kind {
                            StateKind::Compound { initial, states } => {
                                if states.contains_key(initial) {
                                    queue.push_back(WorkItem::EnterDefault(initial.clone()));
                                }
                            }
                            StateKind::Parallel { states } => {
                                for (region_id, region_node) in states {
                                    if !matches!(region_node.kind, StateKind::History { .. }) {
                                        queue.push_back(WorkItem::EnterDefault(region_id.clone()));
                                    }
                                }
                            }
                            StateKind::History { default_target, .. } => {
                                if let Some(target) = default_target
                                    && registry.contains_key(target)
                                {
                                    queue.push_back(WorkItem::EnterDefault(target.clone()));
                                } else if let Some(parent_id) = &rec.parent
                                    && let Some(parent_rec) = registry.get(parent_id)
                                    && let StateKind::Compound { initial, states } =
                                        &parent_rec.node.kind
                                    && states.contains_key(initial)
                                {
                                    queue.push_back(WorkItem::EnterDefault(initial.clone()));
                                }
                            }
                            StateKind::Atomic | StateKind::Final => {}
                        }
                    }
                }
                WorkItem::Activate(state_id) => {
                    let Some(rec) = registry.get(&state_id) else {
                        continue;
                    };
                    if active.insert(state_id.clone()) {
                        if let Some(parent_id) = &rec.parent {
                            queue.push_back(WorkItem::Activate(parent_id.clone()));
                            if let Some(regions) = parallel_regions.get(parent_id) {
                                for &region_id in regions {
                                    if region_id != &state_id {
                                        queue.push_back(WorkItem::EnterDefault(region_id.clone()));
                                    }
                                }
                            }
                        }

                        for tr in &rec.node.on {
                            if !tr.forbidden
                                && let Some(target) = &tr.target
                                && registry.contains_key(target)
                            {
                                queue.push_back(WorkItem::EnterDefault(target.clone()));
                            }
                        }

                        for outcome in rec.node.always.iter().chain(rec.node.on_done.iter()) {
                            if let Some(target) = &outcome.target
                                && registry.contains_key(target)
                            {
                                queue.push_back(WorkItem::EnterDefault(target.clone()));
                            }
                        }

                        for after_def in &rec.node.after {
                            for outcome in &after_def.outcomes {
                                if let Some(target) = &outcome.target
                                    && registry.contains_key(target)
                                {
                                    queue.push_back(WorkItem::EnterDefault(target.clone()));
                                }
                            }
                        }

                        for inv in &rec.node.invoke {
                            for outcome in inv.on_done.iter().chain(inv.on_error.iter()) {
                                if let Some(target) = &outcome.target
                                    && registry.contains_key(target)
                                {
                                    queue.push_back(WorkItem::EnterDefault(target.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }

        active
    }

    fn build_registry(&self) -> IndexMap<StateId, NodeRecord<'_>> {
        let mut registry = IndexMap::new();
        let mut errors = Vec::new();
        let mut has_dups = false;
        Self::collect_states(
            &self.id,
            None,
            &self.states,
            &mut registry,
            &mut errors,
            &mut has_dups,
        );
        registry
    }

    fn allowed_events_from_registry(
        state_id: &StateId,
        registry: &IndexMap<StateId, NodeRecord<'_>>,
    ) -> Vec<String> {
        let mut forbidden: HashSet<String> = HashSet::new();
        let mut allowed: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(state_id.clone());

        while let Some(id) = cursor {
            let Some(rec) = registry.get(&id) else {
                break;
            };
            for tr in &rec.node.on {
                if tr.forbidden {
                    forbidden.insert(tr.event.clone());
                }
            }
            for tr in &rec.node.on {
                if !tr.forbidden
                    && !forbidden.contains(&tr.event)
                    && seen.insert(tr.event.clone())
                {
                    allowed.push(tr.event.clone());
                }
            }
            cursor = rec.parent.clone();
        }
        allowed
    }

    /// Returns deduplicated event names handled by `state_id` or its ancestors (excluding `forbid`s).
    pub fn allowed_events_for_state(&self, state_id: &StateId) -> Vec<String> {
        let registry = self.build_registry();
        Self::allowed_events_from_registry(state_id, &registry)
    }

    /// Returns deduplicated outgoing event names handled by any of `active_states` (or their ancestors),
    /// respecting `forbid` transitions. Accepts dotted XState paths (`"working.thinking"`) or leaf IDs.
    pub fn allowed_events_for_active_states(&self, active_states: &[String]) -> Vec<String> {
        let registry = self.build_registry();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for active in active_states {
            let leaf = active.rsplit('.').next().unwrap_or(active.as_str());
            let sid = StateId::from(leaf);
            for ev in Self::allowed_events_from_registry(&sid, &registry) {
                if seen.insert(ev.clone()) {
                    out.push(ev);
                }
            }
        }
        out
    }

    /// Resolves the active `json-render` UI spec for `active_states` (checking each active leaf and
    /// its ancestors for `meta["ui"]`). If `meta["ui"]` is a `{"$state": "/pointer"}` binding,
    /// resolves the RFC 6901 JSON Pointer against `context`.
    pub fn resolve_ui_for_active_states(
        &self,
        active_states: &[String],
        context: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let registry = self.build_registry();
        for active in active_states {
            let leaf = active.rsplit('.').next().unwrap_or(active.as_str());
            let mut cursor = Some(StateId::from(leaf));
            while let Some(id) = cursor {
                let Some(rec) = registry.get(&id) else {
                    break;
                };
                if let Some(ui_val) = rec.node.meta.as_ref().and_then(|m| m.get("ui"))
                    && let Some(resolved) = resolve_ui_spec_value(ui_val, context)
                {
                    return Some(resolved);
                }
                cursor = rec.parent.clone();
            }
        }
        None
    }
}

/// Resolves a `meta["ui"]` entry: either a static `{"root": ..., "elements": ...}` spec or a
/// dynamic `{"$state": "/pointer"}` RFC 6901 pointer into `context`.
pub fn resolve_ui_spec_value(
    ui_val: &serde_json::Value,
    context: &serde_json::Value,
) -> Option<serde_json::Value> {
    let obj = ui_val.as_object()?;
    if let Some(ptr) = obj.get("$state").and_then(serde_json::Value::as_str) {
        let target = context.pointer(ptr)?;
        if target.is_object()
            && target.get("root").is_some()
            && target.get("elements").is_some()
        {
            return Some(target.clone());
        }
        return None;
    }
    if obj.get("root").is_some() && obj.get("elements").is_some() {
        return Some(ui_val.clone());
    }
    None
}

/// Collects all static event names declared by interactive elements inside a `json-render` UI spec.
pub fn collect_ui_spec_events(ui_val: &serde_json::Value) -> Vec<String> {
    let mut events = Vec::new();
    let mut seen = HashSet::new();
    let Some(elements) = ui_val.get("elements").and_then(serde_json::Value::as_object) else {
        return events;
    };

    let mut push_event = |raw: &str| {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_owned()) {
            events.push(trimmed.to_owned());
        }
    };

    for elem in elements.values() {
        if let Some(props) = elem.get("props").and_then(serde_json::Value::as_object) {
            for key in [
                "event",
                "approve_event",
                "revise_event",
                "confirm_event",
                "cancel_event",
            ] {
                if let Some(ev) = props.get(key).and_then(serde_json::Value::as_str) {
                    push_event(ev);
                }
            }
        }
        if let Some(on_map) = elem.get("on").and_then(serde_json::Value::as_object) {
            for handler in on_map.values() {
                if let Some(ev) = handler
                    .get("params")
                    .and_then(|p| p.get("event"))
                    .and_then(serde_json::Value::as_str)
                {
                    push_event(ev);
                }
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plain_tr(
        event: &str,
        target: Option<&str>,
        guard: Option<GuardRef>,
        actions: Vec<ActionDef>,
        assign: Option<AssignRef>,
    ) -> TransitionDef {
        TransitionDef {
            event: event.to_owned(),
            target: target.map(StateId::from),
            guard,
            actions,
            assign,
            reenter: false,
            forbidden: false,
            description: None,
        }
    }

    fn sample_order_workflow() -> MachineDef {
        let mut processing_children = IndexMap::new();
        processing_children.insert(
            StateId::from("charging"),
            StateNodeDef {
                kind: StateKind::Atomic,
                entry: vec!["CHARGE_CARD".into()],
                exit: vec![],
                on: vec![
                    plain_tr("CHARGE_OK", Some("fulfilled"), None, vec![], None),
                    plain_tr(
                        "CHARGE_ERR",
                        Some("failed"),
                        None,
                        vec![],
                        Some(AssignRef::StarlarkSlot(0)),
                    ),
                ],
                ..StateNodeDef::atomic()
            },
        );

        let mut states = IndexMap::new();
        states.insert(
            StateId::from("validating"),
            StateNodeDef {
                kind: StateKind::Atomic,
                on: vec![
                    plain_tr(
                        "APPROVED",
                        Some("processing"),
                        Some(GuardRef::StarlarkSlot(0)),
                        vec![],
                        None,
                    ),
                    plain_tr(
                        "REJECTED",
                        Some("rejected"),
                        None,
                        vec!["LOG_REJECTION".into()],
                        None,
                    ),
                ],
                ..StateNodeDef::atomic()
            },
        );
        states.insert(
            StateId::from("processing"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("charging"),
                    states: processing_children,
                },
                on: vec![plain_tr("CANCEL", Some("rejected"), None, vec![], None)],
                ..StateNodeDef::atomic()
            },
        );
        states.insert(
            StateId::from("fulfilled"),
            StateNodeDef {
                kind: StateKind::Final,
                entry: vec!["NOTIFY_USER".into()],
                ..StateNodeDef::final_state()
            },
        );
        states.insert(StateId::from("rejected"), StateNodeDef::final_state());
        states.insert(StateId::from("failed"), StateNodeDef::final_state());

        MachineDef {
            id: "order_workflow".into(),
            initial: StateId::from("validating"),
            context: json!({"limit": 1000, "attempts": 0}),
            states,
            output: None,
            description: None,
        }
    }

    #[test]
    fn valid_nested_compound_statechart_passes_validation() {
        let machine = sample_order_workflow();
        assert_eq!(machine.validate(), Ok(()));
    }

    #[test]
    fn missing_root_initial_state_fails_with_missing_initial_state() {
        let mut machine = sample_order_workflow();
        machine.initial = StateId::from("nonexistent");

        let err = machine.validate().expect_err("expected validation failure");
        assert!(err.contains(&IrValidationError::MissingInitialState {
            parent: None,
            initial: StateId::from("nonexistent"),
        }));
    }

    #[test]
    fn missing_compound_initial_state_fails_with_missing_initial_state() {
        let mut machine = sample_order_workflow();
        if let Some(processing) = machine.states.get_mut(&StateId::from("processing"))
            && let StateKind::Compound { initial, .. } = &mut processing.kind
        {
            *initial = StateId::from("missing_child");
        }

        let err = machine.validate().expect_err("expected validation failure");
        assert!(err.contains(&IrValidationError::MissingInitialState {
            parent: Some(StateId::from("processing")),
            initial: StateId::from("missing_child"),
        }));
    }

    #[test]
    fn transition_to_undeclared_state_fails_with_unknown_target_state() {
        let mut machine = sample_order_workflow();
        machine
            .states
            .get_mut(&StateId::from("validating"))
            .unwrap()
            .on
            .push(plain_tr("GHOST", Some("phantom"), None, vec![], None));

        let err = machine.validate().expect_err("expected validation failure");
        assert!(err.contains(&IrValidationError::UnknownTargetState {
            from: StateId::from("validating"),
            event: "GHOST".into(),
            target: StateId::from("phantom"),
        }));
    }

    #[test]
    fn outgoing_transition_on_final_state_fails() {
        let mut machine = sample_order_workflow();
        machine
            .states
            .get_mut(&StateId::from("fulfilled"))
            .unwrap()
            .on
            .push(plain_tr("RETRY", Some("validating"), None, vec![], None));

        let err = machine.validate().expect_err("expected validation failure");
        assert!(err.contains(&IrValidationError::FinalStateHasTransitions {
            state: StateId::from("fulfilled"),
        }));
    }

    #[test]
    fn orphaned_unreachable_state_is_detected_and_reported() {
        let mut machine = sample_order_workflow();
        machine
            .states
            .insert(StateId::from("orphaned"), StateNodeDef::atomic());

        let err = machine.validate().expect_err("expected validation failure");
        assert_eq!(
            err,
            vec![IrValidationError::UnreachableState {
                state: StateId::from("orphaned"),
                initial: StateId::from("validating"),
            }]
        );
    }

    #[test]
    fn duplicate_state_id_across_hierarchy_is_rejected() {
        let mut machine = sample_order_workflow();
        if let Some(processing) = machine.states.get_mut(&StateId::from("processing"))
            && let StateKind::Compound { states, .. } = &mut processing.kind
        {
            states.insert(StateId::from("rejected"), StateNodeDef::atomic());
        }

        let err = machine.validate().expect_err("expected validation failure");
        assert!(err.contains(&IrValidationError::DuplicateStateId {
            machine: "order_workflow".into(),
            state: StateId::from("rejected"),
        }));
    }

    #[test]
    fn parallel_state_enters_all_regions_for_reachability() {
        let mut region_a_children = IndexMap::new();
        region_a_children.insert(StateId::from("a_init"), StateNodeDef::atomic());

        let mut region_b_children = IndexMap::new();
        region_b_children.insert(StateId::from("b_init"), StateNodeDef::atomic());

        let mut parallel_regions = IndexMap::new();
        parallel_regions.insert(
            StateId::from("region_a"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("a_init"),
                    states: region_a_children,
                },
                ..StateNodeDef::atomic()
            },
        );
        parallel_regions.insert(
            StateId::from("region_b"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("b_init"),
                    states: region_b_children,
                },
                ..StateNodeDef::atomic()
            },
        );

        let mut root_states = IndexMap::new();
        root_states.insert(
            StateId::from("active_parallel"),
            StateNodeDef {
                kind: StateKind::Parallel {
                    states: parallel_regions,
                },
                ..StateNodeDef::atomic()
            },
        );

        let machine = MachineDef {
            id: "parallel_demo".into(),
            initial: StateId::from("active_parallel"),
            context: json!({}),
            states: root_states,
            output: None,
            description: None,
        };

        assert_eq!(machine.validate(), Ok(()));
    }

    #[test]
    fn invoke_outcomes_participate_in_reachability_and_target_validation() {
        let mut states = IndexMap::new();
        states.insert(
            StateId::from("fetching"),
            StateNodeDef {
                kind: StateKind::Atomic,
                invoke: vec![InvokeDef {
                    id: Some("fetch_user".into()),
                    src: "FETCH_USER".into(),
                    input: json!({"id": 42}),
                    on_done: vec![OutcomeDef {
                        target: Some(StateId::from("ready")),
                        guard: None,
                        actions: vec![],
                        assign: None,
                        reenter: false,
                        description: None,
                    }],
                    on_error: vec![OutcomeDef {
                        target: Some(StateId::from("errored")),
                        guard: None,
                        actions: vec![],
                        assign: None,
                        reenter: false,
                        description: None,
                    }],
                }],
                ..StateNodeDef::atomic()
            },
        );
        states.insert(StateId::from("ready"), StateNodeDef::final_state());
        states.insert(StateId::from("errored"), StateNodeDef::final_state());

        let mut machine = MachineDef {
            id: "invoke_demo".into(),
            initial: StateId::from("fetching"),
            context: json!({}),
            states,
            output: None,
            description: None,
        };
        assert_eq!(machine.validate(), Ok(()));

        machine
            .states
            .get_mut(&StateId::from("fetching"))
            .unwrap()
            .invoke[0]
            .on_done[0]
            .target = Some(StateId::from("missing_state"));

        let err = machine.validate().expect_err("expected unknown target");
        assert!(err.contains(&IrValidationError::UnknownTargetState {
            from: StateId::from("fetching"),
            event: "invoke(FETCH_USER).on_done".into(),
            target: StateId::from("missing_state"),
        }));
    }

    #[test]
    fn state_in_guard_and_history_pseudo_states_validate_cleanly() {
        let mut compound_children = IndexMap::new();
        compound_children.insert(
            StateId::from("hist"),
            StateNodeDef {
                kind: StateKind::History {
                    deep: false,
                    default_target: Some(StateId::from("step_a")),
                },
                ..StateNodeDef::atomic()
            },
        );
        compound_children.insert(
            StateId::from("step_a"),
            StateNodeDef {
                kind: StateKind::Atomic,
                on: vec![plain_tr("NEXT", Some("step_b"), None, vec![], None)],
                ..StateNodeDef::atomic()
            },
        );
        compound_children.insert(StateId::from("step_b"), StateNodeDef::final_state());

        let mut root_states = IndexMap::new();
        root_states.insert(
            StateId::from("wizard"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("step_a"),
                    states: compound_children,
                },
                on: vec![plain_tr("PAUSE", Some("paused"), None, vec![], None)],
                on_done: vec![OutcomeDef {
                    target: Some(StateId::from("done")),
                    guard: None,
                    actions: vec![],
                    assign: None,
                    reenter: false,
                    description: None,
                }],
                ..StateNodeDef::atomic()
            },
        );
        root_states.insert(
            StateId::from("paused"),
            StateNodeDef {
                kind: StateKind::Atomic,
                on: vec![plain_tr(
                    "RESUME",
                    Some("hist"),
                    Some(GuardRef::Not(Box::new(GuardRef::StateIn(StateId::from(
                        "done",
                    ))))),
                    vec![],
                    None,
                )],
                ..StateNodeDef::atomic()
            },
        );
        root_states.insert(StateId::from("done"), StateNodeDef::final_state());

        let mut machine = MachineDef {
            id: "wizard_hist".into(),
            initial: StateId::from("wizard"),
            context: json!({}),
            states: root_states,
            output: None,
            description: None,
        };
        assert_eq!(machine.validate(), Ok(()));

        // Corrupt `state_in` target -> should report `UnknownStateInGuard`.
        machine
            .states
            .get_mut(&StateId::from("paused"))
            .unwrap()
            .on[0]
            .guard = Some(GuardRef::StateIn(StateId::from("ghost_state")));

        let err = machine.validate().expect_err("expected UnknownStateInGuard");
        assert!(err.contains(&IrValidationError::UnknownStateInGuard {
            from: StateId::from("paused"),
            target: StateId::from("ghost_state"),
        }));
    }
}

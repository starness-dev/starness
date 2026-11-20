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

/// Transition definition attached to a state node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionDef {
    pub event: String,
    /// `None` indicates an internal self-transition without exiting/re-entering the state.
    pub target: Option<StateId>,
    pub guard: Option<GuardRef>,
    pub actions: Vec<String>,
    pub assign: Option<AssignRef>,
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
}

/// Definition of a single state node in the hierarchy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateNodeDef {
    pub kind: StateKind,
    pub entry: Vec<String>,
    pub exit: Vec<String>,
    pub on: Vec<TransitionDef>,
}

impl StateNodeDef {
    pub fn atomic() -> Self {
        Self {
            kind: StateKind::Atomic,
            entry: Vec::new(),
            exit: Vec::new(),
            on: Vec::new(),
        }
    }

    pub fn final_state() -> Self {
        Self {
            kind: StateKind::Final,
            entry: Vec::new(),
            exit: Vec::new(),
            on: Vec::new(),
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
}

/// Graph and structural integrity errors reported by [`MachineDef::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IrValidationError {
    #[error("duplicate state id `{state}` declared in machine `{machine}`")]
    DuplicateStateId { machine: String, state: StateId },

    #[error("compound state `{state}` must declare at least one child state")]
    EmptyCompoundState { state: StateId },

    #[error("parallel state `{state}` must declare at least one region state")]
    EmptyParallelState { state: StateId },

    #[error("{}", format_missing_initial(.parent.as_ref(), .initial))]
    MissingInitialState {
        parent: Option<StateId>,
        initial: StateId,
    },

    #[error("final state `{state}` cannot declare outgoing `on` transitions")]
    FinalStateHasTransitions { state: StateId },

    #[error("transition on event `{event}` from state `{from}` targets undeclared state `{target}`")]
    UnknownTargetState {
        from: StateId,
        event: String,
        target: StateId,
    },

    #[error("state `{state}` is unreachable from initial state `{initial}`")]
    UnreachableState { state: StateId, initial: StateId },
}

fn format_missing_initial(parent: Option<&StateId>, initial: &StateId) -> String {
    match parent {
        Some(p) => format!("compound state `{p}` initial state `{initial}` does not exist in its immediate `states`"),
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
    /// - No outgoing `on` transitions on `Final` states
    /// - All transition targets resolve to declared `StateId`s
    /// - Full reachability from root `initial`
    pub fn validate(&self) -> Result<(), Vec<IrValidationError>> {
        let mut errors = Vec::new();
        let mut registry: IndexMap<StateId, NodeRecord<'_>> = IndexMap::new();
        let mut has_duplicates = false;

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

        // Validate node-local constraints and transition targets in declaration order.
        for (state_id, record) in &registry {
            match &record.node.kind {
                StateKind::Compound { initial, states } => {
                    if states.is_empty() {
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
                }
                StateKind::Final => {
                    if !record.node.on.is_empty() {
                        errors.push(IrValidationError::FinalStateHasTransitions {
                            state: state_id.clone(),
                        });
                    }
                }
                StateKind::Atomic => {}
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
            }
        }

        // Reachability analysis from root initial when unambiguous and root initial exists.
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
                StateKind::Compound { states: children, .. }
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
                StateKind::Atomic | StateKind::Final => {}
            }
        }
    }

    fn compute_reachable_states(
        &self,
        registry: &IndexMap<StateId, NodeRecord<'_>>,
    ) -> HashSet<StateId> {
        // `active` tracks states that can become active at runtime.
        // `default_entered` tracks states targeted directly (so their default child entries fire).
        let mut active: HashSet<StateId> = HashSet::new();
        let mut default_entered: HashSet<StateId> = HashSet::new();

        #[derive(Debug, Clone)]
        enum WorkItem {
            EnterDefault(StateId),
            Activate(StateId),
        }

        let mut queue: VecDeque<WorkItem> = VecDeque::new();
        queue.push_back(WorkItem::EnterDefault(self.initial.clone()));

        // Map each parallel state to its immediate children for ancestor activation.
        let mut parallel_regions: HashMap<&StateId, Vec<&StateId>> = HashMap::new();
        for (id, rec) in registry {
            if let StateKind::Parallel { states } = &rec.node.kind {
                parallel_regions.insert(id, states.keys().collect());
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
                                for region_id in states.keys() {
                                    queue.push_back(WorkItem::EnterDefault(region_id.clone()));
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
                        // Activating a child also activates its ancestors.
                        if let Some(parent_id) = &rec.parent {
                            queue.push_back(WorkItem::Activate(parent_id.clone()));
                            // Entering inside a parallel state enters all orthogonal sibling regions.
                            if let Some(regions) = parallel_regions.get(parent_id) {
                                for &region_id in regions {
                                    if region_id != &state_id {
                                        queue.push_back(WorkItem::EnterDefault(region_id.clone()));
                                    }
                                }
                            }
                        }

                        // Outgoing transitions from any active state can fire.
                        for tr in &rec.node.on {
                            if let Some(target) = &tr.target
                                && registry.contains_key(target)
                            {
                                queue.push_back(WorkItem::EnterDefault(target.clone()));
                            }
                        }
                    }
                }
            }
        }

        active
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_order_workflow() -> MachineDef {
        let mut processing_children = IndexMap::new();
        processing_children.insert(
            StateId::from("charging"),
            StateNodeDef {
                kind: StateKind::Atomic,
                entry: vec!["CHARGE_CARD".into()],
                exit: vec![],
                on: vec![
                    TransitionDef {
                        event: "CHARGE_OK".into(),
                        target: Some(StateId::from("fulfilled")),
                        guard: None,
                        actions: vec![],
                        assign: None,
                    },
                    TransitionDef {
                        event: "CHARGE_ERR".into(),
                        target: Some(StateId::from("failed")),
                        guard: None,
                        actions: vec![],
                        assign: Some(AssignRef::StarlarkSlot(0)),
                    },
                ],
            },
        );

        let mut states = IndexMap::new();
        states.insert(
            StateId::from("validating"),
            StateNodeDef {
                kind: StateKind::Atomic,
                entry: vec![],
                exit: vec![],
                on: vec![
                    TransitionDef {
                        event: "APPROVED".into(),
                        target: Some(StateId::from("processing")),
                        guard: Some(GuardRef::StarlarkSlot(0)),
                        actions: vec![],
                        assign: None,
                    },
                    TransitionDef {
                        event: "REJECTED".into(),
                        target: Some(StateId::from("rejected")),
                        guard: None,
                        actions: vec!["LOG_REJECTION".into()],
                        assign: None,
                    },
                ],
            },
        );
        states.insert(
            StateId::from("processing"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("charging"),
                    states: processing_children,
                },
                entry: vec![],
                exit: vec![],
                on: vec![TransitionDef {
                    event: "CANCEL".into(),
                    target: Some(StateId::from("rejected")),
                    guard: None,
                    actions: vec![],
                    assign: None,
                }],
            },
        );
        states.insert(
            StateId::from("fulfilled"),
            StateNodeDef {
                kind: StateKind::Final,
                entry: vec!["NOTIFY_USER".into()],
                exit: vec![],
                on: vec![],
            },
        );
        states.insert(StateId::from("rejected"), StateNodeDef::final_state());
        states.insert(StateId::from("failed"), StateNodeDef::final_state());

        MachineDef {
            id: "order_workflow".into(),
            initial: StateId::from("validating"),
            context: json!({"limit": 1000, "attempts": 0}),
            states,
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
            .push(TransitionDef {
                event: "GHOST".into(),
                target: Some(StateId::from("phantom")),
                guard: None,
                actions: vec![],
                assign: None,
            });

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
            .push(TransitionDef {
                event: "RETRY".into(),
                target: Some(StateId::from("validating")),
                guard: None,
                actions: vec![],
                assign: None,
            });

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
                entry: vec![],
                exit: vec![],
                on: vec![],
            },
        );
        parallel_regions.insert(
            StateId::from("region_b"),
            StateNodeDef {
                kind: StateKind::Compound {
                    initial: StateId::from("b_init"),
                    states: region_b_children,
                },
                entry: vec![],
                exit: vec![],
                on: vec![],
            },
        );

        let mut root_states = IndexMap::new();
        root_states.insert(
            StateId::from("active_parallel"),
            StateNodeDef {
                kind: StateKind::Parallel {
                    states: parallel_regions,
                },
                entry: vec![],
                exit: vec![],
                on: vec![],
            },
        );

        let machine = MachineDef {
            id: "parallel_demo".into(),
            initial: StateId::from("active_parallel"),
            context: json!({}),
            states: root_states,
        };

        assert_eq!(machine.validate(), Ok(()));
    }
}

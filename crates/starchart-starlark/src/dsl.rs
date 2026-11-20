use anyhow::bail;
use starchart_ir::StateId;
use indexmap::IndexSet;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::starlark_module;
use starlark::syntax::{Dialect, DialectTypes};
use starlark::values::Value;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::UnpackList;
use starlark::values::record::Record;
use starlark::values::tuple::UnpackTuple;

use crate::types::{
    AssignCallable, GuardExprGen, GuardInput, NodeKindDataGen, StarlarkGuard, StarlarkMachine,
    StarlarkStateId, StarlarkStateNode, StarlarkStateSpace, StarlarkTransition, StateNodeDataGen,
    SymbolOrEnum, TransitionDataGen, build_and_validate_machine, clone_node_to_value,
    clone_transition_to_value,
};

#[starlark_module]
#[starlark_types(
    StarlarkStateId as StateId,
    StarlarkStateSpace as StateSpace,
    StarlarkGuard as Guard,
    StarlarkTransition as Transition,
    StarlarkStateNode as StateNode,
    StarlarkMachine as StateMachine
)]
pub fn register_dsl(builder: &mut GlobalsBuilder) {
    /// Declares a strongly typed `StateId` handle.
    fn state_id(name: &str) -> anyhow::Result<StarlarkStateId> {
        if name.trim().is_empty() {
            bail!("state_id name must not be empty");
        }
        Ok(StarlarkStateId(StateId::new(name)))
    }

    /// Declares a typed namespace of `StateId` handles without repeating names.
    fn state_space(#[starlark(args)] names: UnpackTuple<String>) -> anyhow::Result<StarlarkStateSpace> {
        if names.items.is_empty() {
            bail!("state_space(...) requires at least one state name");
        }
        let mut set = IndexSet::new();
        for name in names.items {
            if name.trim().is_empty() {
                bail!("state_space(...) state names must not be empty");
            }
            if !set.insert(name.clone()) {
                bail!("duplicate state name `{name}` in state_space(...)");
            }
        }
        Ok(StarlarkStateSpace::new(set))
    }

    /// References a named host-side guard registered in Rust.
    fn named_guard<'v>(name: SymbolOrEnum) -> anyhow::Result<StarlarkGuard<'v>> {
        Ok(StarlarkGuard {
            expr: GuardExprGen::Named(name.0),
        })
    }

    /// Logical AND combinator over two or more guards.
    fn all_of<'v>(
        #[starlark(args)] guards: UnpackTuple<GuardInput<'v>>,
    ) -> anyhow::Result<StarlarkGuard<'v>> {
        if guards.items.is_empty() {
            bail!("all_of(...) requires at least one guard");
        }
        Ok(StarlarkGuard {
            expr: GuardExprGen::AllOf(guards.items.into_iter().map(|g| g.0).collect()),
        })
    }

    /// Logical OR combinator over two or more guards.
    fn any_of<'v>(
        #[starlark(args)] guards: UnpackTuple<GuardInput<'v>>,
    ) -> anyhow::Result<StarlarkGuard<'v>> {
        if guards.items.is_empty() {
            bail!("any_of(...) requires at least one guard");
        }
        Ok(StarlarkGuard {
            expr: GuardExprGen::AnyOf(guards.items.into_iter().map(|g| g.0).collect()),
        })
    }

    /// Logical negation combinator for a guard.
    fn not_<'v>(guard: GuardInput<'v>) -> anyhow::Result<StarlarkGuard<'v>> {
        Ok(StarlarkGuard {
            expr: GuardExprGen::Not(Box::new(guard.0)),
        })
    }

    /// Declares a typed state transition.
    fn transition<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
    ) -> anyhow::Result<StarlarkTransition<'v>> {
        Ok(StarlarkTransition {
            data: TransitionDataGen {
                event: event.0,
                target: target.map(|t| t.0.0.clone()),
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
            },
        })
    }

    /// Declares an internal self-transition that executes actions/reducers without exiting the state.
    fn internal_transition<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
    ) -> anyhow::Result<StarlarkTransition<'v>> {
        Ok(StarlarkTransition {
            data: TransitionDataGen {
                event: event.0,
                target: None,
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
            },
        })
    }

    /// Declares an atomic leaf state node.
    fn state<'v>(
        #[starlark(require = named, default = UnpackList::default())] on: UnpackList<
            &StarlarkTransition<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            SymbolOrEnum,
        >,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::Atomic,
                entry: entry.items.into_iter().map(|a| a.0).collect(),
                exit: exit.items.into_iter().map(|a| a.0).collect(),
                on: on
                    .items
                    .into_iter()
                    .map(|t| clone_transition_to_value(&t.data))
                    .collect(),
            },
        })
    }

    /// Declares a hierarchical compound state containing nested substates and an initial child.
    fn compound_state<'v>(
        #[starlark(require = named)] initial: &StarlarkStateId,
        #[starlark(require = named)] states: UnpackDictEntries<
            &'v StarlarkStateId,
            &'v StarlarkStateNode<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] on: UnpackList<
            &StarlarkTransition<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            SymbolOrEnum,
        >,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        let children = states
            .entries
            .into_iter()
            .map(|(k, v)| (k.0.0.clone(), clone_node_to_value(&v.data)))
            .collect();

        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::Compound {
                    initial: initial.0.0.clone(),
                    children,
                },
                entry: entry.items.into_iter().map(|a| a.0).collect(),
                exit: exit.items.into_iter().map(|a| a.0).collect(),
                on: on
                    .items
                    .into_iter()
                    .map(|t| clone_transition_to_value(&t.data))
                    .collect(),
            },
        })
    }

    /// Declares an orthogonal parallel state where all child regions are active simultaneously.
    fn parallel_state<'v>(
        #[starlark(require = named)] states: UnpackDictEntries<
            &'v StarlarkStateId,
            &'v StarlarkStateNode<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] on: UnpackList<
            &StarlarkTransition<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            SymbolOrEnum,
        >,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        let children = states
            .entries
            .into_iter()
            .map(|(k, v)| (k.0.0.clone(), clone_node_to_value(&v.data)))
            .collect();

        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::Parallel { children },
                entry: entry.items.into_iter().map(|a| a.0).collect(),
                exit: exit.items.into_iter().map(|a| a.0).collect(),
                on: on
                    .items
                    .into_iter()
                    .map(|t| clone_transition_to_value(&t.data))
                    .collect(),
            },
        })
    }

    /// Declares a terminal final state node.
    fn final_state<'v>(
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            SymbolOrEnum,
        >,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::Final,
                entry: entry.items.into_iter().map(|a| a.0).collect(),
                exit: exit.items.into_iter().map(|a| a.0).collect(),
                on: Vec::new(),
            },
        })
    }

    /// Constructs and validates a complete Harel statechart.
    fn machine<'v>(
        #[starlark(require = named)] id: &str,
        #[starlark(require = named)] initial: &StarlarkStateId,
        #[starlark(require = named)] states: UnpackDictEntries<
            &'v StarlarkStateId,
            &'v StarlarkStateNode<'v>,
        >,
        #[starlark(require = named)] context: Option<Value<'v>>,
    ) -> anyhow::Result<StarlarkMachine<'v>> {
        let (context_json, initial_record_context) = match context {
            Some(val) if !val.is_none() => {
                let is_record = Record::from_value(val).is_some();
                let json = val.to_json_value()?;
                if !json.is_object() {
                    bail!("machine `context` must be a dict or record, got `{}`", val.get_type());
                }
                (json, is_record.then_some(val))
            }
            _ => (serde_json::json!({}), None),
        };

        let entries = states
            .entries
            .into_iter()
            .map(|(k, v)| (k.0.clone(), clone_node_to_value(&v.data)))
            .collect();

        match build_and_validate_machine(
            id.to_owned(),
            initial.0.clone(),
            context_json,
            initial_record_context,
            entries,
        ) {
            Ok(machine) => Ok(machine),
            Err(errors) => {
                let joined = errors
                    .into_iter()
                    .map(|e| format!("  - {e}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("invalid statechart `{id}`:\n{joined}")
            }
        }
    }
}

/// Builds the pure Starlark globals with `EnumType`, `RecordType`, `Typing`, and the Starchart DSL.
pub fn chart_globals() -> Globals {
    GlobalsBuilder::extended_by(&[
        LibraryExtension::EnumType,
        LibraryExtension::RecordType,
        LibraryExtension::Typing,
        LibraryExtension::StructType,
        LibraryExtension::NamespaceType,
    ])
    .with(register_dsl)
    .build()
}

/// Returns the Starlark dialect with static typechecking enabled.
pub fn chart_dialect() -> Dialect {
    Dialect {
        enable_types: DialectTypes::Enable,
        ..Dialect::Extended
    }
}

#![allow(clippy::too_many_arguments)]

use anyhow::bail;
use indexmap::IndexSet;
use starchart_ir::StateId;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{Dialect, DialectTypes};
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::{AllocList, UnpackList};
use starlark::values::none::NoneOr;
use starlark::values::record::Record;
use starlark::values::tuple::UnpackTuple;
use starlark::values::typing::{StarlarkCallable, StarlarkCallableParamAny};
use starlark::values::{FrozenValue, Value};

use crate::schema::{
    instantiate_record_in_eval, parse_record_schema_value, update_record_or_dict,
};
use crate::types::{
    ActionDataGen, ActionInput, AfterDataGen, AgentToolDataGen, ArtifactMode, AssignCallable,
    DelayInput, GuardExprGen, GuardInput, InvokeDataGen, InvokeInputGen, InvokeOutcomesInput,
    JsonHolder, NodeKindDataGen, OneOrManyAfter, OneOrManyInvoke, OutcomeDataGen, StarlarkAction,
    StarlarkAfter, StarlarkAgentTool, StarlarkGuard, StarlarkInvoke, StarlarkMachine,
    StarlarkOutcome, StarlarkStateId, StarlarkStateNode, StarlarkStateSpace, StarlarkTransition,
    StateNodeDataGen, SymbolOrEnum, TransitionDataGen, TypeShapeHolder,
    build_and_validate_machine, clone_node_to_value, clone_transition_to_value,
    unpack_invoke_input_value,
};

fn validate_mcp_tool_name(name: &str, fn_label: &str) -> anyhow::Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("{fn_label} `name` must not be empty");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "{fn_label} `name` `{trimmed}` must contain only ASCII letters, digits, `_`, or `-`"
        );
    }
    Ok(trimmed.to_owned())
}

fn build_artifact_tool<'v>(
    name: &str,
    schema: Value<'v>,
    description: &str,
    mode: Option<SymbolOrEnum>,
    required: bool,
    validate: NoneOr<StarlarkCallable<'v, (FrozenValue, FrozenValue), NoneOr<String>>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<StarlarkAgentTool<'v>> {
    let tool_name = validate_mcp_tool_name(name, "artifact_tool")?;
    let shape = parse_record_schema_value(schema, eval.heap())?;
    let resolved_mode = match mode {
        Some(m) => ArtifactMode::parse(&m.0)?,
        None => ArtifactMode::Replace,
    };
    Ok(StarlarkAgentTool {
        data: AgentToolDataGen::Artifact {
            name: tool_name,
            description: description.to_owned(),
            shape: TypeShapeHolder(shape),
            schema_ctor: schema,
            mode: resolved_mode,
            required,
            validate: validate.into_option().map(|c| c.0),
        },
    })
}

fn build_read_tool<'v>(
    name: &str,
    handler: StarlarkCallable<'v, StarlarkCallableParamAny, FrozenValue>,
    description: &str,
    schema: NoneOr<Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<StarlarkAgentTool<'v>> {
    let tool_name = validate_mcp_tool_name(name, "read_tool")?;
    let (shape_opt, ctor_opt) = match schema.into_option() {
        Some(s) if !s.is_none() => {
            let parsed = parse_record_schema_value(s, eval.heap())?;
            (Some(TypeShapeHolder(parsed)), Some(s))
        }
        _ => (None, None),
    };
    Ok(StarlarkAgentTool {
        data: AgentToolDataGen::Read {
            name: tool_name,
            description: description.to_owned(),
            shape: shape_opt,
            schema_ctor: ctor_opt,
            handler: handler.0,
        },
    })
}

fn unpack_optional_json(val: Option<Value<'_>>) -> anyhow::Result<Option<JsonHolder>> {
    match val {
        Some(v) if !v.is_none() => Ok(Some(JsonHolder(v.to_json_value()?))),
        _ => Ok(None),
    }
}

fn unpack_optional_meta(val: Option<Value<'_>>) -> anyhow::Result<Option<JsonHolder>> {
    match val {
        Some(v) if !v.is_none() => {
            let json = v.to_json_value()?;
            if !json.is_object() {
                bail!("`meta` must be a dict or record, got `{}`", v.get_type());
            }
            Ok(Some(JsonHolder(json)))
        }
        _ => Ok(None),
    }
}

#[starlark_module]
#[starlark_types(
    StarlarkStateId as StateId,
    StarlarkStateSpace as StateSpace,
    StarlarkGuard as Guard,
    StarlarkAction as ActionSpec,
    StarlarkTransition as Transition,
    StarlarkOutcome as Outcome,
    StarlarkAfter as After,
    StarlarkAgentTool as AgentTool,
    StarlarkInvoke as Invoke,
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
    fn state_space(
        #[starlark(args)] names: UnpackTuple<String>,
    ) -> anyhow::Result<StarlarkStateSpace> {
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

    /// Harel `In(state)` guard: evaluates to `True` while `state` is active (e.g. in a sibling parallel region).
    fn state_in<'v>(state: &StarlarkStateId) -> anyhow::Result<StarlarkGuard<'v>> {
        Ok(StarlarkGuard {
            expr: GuardExprGen::StateIn(state.0.0.clone()),
        })
    }

    /// Logical AND combinator over one or more guards.
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

    /// Logical OR combinator over one or more guards.
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

    /// Built-in action that queues an internal event to this machine (optionally delayed and cancellable via `id`).
    fn raise_event<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] payload: Option<Value<'v>>,
        #[starlark(require = named)] delay: Option<DelayInput<'v>>,
        #[starlark(require = named)] id: Option<&str>,
    ) -> anyhow::Result<StarlarkAction<'v>> {
        let payload_json = match payload {
            Some(v) if !v.is_none() => v.to_json_value()?,
            _ => serde_json::Value::Null,
        };
        Ok(StarlarkAction {
            data: ActionDataGen::Raise {
                event: event.0,
                payload: JsonHolder(payload_json),
                delay: delay.map(|d| d.0),
                id: id.map(str::to_owned),
            },
        })
    }

    /// Built-in action that cancels a pending delayed `raise_event` or `send_to` by its `id`.
    fn cancel_send<'v>(#[starlark(require = named)] id: &str) -> anyhow::Result<StarlarkAction<'v>> {
        if id.trim().is_empty() {
            bail!("cancel_send `id` must not be empty");
        }
        Ok(StarlarkAction {
            data: ActionDataGen::Cancel {
                send_id: id.to_owned(),
            },
        })
    }

    /// Built-in action that emits an external event to observers / `runner.take_emitted()`.
    fn emit_event<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] payload: Option<Value<'v>>,
    ) -> anyhow::Result<StarlarkAction<'v>> {
        let payload_json = match payload {
            Some(v) if !v.is_none() => v.to_json_value()?,
            _ => serde_json::Value::Null,
        };
        Ok(StarlarkAction {
            data: ActionDataGen::Emit {
                event: event.0,
                payload: JsonHolder(payload_json),
            },
        })
    }

    /// Built-in action that sends an event to another actor (`"parent"`, `"self"`, child id, or system id).
    fn send_to<'v>(
        #[starlark(require = named)] to: &str,
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] payload: Option<Value<'v>>,
        #[starlark(require = named)] delay: Option<DelayInput<'v>>,
        #[starlark(require = named)] id: Option<&str>,
    ) -> anyhow::Result<StarlarkAction<'v>> {
        let payload_json = match payload {
            Some(v) if !v.is_none() => v.to_json_value()?,
            _ => serde_json::Value::Null,
        };
        Ok(StarlarkAction {
            data: ActionDataGen::SendTo {
                to: to.to_owned(),
                event: event.0,
                payload: JsonHolder(payload_json),
                delay: delay.map(|d| d.0),
                id: id.map(str::to_owned),
            },
        })
    }

    /// Built-in action that sends an event to the parent machine when invoked as a child actor.
    fn send_parent<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] payload: Option<Value<'v>>,
    ) -> anyhow::Result<StarlarkAction<'v>> {
        let payload_json = match payload {
            Some(v) if !v.is_none() => v.to_json_value()?,
            _ => serde_json::Value::Null,
        };
        Ok(StarlarkAction {
            data: ActionDataGen::SendParent {
                event: event.0,
                payload: JsonHolder(payload_json),
            },
        })
    }

    /// Built-in action that appends a static value to the interpreter's log buffer (`runner.take_logs()`).
    fn log_action<'v>(expr: Value<'v>) -> anyhow::Result<StarlarkAction<'v>> {
        Ok(StarlarkAction {
            data: ActionDataGen::Log {
                expr: JsonHolder(expr.to_json_value()?),
            },
        })
    }

    /// Declares a typed state transition.
    fn transition<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
        #[starlark(require = named, default = false)] reenter: bool,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkTransition<'v>> {
        Ok(StarlarkTransition {
            data: TransitionDataGen {
                event: event.0,
                target: target.map(|t| t.0.0.clone()),
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
                reenter,
                forbidden: false,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares an internal self-transition that executes actions/reducers without exiting the state.
    fn internal_transition<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkTransition<'v>> {
        Ok(StarlarkTransition {
            data: TransitionDataGen {
                event: event.0,
                target: None,
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
                reenter: false,
                forbidden: false,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Explicitly forbids `event` in a state, consuming it so ancestor superstates do not handle it.
    fn forbid<'v>(
        #[starlark(require = named)] event: SymbolOrEnum,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkTransition<'v>> {
        Ok(StarlarkTransition {
            data: TransitionDataGen {
                event: event.0,
                target: None,
                guard: None,
                actions: Vec::new(),
                assign: None,
                reenter: false,
                forbidden: true,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares a transition outcome branch used by `always`, `after`, `on_done`, and `invoke`.
    fn outcome<'v>(
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
        #[starlark(require = named, default = false)] reenter: bool,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkOutcome<'v>> {
        Ok(StarlarkOutcome {
            data: OutcomeDataGen {
                target: target.map(|t| t.0.0.clone()),
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
                reenter,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares a delayed (`after`) timer transition fired after `delay`.
    fn after<'v>(
        #[starlark(require = named)] delay: DelayInput<'v>,
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] guard: Option<GuardInput<'v>>,
        #[starlark(require = named, default = UnpackList::default())] actions: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named)] assign: Option<AssignCallable<'v>>,
        #[starlark(require = named, default = false)] reenter: bool,
        #[starlark(require = named)] description: Option<&str>,
        #[starlark(require = named)] outcomes: Option<InvokeOutcomesInput<'v>>,
    ) -> anyhow::Result<StarlarkAfter<'v>> {
        let has_inline = target.is_some()
            || guard.is_some()
            || !actions.items.is_empty()
            || assign.is_some()
            || reenter
            || description.is_some();
        let resolved_outcomes = match (outcomes, has_inline) {
            (Some(_), true) => {
                bail!(
                    "after(...) accepts either inline `target`/`guard`/`actions`/`assign` OR `outcomes = [...]`, not both"
                );
            }
            (Some(branches), false) => {
                if branches.0.is_empty() {
                    bail!("after(..., outcomes = [...]) must not be empty");
                }
                branches.0
            }
            (None, _) => vec![OutcomeDataGen {
                target: target.map(|t| t.0.0.clone()),
                guard: guard.map(|g| g.0),
                actions: actions.items.into_iter().map(|a| a.0).collect(),
                assign: assign.map(|c| c.0),
                reenter,
                description: description.map(str::to_owned),
            }],
        };

        Ok(StarlarkAfter {
            data: AfterDataGen {
                delay: delay.0,
                outcomes: resolved_outcomes,
            },
        })
    }

    /// Declares an invoked host service or child statechart started on state entry and cancelled on exit.
    fn invoke<'v>(
        #[starlark(require = named)] src: SymbolOrEnum,
        #[starlark(require = named, default = NoneOr::None)] id: NoneOr<&str>,
        #[starlark(require = named)] input: Option<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] on_done: NoneOr<
            InvokeOutcomesInput<'v>,
        >,
        #[starlark(require = named, default = NoneOr::None)] on_error: NoneOr<
            InvokeOutcomesInput<'v>,
        >,
    ) -> anyhow::Result<StarlarkInvoke<'v>> {
        if src.0.trim().is_empty() {
            bail!("invoke `src` must not be empty");
        }
        let id_opt = id.into_option();
        if let Some(explicit_id) = id_opt
            && explicit_id.trim().is_empty()
        {
            bail!("invoke `id` must not be empty");
        }

        let input_data = match input {
            Some(val) if !val.is_none() => unpack_invoke_input_value(val)?,
            _ => InvokeInputGen::Json(JsonHolder(serde_json::Value::Null)),
        };

        Ok(StarlarkInvoke {
            data: InvokeDataGen {
                id: id_opt.map(str::to_owned),
                src: src.0,
                input: input_data,
                on_done: on_done.into_option().map(|o| o.0).unwrap_or_default(),
                on_error: on_error.into_option().map(|o| o.0).unwrap_or_default(),
            },
        })
    }

    /// Declares an atomic leaf state node.
    fn state<'v>(
        #[starlark(require = named, default = UnpackList::default())] on: UnpackList<
            &StarlarkTransition<'v>,
        >,
        #[starlark(require = named, default = InvokeOutcomesInput::default())]
        always: InvokeOutcomesInput<'v>,
        #[starlark(require = named, default = OneOrManyAfter::default())] after: OneOrManyAfter<
            'v,
        >,
        #[starlark(require = named, default = OneOrManyInvoke::default())]
        invoke: OneOrManyInvoke<'v>,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] tags: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] meta: Option<Value<'v>>,
        #[starlark(require = named)] description: Option<&str>,
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
                always: always.0,
                after: after.0,
                invoke: invoke.0,
                on_done: Vec::new(),
                output: None,
                tags: tags.items.into_iter().map(|t| t.0).collect(),
                meta: unpack_optional_meta(meta)?,
                description: description.map(str::to_owned),
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
        #[starlark(require = named, default = InvokeOutcomesInput::default())]
        always: InvokeOutcomesInput<'v>,
        #[starlark(require = named, default = OneOrManyAfter::default())] after: OneOrManyAfter<
            'v,
        >,
        #[starlark(require = named, default = OneOrManyInvoke::default())]
        invoke: OneOrManyInvoke<'v>,
        #[starlark(require = named, default = InvokeOutcomesInput::default())]
        on_done: InvokeOutcomesInput<'v>,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] tags: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] meta: Option<Value<'v>>,
        #[starlark(require = named)] description: Option<&str>,
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
                always: always.0,
                after: after.0,
                invoke: invoke.0,
                on_done: on_done.0,
                output: None,
                tags: tags.items.into_iter().map(|t| t.0).collect(),
                meta: unpack_optional_meta(meta)?,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares an orthogonal parallel state where all child regions are active simultaneously.
    /// `on_done` fires automatically once every child region reaches a `final_state`.
    fn parallel_state<'v>(
        #[starlark(require = named)] states: UnpackDictEntries<
            &'v StarlarkStateId,
            &'v StarlarkStateNode<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] on: UnpackList<
            &StarlarkTransition<'v>,
        >,
        #[starlark(require = named, default = InvokeOutcomesInput::default())]
        always: InvokeOutcomesInput<'v>,
        #[starlark(require = named, default = OneOrManyAfter::default())] after: OneOrManyAfter<
            'v,
        >,
        #[starlark(require = named, default = OneOrManyInvoke::default())]
        invoke: OneOrManyInvoke<'v>,
        #[starlark(require = named, default = InvokeOutcomesInput::default())]
        on_done: InvokeOutcomesInput<'v>,
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] tags: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] meta: Option<Value<'v>>,
        #[starlark(require = named)] description: Option<&str>,
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
                always: always.0,
                after: after.0,
                invoke: invoke.0,
                on_done: on_done.0,
                output: None,
                tags: tags.items.into_iter().map(|t| t.0).collect(),
                meta: unpack_optional_meta(meta)?,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares a terminal final state node (with optional structured `output`).
    fn final_state<'v>(
        #[starlark(require = named, default = UnpackList::default())] entry: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named, default = UnpackList::default())] exit: UnpackList<
            ActionInput<'v>,
        >,
        #[starlark(require = named)] output: Option<Value<'v>>,
        #[starlark(require = named, default = UnpackList::default())] tags: UnpackList<
            SymbolOrEnum,
        >,
        #[starlark(require = named)] meta: Option<Value<'v>>,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::Final,
                entry: entry.items.into_iter().map(|a| a.0).collect(),
                exit: exit.items.into_iter().map(|a| a.0).collect(),
                on: Vec::new(),
                always: Vec::new(),
                after: Vec::new(),
                invoke: Vec::new(),
                on_done: Vec::new(),
                output: unpack_optional_json(output)?,
                tags: tags.items.into_iter().map(|t| t.0).collect(),
                meta: unpack_optional_meta(meta)?,
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares a Harel shallow history (`H`) pseudo-state inside a `compound_state`.
    fn shallow_history<'v>(
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::History {
                    deep: false,
                    default_target: target.map(|t| t.0.0.clone()),
                },
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
                description: description.map(str::to_owned),
            },
        })
    }

    /// Declares a Harel deep history (`H*`) pseudo-state inside a `compound_state`.
    fn deep_history<'v>(
        #[starlark(require = named)] target: Option<&StarlarkStateId>,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkStateNode<'v>> {
        Ok(StarlarkStateNode {
            data: StateNodeDataGen {
                kind: NodeKindDataGen::History {
                    deep: true,
                    default_target: target.map(|t| t.0.0.clone()),
                },
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
                description: description.map(str::to_owned),
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
        #[starlark(require = named)] output: Option<Value<'v>>,
        #[starlark(require = named)] description: Option<&str>,
    ) -> anyhow::Result<StarlarkMachine<'v>> {
        let (context_json, initial_record_context) = match context {
            Some(val) if !val.is_none() => {
                let is_record = Record::from_value(val).is_some();
                let json = val.to_json_value()?;
                if !json.is_object() {
                    bail!(
                        "machine `context` must be a dict or record, got `{}`",
                        val.get_type()
                    );
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
            unpack_optional_json(output)?.map(|j| j.0),
            description.map(str::to_owned),
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

    /// Declares a turn-scoped MCP artifact/write tool validated against a Starlark `record(...)`.
    fn artifact_tool<'v>(
        name: &str,
        #[starlark(require = named)] schema: Value<'v>,
        #[starlark(require = named, default = "")] description: &str,
        #[starlark(require = named)] mode: Option<SymbolOrEnum>,
        #[starlark(require = named, default = true)] required: bool,
        #[starlark(require = named, default = NoneOr::None)] validate: NoneOr<
            StarlarkCallable<'v, (FrozenValue, FrozenValue), NoneOr<String>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<StarlarkAgentTool<'v>> {
        build_artifact_tool(name, schema, description, mode, required, validate, eval)
    }

    /// Internal alias used by `@starchart/acp` to re-export `artifact_tool` without local shadowing.
    fn __starchart_artifact_tool__<'v>(
        name: &str,
        #[starlark(require = named)] schema: Value<'v>,
        #[starlark(require = named, default = "")] description: &str,
        #[starlark(require = named)] mode: Option<SymbolOrEnum>,
        #[starlark(require = named, default = true)] required: bool,
        #[starlark(require = named, default = NoneOr::None)] validate: NoneOr<
            StarlarkCallable<'v, (FrozenValue, FrozenValue), NoneOr<String>>,
        >,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<StarlarkAgentTool<'v>> {
        build_artifact_tool(name, schema, description, mode, required, validate, eval)
    }

    /// Declares a turn-scoped MCP read-only query tool backed by a pure Starlark function.
    fn read_tool<'v>(
        name: &str,
        #[starlark(require = named)] handler: StarlarkCallable<
            'v,
            StarlarkCallableParamAny,
            FrozenValue,
        >,
        #[starlark(require = named, default = "")] description: &str,
        #[starlark(require = named, default = NoneOr::None)] schema: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<StarlarkAgentTool<'v>> {
        build_read_tool(name, handler, description, schema, eval)
    }

    /// Internal alias used by `@starchart/acp` to re-export `read_tool` without local shadowing.
    fn __starchart_read_tool__<'v>(
        name: &str,
        #[starlark(require = named)] handler: StarlarkCallable<
            'v,
            StarlarkCallableParamAny,
            FrozenValue,
        >,
        #[starlark(require = named, default = "")] description: &str,
        #[starlark(require = named, default = NoneOr::None)] schema: NoneOr<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<StarlarkAgentTool<'v>> {
        build_read_tool(name, handler, description, schema, eval)
    }

    /// Returns a shallow-updated copy of a `record` or `dict` with only the specified fields changed
    /// (similar to Python's `dataclasses.replace(ctx, field = val)`).
    fn update<'v>(
        #[starlark(require = pos)] target: Value<'v>,
        #[starlark(kwargs)] kwargs: UnpackDictEntries<&'v str, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        update_record_or_dict(target, kwargs.entries, eval)
    }

    /// Extracts a validated turn artifact from an `on_done` event (`event["output"]["artifacts"][name]`)
    /// and instantiates it as `schema` (`record(...)`).
    fn artifact<'v>(
        event: Value<'v>,
        name: &str,
        schema: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        extract_single_artifact(event, name, schema, eval)
    }

    /// Internal alias used by `@starchart/acp` to re-export `artifact`.
    fn __starchart_artifact__<'v>(
        event: Value<'v>,
        name: &str,
        schema: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        extract_single_artifact(event, name, schema, eval)
    }

    /// Extracts an `append`-mode list of turn artifacts (`event["output"]["artifacts"][name]`)
    /// and instantiates each item as `schema` (`record(...)`).
    fn artifact_list<'v>(
        event: Value<'v>,
        name: &str,
        schema: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        extract_artifact_list(event, name, schema, eval)
    }

    /// Internal alias used by `@starchart/acp` to re-export `artifact_list`.
    fn __starchart_artifact_list__<'v>(
        event: Value<'v>,
        name: &str,
        schema: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        extract_artifact_list(event, name, schema, eval)
    }
}

fn lookup_artifacts_map(
    event_json: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    event_json
        .get("output")
        .and_then(|o| o.get("artifacts"))
        .and_then(serde_json::Value::as_object)
        .or_else(|| event_json.get("artifacts").and_then(serde_json::Value::as_object))
        .or_else(|| event_json.as_object())
}

fn extract_single_artifact<'v>(
    event: Value<'v>,
    name: &str,
    schema: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let ev_json = event.to_json_value()?;
    let raw = lookup_artifacts_map(&ev_json)
        .and_then(|m| m.get(name))
        .ok_or_else(|| anyhow::anyhow!("missing artifact `{name}` in event output"))?;
    instantiate_record_in_eval(schema, raw, eval)
}

fn extract_artifact_list<'v>(
    event: Value<'v>,
    name: &str,
    schema: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let ev_json = event.to_json_value()?;
    let Some(raw) = lookup_artifacts_map(&ev_json).and_then(|m| m.get(name)) else {
        return Ok(eval.heap().alloc(AllocList(Vec::<Value<'v>>::new())));
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("artifact `{name}` is not a list, got `{raw}`"))?;
    let mut items = Vec::with_capacity(arr.len());
    for item in arr {
        items.push(instantiate_record_in_eval(schema, item, eval)?);
    }
    Ok(eval.heap().alloc(AllocList(items)))
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

use std::fmt::{self, Display};
use std::hash::Hash;

use allocative::Allocative;
use starchart_ir::{
    AssignRef, GuardRef, IrValidationError, MachineDef, StateId, StateKind, StateNodeDef,
    TransitionDef,
};
use indexmap::{IndexMap, IndexSet};
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::StarlarkHasher;
use starlark::starlark_complex_value;
use starlark::starlark_simple_value;
use starlark::typing::Ty;
use starlark::values::dict::DictRef;
use starlark::values::enumeration::EnumValue;
use starlark::values::record::Record;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::typing::StarlarkCallable;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace,
    UnpackValue, Value, ValueLifetimeless, ValueLike, starlark_value,
};

/// Accepts either a raw `str` or a Starlark `enum(...)` variant (`EnumValue`),
/// extracting the underlying string name while preserving static typechecking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOrEnum(pub String);

impl StarlarkTypeRepr for SymbolOrEnum {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(String::starlark_type_repr(), EnumValue::starlark_type_repr())
    }
}

impl<'v> UnpackValue<'v> for SymbolOrEnum {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(s) = value.unpack_str() {
            return Ok(Some(Self(s.to_owned())));
        }
        if EnumValue::from_value(value).is_some() {
            let json = value.to_json_value()?;
            match json {
                serde_json::Value::String(s) => return Ok(Some(Self(s))),
                other => {
                    return Err(starlark::Error::new_other(anyhow::anyhow!(
                        "enum variant must wrap a string value, got `{other}`"
                    )));
                }
            }
        }
        Ok(None)
    }
}

/// Type marker representing `dict | record` for static typechecking of context reducers.
pub struct DictOrRecord;

impl StarlarkTypeRepr for DictOrRecord {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(DictRef::starlark_type_repr(), Record::starlark_type_repr())
    }
}

impl<'v> UnpackValue<'v> for DictOrRecord {
    type Error = std::convert::Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if DictRef::from_value(value).is_some() || Record::from_value(value).is_some() {
            Ok(Some(Self))
        } else {
            Ok(None)
        }
    }
}

/// Statically typed `(ctx, event) -> bool` Starlark guard callable.
pub type GuardCallable<'v> = StarlarkCallable<'v, (FrozenValue, FrozenValue), bool>;

/// Statically typed `(ctx, event) -> dict | record` Starlark assign reducer callable.
pub type AssignCallable<'v> = StarlarkCallable<'v, (FrozenValue, FrozenValue), DictOrRecord>;

/// Strongly typed Starlark wrapper around [`StateId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Allocative, ProvidesStaticType, NoSerialize)]
pub struct StarlarkStateId(#[allocative(skip)] pub StateId);

starlark_simple_value!(StarlarkStateId);

impl Display for StarlarkStateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state_id({:?})", self.0.as_str())
    }
}

#[starlark_value(type = "StateId")]
impl<'v> StarlarkValue<'v> for StarlarkStateId {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(match StarlarkStateId::from_value(other) {
            Some(other_id) => self.0 == other_id.0,
            None => false,
        })
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.0.hash(hasher);
        Ok(())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "id" | "name" => Some(heap.alloc_str(self.0.as_str()).to_value()),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["id".to_owned(), "name".to_owned()]
    }
}

/// Namespace of typed `StateId` handles created by `state_space("a", "b", ...)`.
#[derive(Debug, Clone, Allocative, ProvidesStaticType, NoSerialize)]
pub struct StarlarkStateSpace {
    names: Vec<String>,
}

starlark_simple_value!(StarlarkStateSpace);

impl StarlarkStateSpace {
    pub fn new(names: IndexSet<String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }
}

impl Display for StarlarkStateSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state_space(")?;
        for (idx, name) in self.names.iter().enumerate() {
            if idx > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name:?}")?;
        }
        write!(f, ")")
    }
}

#[starlark_value(type = "StateSpace")]
impl<'v> StarlarkValue<'v> for StarlarkStateSpace {
    fn attr_ty(_name: &str) -> Option<Ty> {
        Some(StarlarkStateId::starlark_type_repr())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if self.names.iter().any(|n| n == attribute) {
            Some(heap.alloc_simple(StarlarkStateId(StateId::new(attribute))))
        } else {
            None
        }
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        self.names.iter().any(|n| n == attribute)
    }

    fn dir_attr(&self) -> Vec<String> {
        self.names.clone()
    }
}

/// Boolean guard expression tree holding either Starlark callables or named host guards.
#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum GuardExprGen<V: ValueLifetimeless> {
    Callable(V),
    Named(String),
    AllOf(Vec<GuardExprGen<V>>),
    AnyOf(Vec<GuardExprGen<V>>),
    Not(Box<GuardExprGen<V>>),
}

unsafe impl<'v> Coerce<GuardExprGen<Value<'v>>> for GuardExprGen<FrozenValue> {}

/// Starlark value representing a composable guard (`named_guard`, `all_of`, `any_of`, `not_`).
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkGuardGen<V: ValueLifetimeless> {
    pub expr: GuardExprGen<V>,
}

starlark_complex_value!(pub StarlarkGuard);

impl<V: ValueLifetimeless> Display for StarlarkGuardGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<Guard>")
    }
}

#[starlark_value(type = "Guard")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkGuardGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Polymorphic guard input accepted by `transition(guard = ...)` and guard combinators.
pub struct GuardInput<'v>(pub GuardExprGen<Value<'v>>);

impl<'v> StarlarkTypeRepr for GuardInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::unions(vec![
            GuardCallable::starlark_type_repr(),
            StarlarkGuard::starlark_type_repr(),
            SymbolOrEnum::starlark_type_repr(),
        ])
    }
}

impl<'v> UnpackValue<'v> for GuardInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(guard) = StarlarkGuard::from_value(value) {
            return Ok(Some(Self(clone_guard_expr_to_value(&guard.expr))));
        }
        if let Some(sym) = SymbolOrEnum::unpack_value_impl(value)? {
            return Ok(Some(Self(GuardExprGen::Named(sym.0))));
        }
        if let Ok(Some(callable)) = GuardCallable::unpack_value_impl(value) {
            return Ok(Some(Self(GuardExprGen::Callable(callable.0))));
        }
        Ok(None)
    }
}

pub fn clone_guard_expr_to_value<'v, V: ValueLike<'v>>(
    expr: &GuardExprGen<V>,
) -> GuardExprGen<Value<'v>> {
    match expr {
        GuardExprGen::Callable(v) => GuardExprGen::Callable(v.to_value()),
        GuardExprGen::Named(name) => GuardExprGen::Named(name.clone()),
        GuardExprGen::AllOf(items) => {
            GuardExprGen::AllOf(items.iter().map(clone_guard_expr_to_value).collect())
        }
        GuardExprGen::AnyOf(items) => {
            GuardExprGen::AnyOf(items.iter().map(clone_guard_expr_to_value).collect())
        }
        GuardExprGen::Not(inner) => GuardExprGen::Not(Box::new(clone_guard_expr_to_value(inner))),
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct TransitionDataGen<V: ValueLifetimeless> {
    pub event: String,
    pub target: Option<String>,
    pub guard: Option<GuardExprGen<V>>,
    pub actions: Vec<String>,
    pub assign: Option<V>,
}

unsafe impl<'v> Coerce<TransitionDataGen<Value<'v>>> for TransitionDataGen<FrozenValue> {}

/// Typed Starlark transition value returned by `transition(...)` and `internal_transition(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkTransitionGen<V: ValueLifetimeless> {
    pub data: TransitionDataGen<V>,
}

starlark_complex_value!(pub StarlarkTransition);

impl<V: ValueLifetimeless> Display for StarlarkTransitionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data.target {
            Some(target) => {
                write!(f, "transition(event={:?}, target={:?})", self.data.event, target)
            }
            None => write!(f, "internal_transition(event={:?})", self.data.event),
        }
    }
}

#[starlark_value(type = "Transition")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkTransitionGen<V> where
    Self: ProvidesStaticType<'v>
{
}

pub fn clone_transition_to_value<'v, V: ValueLike<'v>>(
    tr: &TransitionDataGen<V>,
) -> TransitionDataGen<Value<'v>> {
    TransitionDataGen {
        event: tr.event.clone(),
        target: tr.target.clone(),
        guard: tr.guard.as_ref().map(clone_guard_expr_to_value),
        actions: tr.actions.clone(),
        assign: tr.assign.map(|v| v.to_value()),
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum NodeKindDataGen<V: ValueLifetimeless> {
    Atomic,
    Compound {
        initial: String,
        children: Vec<(String, StateNodeDataGen<V>)>,
    },
    Parallel {
        children: Vec<(String, StateNodeDataGen<V>)>,
    },
    Final,
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct StateNodeDataGen<V: ValueLifetimeless> {
    pub kind: NodeKindDataGen<V>,
    pub entry: Vec<String>,
    pub exit: Vec<String>,
    pub on: Vec<TransitionDataGen<V>>,
}

unsafe impl<'v> Coerce<NodeKindDataGen<Value<'v>>> for NodeKindDataGen<FrozenValue> {}
unsafe impl<'v> Coerce<StateNodeDataGen<Value<'v>>> for StateNodeDataGen<FrozenValue> {}

pub fn clone_node_to_value<'v, V: ValueLike<'v>>(
    node: &StateNodeDataGen<V>,
) -> StateNodeDataGen<Value<'v>> {
    let kind = match &node.kind {
        NodeKindDataGen::Atomic => NodeKindDataGen::Atomic,
        NodeKindDataGen::Compound { initial, children } => NodeKindDataGen::Compound {
            initial: initial.clone(),
            children: children
                .iter()
                .map(|(k, v)| (k.clone(), clone_node_to_value(v)))
                .collect(),
        },
        NodeKindDataGen::Parallel { children } => NodeKindDataGen::Parallel {
            children: children
                .iter()
                .map(|(k, v)| (k.clone(), clone_node_to_value(v)))
                .collect(),
        },
        NodeKindDataGen::Final => NodeKindDataGen::Final,
    };
    StateNodeDataGen {
        kind,
        entry: node.entry.clone(),
        exit: node.exit.clone(),
        on: node.on.iter().map(clone_transition_to_value).collect(),
    }
}

/// Typed Starlark state node returned by `state`, `compound_state`, `parallel_state`, and `final_state`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkStateNodeGen<V: ValueLifetimeless> {
    pub data: StateNodeDataGen<V>,
}

starlark_complex_value!(pub StarlarkStateNode);

impl<V: ValueLifetimeless> Display for StarlarkStateNodeGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data.kind {
            NodeKindDataGen::Atomic => f.write_str("state(...)"),
            NodeKindDataGen::Compound { initial, .. } => {
                write!(f, "compound_state(initial={initial:?}, ...)")
            }
            NodeKindDataGen::Parallel { .. } => f.write_str("parallel_state(...)"),
            NodeKindDataGen::Final => f.write_str("final_state(...)"),
        }
    }
}

#[starlark_value(type = "StateNode")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkStateNodeGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Wrapper around [`MachineDef`] implementing `Allocative` for Starlark value storage.
#[derive(Debug, Clone, PartialEq, Allocative)]
pub struct IrHolder(#[allocative(skip)] pub MachineDef);

/// Typed Starlark state machine returned by `machine(...)`.
#[derive(Debug, Clone, Trace, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkMachineGen<V: ValueLifetimeless> {
    #[trace(unsafe_ignore)]
    pub ir: IrHolder,
    pub guard_slots: Vec<V>,
    pub reducer_slots: Vec<V>,
    /// Preserved initial context value if constructed from a Starlark `record(...)` type.
    pub initial_record_context: Option<V>,
}

unsafe impl<'v> Coerce<StarlarkMachineGen<Value<'v>>> for StarlarkMachineGen<FrozenValue> {}

impl<'v> Freeze for StarlarkMachineGen<Value<'v>> {
    type Frozen = StarlarkMachineGen<FrozenValue>;

    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(StarlarkMachineGen {
            ir: self.ir,
            guard_slots: self.guard_slots.freeze(freezer)?,
            reducer_slots: self.reducer_slots.freeze(freezer)?,
            initial_record_context: self.initial_record_context.freeze(freezer)?,
        })
    }
}

impl<V: ValueLifetimeless> Display for StarlarkMachineGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "machine(id={:?})", self.ir.0.id)
    }
}

#[starlark_value(type = "StateMachine")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkMachineGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "id" => Some(heap.alloc_str(&self.ir.0.id).to_value()),
            "initial" => Some(heap.alloc_simple(StarlarkStateId(self.ir.0.initial.clone()))),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["id".to_owned(), "initial".to_owned()]
    }
}

starlark_complex_value!(pub StarlarkMachine);

/// Lowers Starlark state node data into [`MachineDef`], assigning callable slots
/// and running [`MachineDef::validate`].
pub fn build_and_validate_machine<'v>(
    id: String,
    initial: StateId,
    context: serde_json::Value,
    initial_record_context: Option<Value<'v>>,
    entries: Vec<(StateId, StateNodeDataGen<Value<'v>>)>,
) -> Result<StarlarkMachine<'v>, Vec<IrValidationError>> {
    let mut guard_slots: Vec<Value<'v>> = Vec::new();
    let mut reducer_slots: Vec<Value<'v>> = Vec::new();

    let mut states = IndexMap::new();
    for (state_id, node_data) in entries {
        let ir_node = lower_node(node_data, &mut guard_slots, &mut reducer_slots);
        states.insert(state_id, ir_node);
    }

    let machine_def = MachineDef {
        id,
        initial,
        context,
        states,
    };

    machine_def.validate()?;

    Ok(StarlarkMachine {
        ir: IrHolder(machine_def),
        guard_slots,
        reducer_slots,
        initial_record_context,
    })
}

fn lower_guard<'v>(expr: GuardExprGen<Value<'v>>, guard_slots: &mut Vec<Value<'v>>) -> GuardRef {
    match expr {
        GuardExprGen::Callable(val) => {
            let slot = guard_slots.len();
            guard_slots.push(val);
            GuardRef::StarlarkSlot(slot)
        }
        GuardExprGen::Named(name) => GuardRef::Named(name),
        GuardExprGen::AllOf(items) => {
            GuardRef::AllOf(items.into_iter().map(|g| lower_guard(g, guard_slots)).collect())
        }
        GuardExprGen::AnyOf(items) => {
            GuardRef::AnyOf(items.into_iter().map(|g| lower_guard(g, guard_slots)).collect())
        }
        GuardExprGen::Not(inner) => GuardRef::Not(Box::new(lower_guard(*inner, guard_slots))),
    }
}

fn lower_transition<'v>(
    tr: TransitionDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
) -> TransitionDef {
    let guard = tr.guard.map(|g| lower_guard(g, guard_slots));
    let assign = tr.assign.map(|val| {
        let slot = reducer_slots.len();
        reducer_slots.push(val);
        AssignRef::StarlarkSlot(slot)
    });
    TransitionDef {
        event: tr.event,
        target: tr.target.map(StateId),
        guard,
        actions: tr.actions,
        assign,
    }
}

fn lower_node<'v>(
    node: StateNodeDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
) -> StateNodeDef {
    let kind = match node.kind {
        NodeKindDataGen::Atomic => StateKind::Atomic,
        NodeKindDataGen::Compound { initial, children } => {
            let mut map = IndexMap::new();
            for (child_id, child_node) in children {
                map.insert(
                    StateId(child_id),
                    lower_node(child_node, guard_slots, reducer_slots),
                );
            }
            StateKind::Compound {
                initial: StateId(initial),
                states: map,
            }
        }
        NodeKindDataGen::Parallel { children } => {
            let mut map = IndexMap::new();
            for (child_id, child_node) in children {
                map.insert(
                    StateId(child_id),
                    lower_node(child_node, guard_slots, reducer_slots),
                );
            }
            StateKind::Parallel { states: map }
        }
        NodeKindDataGen::Final => StateKind::Final,
    };

    let on = node
        .on
        .into_iter()
        .map(|tr| lower_transition(tr, guard_slots, reducer_slots))
        .collect();

    StateNodeDef {
        kind,
        entry: node.entry,
        exit: node.exit,
        on,
    }
}

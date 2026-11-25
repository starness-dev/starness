use std::fmt::{self, Display};
use std::hash::Hash;

use allocative::Allocative;
use indexmap::{IndexMap, IndexSet};
use serde::ser::SerializeMap;
use starness_ir::{
    ActionDef, AfterDef, AssignRef, DelayRef, GuardRef, InvokeDef, IrValidationError, MachineDef,
    OutcomeDef, SECRET_MARKER_KEY, StateId, StateKind, StateNodeDef, TransitionDef,
};
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::StarlarkHasher;
use starlark::starlark_complex_value;
use starlark::starlark_simple_value;
use starlark::typing::Ty;
use starlark::values::dict::DictRef;
use starlark::values::enumeration::EnumValue;
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::record::Record;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::typing::StarlarkCallable;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace,
    UnpackValue, Value, ValueLifetimeless, ValueLike, starlark_value,
};

use crate::schema::TypeShape;

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

/// Statically typed `(ctx) -> list` callable for `invoke_all(items = ...)`.
pub type BatchItemsCallable<'v> = StarlarkCallable<'v, (FrozenValue,), Vec<FrozenValue>>;

/// Polymorphic `items` input accepted by `invoke_all(items = ...)`: either a static list of
/// `batch_item(...)` dicts or a 1-arg `(ctx) -> list` callable.
pub struct BatchItemsInput<'v>(pub Value<'v>);

impl<'v> StarlarkTypeRepr for BatchItemsInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(Ty::list(Ty::any()), BatchItemsCallable::starlark_type_repr())
    }
}

impl<'v> UnpackValue<'v> for BatchItemsInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if starlark::values::list::ListRef::from_value(value).is_some()
            || matches!(BatchItemsCallable::unpack_value_impl(value), Ok(Some(_)))
        {
            return Ok(Some(Self(value)));
        }
        Ok(None)
    }
}

/// Type marker representing `int | float` for dynamic delay callables `(ctx, event) -> ms`.
pub struct IntOrFloat;

impl StarlarkTypeRepr for IntOrFloat {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(i64::starlark_type_repr(), f64::starlark_type_repr())
    }
}

impl<'v> UnpackValue<'v> for IntOrFloat {
    type Error = std::convert::Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if value.unpack_i32().is_some() || value.get_type() == "int" || value.get_type() == "float"
        {
            Ok(Some(Self))
        } else {
            Ok(None)
        }
    }
}

/// Statically typed `(ctx, event) -> int | float` Starlark dynamic delay callable.
pub type DelayCallable<'v> = StarlarkCallable<'v, (FrozenValue, FrozenValue), IntOrFloat>;

/// Wrapper around `serde_json::Value` skipping `Allocative` traversal.
#[derive(Debug, Clone, PartialEq, Allocative)]
pub struct JsonHolder(#[allocative(skip)] pub serde_json::Value);

impl Freeze for JsonHolder {
    type Frozen = Self;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(self)
    }
}

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

/// Late-bound secret handle constructed via `@starness/secrets` (`secret("API_KEY", ...)`).
/// Never resolves environment variables, files, or OS keychains during Starlark evaluation;
/// serializes to an opaque `$starness_secret` descriptor resolved inside outbound Rust drivers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Allocative, ProvidesStaticType)]
pub struct StarlarkSecret {
    pub source: String,
    pub name: String,
    pub service: String,
    pub prefix: String,
    pub optional: bool,
}

starlark_simple_value!(StarlarkSecret);

impl Display for StarlarkSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<secret:{}>", self.name)
    }
}

impl serde::Serialize for StarlarkSecret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Inner<'a> {
            source: &'a str,
            name: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            service: Option<&'a str>,
            prefix: &'a str,
            optional: bool,
        }
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(
            SECRET_MARKER_KEY,
            &Inner {
                source: &self.source,
                name: &self.name,
                service: (self.source == "keychain").then_some(self.service.as_str()),
                prefix: &self.prefix,
                optional: self.optional,
            },
        )?;
        map.end()
    }
}

#[starlark_value(type = "Secret")]
impl<'v> StarlarkValue<'v> for StarlarkSecret {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(match StarlarkSecret::from_value(other) {
            Some(other_sec) => self == other_sec,
            None => false,
        })
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.hash(hasher);
        Ok(())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "name" => Some(heap.alloc_str(&self.name).to_value()),
            "source" => Some(heap.alloc_str(&self.source).to_value()),
            "service" => Some(heap.alloc_str(&self.service).to_value()),
            "prefix" => Some(heap.alloc_str(&self.prefix).to_value()),
            "optional" => Some(Value::new_bool(self.optional)),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "name".to_owned(),
            "source".to_owned(),
            "service".to_owned(),
            "prefix".to_owned(),
            "optional".to_owned(),
        ]
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

/// Boolean guard expression tree holding Starlark callables, named host guards, or `state_in` predicates.
#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum GuardExprGen<V: ValueLifetimeless> {
    Callable(V),
    Named(String),
    StateIn(String),
    AllOf(Vec<GuardExprGen<V>>),
    AnyOf(Vec<GuardExprGen<V>>),
    Not(Box<GuardExprGen<V>>),
}

unsafe impl<'v> Coerce<GuardExprGen<Value<'v>>> for GuardExprGen<FrozenValue> {}

/// Starlark value representing a composable guard (`named_guard`, `state_in`, `all_of`, `any_of`, `not_`).
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
        GuardExprGen::StateIn(sid) => GuardExprGen::StateIn(sid.clone()),
        GuardExprGen::AllOf(items) => {
            GuardExprGen::AllOf(items.iter().map(clone_guard_expr_to_value).collect())
        }
        GuardExprGen::AnyOf(items) => {
            GuardExprGen::AnyOf(items.iter().map(clone_guard_expr_to_value).collect())
        }
        GuardExprGen::Not(inner) => GuardExprGen::Not(Box::new(clone_guard_expr_to_value(inner))),
    }
}

/// Delay specification in Starlark: fixed milliseconds (`int >= 0`), named host delay (`SymbolOrEnum`),
/// or pure Starlark dynamic delay callable `(ctx, event) -> int | float`.
#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum DelayExprGen<V: ValueLifetimeless> {
    FixedMs(u64),
    Named(String),
    Callable(V),
}

unsafe impl<'v> Coerce<DelayExprGen<Value<'v>>> for DelayExprGen<FrozenValue> {}

pub fn clone_delay_to_value<'v, V: ValueLike<'v>>(
    d: &DelayExprGen<V>,
) -> DelayExprGen<Value<'v>> {
    match d {
        DelayExprGen::FixedMs(ms) => DelayExprGen::FixedMs(*ms),
        DelayExprGen::Named(name) => DelayExprGen::Named(name.clone()),
        DelayExprGen::Callable(v) => DelayExprGen::Callable(v.to_value()),
    }
}

/// Polymorphic `delay = ...` argument accepted by `after(...)`, `raise_event(...)`, and `send_to(...)`.
pub struct DelayInput<'v>(pub DelayExprGen<Value<'v>>);

impl<'v> StarlarkTypeRepr for DelayInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::unions(vec![
            i64::starlark_type_repr(),
            SymbolOrEnum::starlark_type_repr(),
            DelayCallable::starlark_type_repr(),
        ])
    }
}

impl<'v> UnpackValue<'v> for DelayInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(ms) = i64::unpack_value_impl(value)? {
            if ms < 0 {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "delay in milliseconds must be >= 0, got {ms}"
                )));
            }
            return Ok(Some(Self(DelayExprGen::FixedMs(ms as u64))));
        }
        if let Some(sym) = SymbolOrEnum::unpack_value_impl(value)? {
            if sym.0.trim().is_empty() {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "delay name must not be empty"
                )));
            }
            return Ok(Some(Self(DelayExprGen::Named(sym.0))));
        }
        if let Ok(Some(callable)) = DelayCallable::unpack_value_impl(value) {
            return Ok(Some(Self(DelayExprGen::Callable(callable.0))));
        }
        Ok(None)
    }
}

/// Action specification in Starlark: either a named host action or a built-in pure statechart action.
#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum ActionDataGen<V: ValueLifetimeless> {
    Named(String),
    Raise {
        event: String,
        #[trace(unsafe_ignore)]
        payload: JsonHolder,
        delay: Option<DelayExprGen<V>>,
        id: Option<String>,
    },
    Cancel {
        send_id: String,
    },
    Emit {
        event: String,
        #[trace(unsafe_ignore)]
        payload: JsonHolder,
    },
    SendTo {
        to: String,
        event: String,
        #[trace(unsafe_ignore)]
        payload: JsonHolder,
        delay: Option<DelayExprGen<V>>,
        id: Option<String>,
    },
    SendParent {
        event: String,
        #[trace(unsafe_ignore)]
        payload: JsonHolder,
    },
    Log {
        #[trace(unsafe_ignore)]
        expr: JsonHolder,
    },
}

unsafe impl<'v> Coerce<ActionDataGen<Value<'v>>> for ActionDataGen<FrozenValue> {}

pub fn clone_action_to_value<'v, V: ValueLike<'v>>(
    act: &ActionDataGen<V>,
) -> ActionDataGen<Value<'v>> {
    match act {
        ActionDataGen::Named(name) => ActionDataGen::Named(name.clone()),
        ActionDataGen::Raise {
            event,
            payload,
            delay,
            id,
        } => ActionDataGen::Raise {
            event: event.clone(),
            payload: payload.clone(),
            delay: delay.as_ref().map(clone_delay_to_value),
            id: id.clone(),
        },
        ActionDataGen::Cancel { send_id } => ActionDataGen::Cancel {
            send_id: send_id.clone(),
        },
        ActionDataGen::Emit { event, payload } => ActionDataGen::Emit {
            event: event.clone(),
            payload: payload.clone(),
        },
        ActionDataGen::SendTo {
            to,
            event,
            payload,
            delay,
            id,
        } => ActionDataGen::SendTo {
            to: to.clone(),
            event: event.clone(),
            payload: payload.clone(),
            delay: delay.as_ref().map(clone_delay_to_value),
            id: id.clone(),
        },
        ActionDataGen::SendParent { event, payload } => ActionDataGen::SendParent {
            event: event.clone(),
            payload: payload.clone(),
        },
        ActionDataGen::Log { expr } => ActionDataGen::Log { expr: expr.clone() },
    }
}

/// Typed Starlark action specification returned by `raise_event`, `cancel_send`, `emit_event`, `send_to`, `send_parent`, `log_action`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkActionGen<V: ValueLifetimeless> {
    pub data: ActionDataGen<V>,
}

starlark_complex_value!(pub StarlarkAction);

impl<V: ValueLifetimeless> Display for StarlarkActionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<ActionSpec>")
    }
}

#[starlark_value(type = "ActionSpec")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkActionGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Polymorphic action item accepted in `entry`, `exit`, and `actions`: `SymbolOrEnum` or `ActionSpec`.
pub struct ActionInput<'v>(pub ActionDataGen<Value<'v>>);

impl<'v> StarlarkTypeRepr for ActionInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(
            SymbolOrEnum::starlark_type_repr(),
            StarlarkAction::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for ActionInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(act) = StarlarkAction::from_value(value) {
            return Ok(Some(Self(clone_action_to_value(&act.data))));
        }
        if let Some(sym) = SymbolOrEnum::unpack_value_impl(value)? {
            return Ok(Some(Self(ActionDataGen::Named(sym.0))));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct TransitionDataGen<V: ValueLifetimeless> {
    pub event: String,
    pub target: Option<String>,
    pub guard: Option<GuardExprGen<V>>,
    pub actions: Vec<ActionDataGen<V>>,
    pub assign: Option<V>,
    pub reenter: bool,
    pub forbidden: bool,
    pub description: Option<String>,
}

unsafe impl<'v> Coerce<TransitionDataGen<Value<'v>>> for TransitionDataGen<FrozenValue> {}

/// Typed Starlark transition value returned by `transition(...)`, `internal_transition(...)`, and `forbid(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkTransitionGen<V: ValueLifetimeless> {
    pub data: TransitionDataGen<V>,
}

starlark_complex_value!(pub StarlarkTransition);

impl<V: ValueLifetimeless> Display for StarlarkTransitionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.data.forbidden {
            return write!(f, "forbid(event={:?})", self.data.event);
        }
        match &self.data.target {
            Some(target) => write!(
                f,
                "transition(event={:?}, target={:?})",
                self.data.event, target
            ),
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
        actions: tr.actions.iter().map(clone_action_to_value).collect(),
        assign: tr.assign.map(|v| v.to_value()),
        reenter: tr.reenter,
        forbidden: tr.forbidden,
        description: tr.description.clone(),
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct OutcomeDataGen<V: ValueLifetimeless> {
    pub target: Option<String>,
    pub guard: Option<GuardExprGen<V>>,
    pub actions: Vec<ActionDataGen<V>>,
    pub assign: Option<V>,
    pub reenter: bool,
    pub description: Option<String>,
}

unsafe impl<'v> Coerce<OutcomeDataGen<Value<'v>>> for OutcomeDataGen<FrozenValue> {}

pub fn clone_outcome_to_value<'v, V: ValueLike<'v>>(
    out: &OutcomeDataGen<V>,
) -> OutcomeDataGen<Value<'v>> {
    OutcomeDataGen {
        target: out.target.clone(),
        guard: out.guard.as_ref().map(clone_guard_expr_to_value),
        actions: out.actions.iter().map(clone_action_to_value).collect(),
        assign: out.assign.map(|v| v.to_value()),
        reenter: out.reenter,
        description: out.description.clone(),
    }
}

/// Typed Starlark outcome branch returned by `outcome(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkOutcomeGen<V: ValueLifetimeless> {
    pub data: OutcomeDataGen<V>,
}

starlark_complex_value!(pub StarlarkOutcome);

impl<V: ValueLifetimeless> Display for StarlarkOutcomeGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data.target {
            Some(target) => write!(f, "outcome(target={target:?})"),
            None => f.write_str("outcome()"),
        }
    }
}

#[starlark_value(type = "Outcome")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkOutcomeGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Single outcome branch input: either a `StateId` shorthand or an `Outcome`.
pub struct OutcomeBranchInput<'v>(pub OutcomeDataGen<Value<'v>>);

impl<'v> StarlarkTypeRepr for OutcomeBranchInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(
            StarlarkStateId::starlark_type_repr(),
            StarlarkOutcome::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for OutcomeBranchInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(sid) = StarlarkStateId::from_value(value) {
            return Ok(Some(Self(OutcomeDataGen {
                target: Some(sid.0.0.clone()),
                guard: None,
                actions: Vec::new(),
                assign: None,
                reenter: false,
                description: None,
            })));
        }
        if let Some(outcome) = StarlarkOutcome::from_value(value) {
            return Ok(Some(Self(clone_outcome_to_value(&outcome.data))));
        }
        Ok(None)
    }
}

/// Polymorphic `on_done` / `on_error` / `always` handler input: accepts `StateId`, `Outcome`, or `list[StateId | Outcome]`.
#[derive(Default)]
pub struct InvokeOutcomesInput<'v>(pub Vec<OutcomeDataGen<Value<'v>>>);

impl<'v> StarlarkTypeRepr for InvokeOutcomesInput<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(
            OutcomeBranchInput::starlark_type_repr(),
            UnpackList::<OutcomeBranchInput>::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for InvokeOutcomesInput<'v> {
    type Error = starlark::Error;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(single) = OutcomeBranchInput::unpack_value_impl(value)? {
            return Ok(Some(Self(vec![single.0])));
        }
        if let Some(list) = UnpackList::<OutcomeBranchInput>::unpack_value_impl(value)? {
            return Ok(Some(Self(list.items.into_iter().map(|i| i.0).collect())));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct AfterDataGen<V: ValueLifetimeless> {
    pub delay: DelayExprGen<V>,
    pub outcomes: Vec<OutcomeDataGen<V>>,
}

unsafe impl<'v> Coerce<AfterDataGen<Value<'v>>> for AfterDataGen<FrozenValue> {}

pub fn clone_after_to_value<'v, V: ValueLike<'v>>(
    a: &AfterDataGen<V>,
) -> AfterDataGen<Value<'v>> {
    AfterDataGen {
        delay: clone_delay_to_value(&a.delay),
        outcomes: a.outcomes.iter().map(clone_outcome_to_value).collect(),
    }
}

/// Typed Starlark delayed transition returned by `after(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkAfterGen<V: ValueLifetimeless> {
    pub data: AfterDataGen<V>,
}

starlark_complex_value!(pub StarlarkAfter);

impl<V: ValueLifetimeless> Display for StarlarkAfterGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data.delay {
            DelayExprGen::FixedMs(ms) => write!(f, "after(delay={ms})"),
            DelayExprGen::Named(name) => write!(f, "after(delay={name:?})"),
            DelayExprGen::Callable(_) => f.write_str("after(delay=<callable>)"),
        }
    }
}

#[starlark_value(type = "After")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkAfterGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Polymorphic `after = ...` parameter on state constructors: accepts `After` or `list[After]`.
#[derive(Default)]
pub struct OneOrManyAfter<'v>(pub Vec<AfterDataGen<Value<'v>>>);

impl<'v> StarlarkTypeRepr for OneOrManyAfter<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(
            StarlarkAfter::starlark_type_repr(),
            UnpackList::<&StarlarkAfter>::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for OneOrManyAfter<'v> {
    type Error = std::convert::Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(single) = StarlarkAfter::from_value(value) {
            return Ok(Some(Self(vec![clone_after_to_value(&single.data)])));
        }
        if let Ok(Some(list)) = UnpackList::<&StarlarkAfter<'v>>::unpack_value_impl(value) {
            return Ok(Some(Self(
                list.items
                    .into_iter()
                    .map(|i| clone_after_to_value(&i.data))
                    .collect(),
            )));
        }
        Ok(None)
    }
}

/// JSON object key used inside `InvokeDef.input` to reference a frozen `(ctx) -> value` Starlark callable slot.
pub const INVOKE_INPUT_SLOT_KEY: &str = "__starness_input_slot__";

pub fn starlark_input_slot_name(slot: usize) -> String {
    format!("__starlark_input_{slot}")
}

/// JSON object key used inside `InvokeDef.input` to reference a compiled MCP `AgentTool` slot.
pub const INVOKE_TOOL_SLOT_KEY: &str = "__starness_tool_slot__";

pub fn starlark_tool_slot_name(slot: usize) -> String {
    format!("__starlark_tool_{slot}")
}

/// Cardinality/overwrite behavior for an [`AgentToolDataGen::Artifact`] MCP tool within a single turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMode {
    /// Latest valid call in the turn overwrites previous value (`artifacts[name]` is an object).
    Replace,
    /// Each valid call appends to `artifacts[name]` (`list[object]`, defaulting to `[]` if uncalled).
    Append,
    /// Only one valid call is permitted per turn; subsequent calls return an MCP tool error.
    Once,
}

impl ArtifactMode {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "replace" => Ok(Self::Replace),
            "append" => Ok(Self::Append),
            "once" => Ok(Self::Once),
            other => anyhow::bail!(
                "invalid `mode` `{other}`: expected `\"replace\"`, `\"append\"`, or `\"once\"`"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
            Self::Once => "once",
        }
    }
}

impl Freeze for ArtifactMode {
    type Frozen = Self;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(self)
    }
}

/// Wrapper around [`TypeShape`] skipping `Allocative` traversal and implementing `Freeze`.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct TypeShapeHolder(#[allocative(skip)] pub TypeShape);

impl Freeze for TypeShapeHolder {
    type Frozen = Self;

    fn freeze(self, _freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(self)
    }
}

/// Turn-scoped MCP tool specification (`artifact_tool` or `read_tool`) attached to `invoke_agent`.
#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum AgentToolDataGen<V: ValueLifetimeless> {
    Artifact {
        name: String,
        description: String,
        #[trace(unsafe_ignore)]
        shape: TypeShapeHolder,
        schema_ctor: V,
        #[trace(unsafe_ignore)]
        mode: ArtifactMode,
        required: bool,
        validate: Option<V>,
    },
    Read {
        name: String,
        description: String,
        #[trace(unsafe_ignore)]
        shape: Option<TypeShapeHolder>,
        schema_ctor: Option<V>,
        handler: V,
    },
}

unsafe impl<'v> Coerce<AgentToolDataGen<Value<'v>>> for AgentToolDataGen<FrozenValue> {}

impl<V: ValueLifetimeless> AgentToolDataGen<V> {
    pub fn name(&self) -> &str {
        match self {
            Self::Artifact { name, .. } | Self::Read { name, .. } => name,
        }
    }
}

pub fn clone_agent_tool_to_value<'v, V: ValueLike<'v>>(
    tool: &AgentToolDataGen<V>,
) -> AgentToolDataGen<Value<'v>> {
    match tool {
        AgentToolDataGen::Artifact {
            name,
            description,
            shape,
            schema_ctor,
            mode,
            required,
            validate,
        } => AgentToolDataGen::Artifact {
            name: name.clone(),
            description: description.clone(),
            shape: shape.clone(),
            schema_ctor: schema_ctor.to_value(),
            mode: *mode,
            required: *required,
            validate: validate.as_ref().map(|v| v.to_value()),
        },
        AgentToolDataGen::Read {
            name,
            description,
            shape,
            schema_ctor,
            handler,
        } => AgentToolDataGen::Read {
            name: name.clone(),
            description: description.clone(),
            shape: shape.clone(),
            schema_ctor: schema_ctor.as_ref().map(|v| v.to_value()),
            handler: handler.to_value(),
        },
    }
}

/// Typed Starlark MCP tool value returned by `artifact_tool(...)` and `read_tool(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkAgentToolGen<V: ValueLifetimeless> {
    pub data: AgentToolDataGen<V>,
}

starlark_complex_value!(pub StarlarkAgentTool);

impl<V: ValueLifetimeless> Display for StarlarkAgentToolGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data {
            AgentToolDataGen::Artifact { name, mode, .. } => {
                write!(f, "artifact_tool(name={name:?}, mode={:?})", mode.as_str())
            }
            AgentToolDataGen::Read { name, .. } => write!(f, "read_tool(name={name:?})"),
        }
    }
}

#[starlark_value(type = "AgentTool")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkAgentToolGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "name" => Some(heap.alloc_str(self.data.name()).to_value()),
            "kind" => {
                let k = match &self.data {
                    AgentToolDataGen::Artifact { .. } => "artifact",
                    AgentToolDataGen::Read { .. } => "read",
                };
                Some(heap.alloc_str(k).to_value())
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["name".to_owned(), "kind".to_owned()]
    }
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub enum InvokeInputGen<V: ValueLifetimeless> {
    Json(#[trace(unsafe_ignore)] JsonHolder),
    Callable(V),
    Array(Vec<InvokeInputGen<V>>),
    Object(Vec<(String, InvokeInputGen<V>)>),
    AgentTool(AgentToolDataGen<V>),
}

unsafe impl<'v> Coerce<InvokeInputGen<Value<'v>>> for InvokeInputGen<FrozenValue> {}

pub fn clone_invoke_input_to_value<'v, V: ValueLike<'v>>(
    inp: &InvokeInputGen<V>,
) -> InvokeInputGen<Value<'v>> {
    match inp {
        InvokeInputGen::Json(j) => InvokeInputGen::Json(j.clone()),
        InvokeInputGen::Callable(v) => InvokeInputGen::Callable(v.to_value()),
        InvokeInputGen::Array(items) => {
            InvokeInputGen::Array(items.iter().map(clone_invoke_input_to_value).collect())
        }
        InvokeInputGen::Object(entries) => InvokeInputGen::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), clone_invoke_input_to_value(v)))
                .collect(),
        ),
        InvokeInputGen::AgentTool(t) => InvokeInputGen::AgentTool(clone_agent_tool_to_value(t)),
    }
}

pub fn unpack_invoke_input_value<'v>(value: Value<'v>) -> anyhow::Result<InvokeInputGen<Value<'v>>> {
    if value.is_none() {
        return Ok(InvokeInputGen::Json(JsonHolder(serde_json::Value::Null)));
    }
    if let Some(tool) = StarlarkAgentTool::from_value(value) {
        return Ok(InvokeInputGen::AgentTool(clone_agent_tool_to_value(
            &tool.data,
        )));
    }
    if let Some(dict) = DictRef::from_value(value) {
        let mut entries = Vec::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let key = k
                .unpack_str()
                .ok_or_else(|| anyhow::anyhow!("invoke `input` dict keys must be strings, got `{k}`"))?
                .to_owned();
            entries.push((key, unpack_invoke_input_value(v)?));
        }
        return Ok(InvokeInputGen::Object(entries));
    }
    if let Some(list) = ListRef::from_value(value) {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(unpack_invoke_input_value(item)?);
        }
        return Ok(InvokeInputGen::Array(items));
    }
    if Record::from_value(value).is_none()
        && EnumValue::from_value(value).is_none()
        && StarlarkCallable::<starlark::values::typing::StarlarkCallableParamAny, FrozenValue>::unpack_value_impl(value)
            .ok()
            .flatten()
            .is_some()
    {
        return Ok(InvokeInputGen::Callable(value));
    }
    Ok(InvokeInputGen::Json(JsonHolder(value.to_json_value()?)))
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct InvokeDataGen<V: ValueLifetimeless> {
    pub id: Option<String>,
    pub src: String,
    pub input: InvokeInputGen<V>,
    pub on_done: Vec<OutcomeDataGen<V>>,
    pub on_error: Vec<OutcomeDataGen<V>>,
}

unsafe impl<'v> Coerce<InvokeDataGen<Value<'v>>> for InvokeDataGen<FrozenValue> {}

pub fn clone_invoke_to_value<'v, V: ValueLike<'v>>(
    inv: &InvokeDataGen<V>,
) -> InvokeDataGen<Value<'v>> {
    InvokeDataGen {
        id: inv.id.clone(),
        src: inv.src.clone(),
        input: clone_invoke_input_to_value(&inv.input),
        on_done: inv.on_done.iter().map(clone_outcome_to_value).collect(),
        on_error: inv.on_error.iter().map(clone_outcome_to_value).collect(),
    }
}

/// Typed Starlark service invocation returned by `invoke(...)`.
#[derive(Debug, Clone, Coerce, Trace, Freeze, ProvidesStaticType, NoSerialize, Allocative)]
#[repr(C)]
pub struct StarlarkInvokeGen<V: ValueLifetimeless> {
    pub data: InvokeDataGen<V>,
}

starlark_complex_value!(pub StarlarkInvoke);

impl<V: ValueLifetimeless> Display for StarlarkInvokeGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invoke(src={:?})", self.data.src)
    }
}

#[starlark_value(type = "Invoke")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkInvokeGen<V> where
    Self: ProvidesStaticType<'v>
{
}

/// Polymorphic `invoke = ...` parameter on state constructors: accepts `Invoke` or `list[Invoke]`.
#[derive(Default)]
pub struct OneOrManyInvoke<'v>(pub Vec<InvokeDataGen<Value<'v>>>);

impl<'v> StarlarkTypeRepr for OneOrManyInvoke<'v> {
    type Canonical = Self;

    fn starlark_type_repr() -> Ty {
        Ty::union2(
            StarlarkInvoke::starlark_type_repr(),
            UnpackList::<&StarlarkInvoke>::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for OneOrManyInvoke<'v> {
    type Error = std::convert::Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        if let Some(single) = StarlarkInvoke::from_value(value) {
            return Ok(Some(Self(vec![clone_invoke_to_value(&single.data)])));
        }
        if let Ok(Some(list)) = UnpackList::<&StarlarkInvoke<'v>>::unpack_value_impl(value) {
            return Ok(Some(Self(
                list.items
                    .into_iter()
                    .map(|i| clone_invoke_to_value(&i.data))
                    .collect(),
            )));
        }
        Ok(None)
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
    History {
        deep: bool,
        default_target: Option<String>,
    },
}

#[derive(Debug, Clone, Trace, Freeze, Allocative)]
pub struct StateNodeDataGen<V: ValueLifetimeless> {
    pub kind: NodeKindDataGen<V>,
    pub entry: Vec<ActionDataGen<V>>,
    pub exit: Vec<ActionDataGen<V>>,
    pub on: Vec<TransitionDataGen<V>>,
    pub always: Vec<OutcomeDataGen<V>>,
    pub after: Vec<AfterDataGen<V>>,
    pub invoke: Vec<InvokeDataGen<V>>,
    pub on_done: Vec<OutcomeDataGen<V>>,
    #[trace(unsafe_ignore)]
    pub output: Option<JsonHolder>,
    pub tags: Vec<String>,
    #[trace(unsafe_ignore)]
    pub meta: Option<JsonHolder>,
    pub description: Option<String>,
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
        NodeKindDataGen::History {
            deep,
            default_target,
        } => NodeKindDataGen::History {
            deep: *deep,
            default_target: default_target.clone(),
        },
    };
    StateNodeDataGen {
        kind,
        entry: node.entry.iter().map(clone_action_to_value).collect(),
        exit: node.exit.iter().map(clone_action_to_value).collect(),
        on: node.on.iter().map(clone_transition_to_value).collect(),
        always: node.always.iter().map(clone_outcome_to_value).collect(),
        after: node.after.iter().map(clone_after_to_value).collect(),
        invoke: node.invoke.iter().map(clone_invoke_to_value).collect(),
        on_done: node.on_done.iter().map(clone_outcome_to_value).collect(),
        output: node.output.clone(),
        tags: node.tags.clone(),
        meta: node.meta.clone(),
        description: node.description.clone(),
    }
}

/// Typed Starlark state node returned by `state`, `compound_state`, `parallel_state`, `final_state`, `shallow_history`, and `deep_history`.
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
            NodeKindDataGen::History { deep: false, .. } => f.write_str("shallow_history(...)"),
            NodeKindDataGen::History { deep: true, .. } => f.write_str("deep_history(...)"),
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
    pub delay_slots: Vec<V>,
    pub input_slots: Vec<V>,
    pub tool_slots: Vec<AgentToolDataGen<V>>,
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
            delay_slots: self.delay_slots.freeze(freezer)?,
            input_slots: self.input_slots.freeze(freezer)?,
            tool_slots: self.tool_slots.freeze(freezer)?,
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
    output: Option<serde_json::Value>,
    description: Option<String>,
    entries: Vec<(StateId, StateNodeDataGen<Value<'v>>)>,
) -> Result<StarlarkMachine<'v>, Vec<IrValidationError>> {
    let mut guard_slots: Vec<Value<'v>> = Vec::new();
    let mut reducer_slots: Vec<Value<'v>> = Vec::new();
    let mut delay_slots: Vec<Value<'v>> = Vec::new();
    let mut input_slots: Vec<Value<'v>> = Vec::new();
    let mut tool_slots: Vec<AgentToolDataGen<Value<'v>>> = Vec::new();

    let mut states = IndexMap::new();
    for (state_id, node_data) in entries {
        let ir_node = lower_node(
            node_data,
            &mut guard_slots,
            &mut reducer_slots,
            &mut delay_slots,
            &mut input_slots,
            &mut tool_slots,
        );
        states.insert(state_id, ir_node);
    }

    let machine_def = MachineDef {
        id,
        initial,
        context,
        states,
        output,
        description,
    };

    machine_def.validate()?;

    Ok(StarlarkMachine {
        ir: IrHolder(machine_def),
        guard_slots,
        reducer_slots,
        delay_slots,
        input_slots,
        tool_slots,
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
        GuardExprGen::StateIn(sid) => GuardRef::StateIn(StateId(sid)),
        GuardExprGen::AllOf(items) => GuardRef::AllOf(
            items
                .into_iter()
                .map(|g| lower_guard(g, guard_slots))
                .collect(),
        ),
        GuardExprGen::AnyOf(items) => GuardRef::AnyOf(
            items
                .into_iter()
                .map(|g| lower_guard(g, guard_slots))
                .collect(),
        ),
        GuardExprGen::Not(inner) => GuardRef::Not(Box::new(lower_guard(*inner, guard_slots))),
    }
}

fn lower_delay<'v>(expr: DelayExprGen<Value<'v>>, delay_slots: &mut Vec<Value<'v>>) -> DelayRef {
    match expr {
        DelayExprGen::FixedMs(ms) => DelayRef::FixedMs(ms),
        DelayExprGen::Named(name) => DelayRef::Named(name),
        DelayExprGen::Callable(val) => {
            let slot = delay_slots.len();
            delay_slots.push(val);
            DelayRef::StarlarkSlot(slot)
        }
    }
}

fn lower_action<'v>(act: ActionDataGen<Value<'v>>, delay_slots: &mut Vec<Value<'v>>) -> ActionDef {
    match act {
        ActionDataGen::Named(name) => ActionDef::Named(name),
        ActionDataGen::Raise {
            event,
            payload,
            delay,
            id,
        } => ActionDef::Raise {
            event,
            payload: payload.0,
            delay: delay.map(|d| lower_delay(d, delay_slots)),
            id,
        },
        ActionDataGen::Cancel { send_id } => ActionDef::Cancel { send_id },
        ActionDataGen::Emit { event, payload } => ActionDef::Emit {
            event,
            payload: payload.0,
        },
        ActionDataGen::SendTo {
            to,
            event,
            payload,
            delay,
            id,
        } => ActionDef::SendTo {
            to,
            event,
            payload: payload.0,
            delay: delay.map(|d| lower_delay(d, delay_slots)),
            id,
        },
        ActionDataGen::SendParent { event, payload } => ActionDef::SendParent {
            event,
            payload: payload.0,
        },
        ActionDataGen::Log { expr } => ActionDef::Log { expr: expr.0 },
    }
}

fn lower_transition<'v>(
    tr: TransitionDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
    delay_slots: &mut Vec<Value<'v>>,
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
        actions: tr
            .actions
            .into_iter()
            .map(|a| lower_action(a, delay_slots))
            .collect(),
        assign,
        reenter: tr.reenter,
        forbidden: tr.forbidden,
        description: tr.description,
    }
}

fn lower_outcome<'v>(
    out: OutcomeDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
    delay_slots: &mut Vec<Value<'v>>,
) -> OutcomeDef {
    let guard = out.guard.map(|g| lower_guard(g, guard_slots));
    let assign = out.assign.map(|val| {
        let slot = reducer_slots.len();
        reducer_slots.push(val);
        AssignRef::StarlarkSlot(slot)
    });
    OutcomeDef {
        target: out.target.map(StateId),
        guard,
        actions: out
            .actions
            .into_iter()
            .map(|a| lower_action(a, delay_slots))
            .collect(),
        assign,
        reenter: out.reenter,
        description: out.description,
    }
}

fn lower_invoke_input<'v>(
    inp: InvokeInputGen<Value<'v>>,
    input_slots: &mut Vec<Value<'v>>,
    tool_slots: &mut Vec<AgentToolDataGen<Value<'v>>>,
) -> serde_json::Value {
    match inp {
        InvokeInputGen::Json(j) => j.0,
        InvokeInputGen::Callable(val) => {
            let slot = input_slots.len();
            input_slots.push(val);
            serde_json::json!({
                INVOKE_INPUT_SLOT_KEY: starlark_input_slot_name(slot),
            })
        }
        InvokeInputGen::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|i| lower_invoke_input(i, input_slots, tool_slots))
                .collect(),
        ),
        InvokeInputGen::Object(entries) => {
            let mut map = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                map.insert(k, lower_invoke_input(v, input_slots, tool_slots));
            }
            serde_json::Value::Object(map)
        }
        InvokeInputGen::AgentTool(tool) => {
            let slot = tool_slots.len();
            tool_slots.push(tool);
            serde_json::json!({
                INVOKE_TOOL_SLOT_KEY: starlark_tool_slot_name(slot),
            })
        }
    }
}

fn lower_invoke<'v>(
    inv: InvokeDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
    delay_slots: &mut Vec<Value<'v>>,
    input_slots: &mut Vec<Value<'v>>,
    tool_slots: &mut Vec<AgentToolDataGen<Value<'v>>>,
) -> InvokeDef {
    InvokeDef {
        id: inv.id,
        src: inv.src,
        input: lower_invoke_input(inv.input, input_slots, tool_slots),
        on_done: inv
            .on_done
            .into_iter()
            .map(|o| lower_outcome(o, guard_slots, reducer_slots, delay_slots))
            .collect(),
        on_error: inv
            .on_error
            .into_iter()
            .map(|o| lower_outcome(o, guard_slots, reducer_slots, delay_slots))
            .collect(),
    }
}

fn lower_after<'v>(
    after: AfterDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
    delay_slots: &mut Vec<Value<'v>>,
) -> AfterDef {
    let delay = lower_delay(after.delay, delay_slots);
    let outcomes = after
        .outcomes
        .into_iter()
        .map(|o| lower_outcome(o, guard_slots, reducer_slots, delay_slots))
        .collect();
    AfterDef { delay, outcomes }
}

#[allow(clippy::too_many_arguments)]
fn lower_node<'v>(
    node: StateNodeDataGen<Value<'v>>,
    guard_slots: &mut Vec<Value<'v>>,
    reducer_slots: &mut Vec<Value<'v>>,
    delay_slots: &mut Vec<Value<'v>>,
    input_slots: &mut Vec<Value<'v>>,
    tool_slots: &mut Vec<AgentToolDataGen<Value<'v>>>,
) -> StateNodeDef {
    let kind = match node.kind {
        NodeKindDataGen::Atomic => StateKind::Atomic,
        NodeKindDataGen::Compound { initial, children } => {
            let mut map = IndexMap::new();
            for (child_id, child_node) in children {
                map.insert(
                    StateId(child_id),
                    lower_node(
                        child_node,
                        guard_slots,
                        reducer_slots,
                        delay_slots,
                        input_slots,
                        tool_slots,
                    ),
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
                    lower_node(
                        child_node,
                        guard_slots,
                        reducer_slots,
                        delay_slots,
                        input_slots,
                        tool_slots,
                    ),
                );
            }
            StateKind::Parallel { states: map }
        }
        NodeKindDataGen::Final => StateKind::Final,
        NodeKindDataGen::History {
            deep,
            default_target,
        } => StateKind::History {
            deep,
            default_target: default_target.map(StateId),
        },
    };

    let entry = node
        .entry
        .into_iter()
        .map(|a| lower_action(a, delay_slots))
        .collect();
    let exit = node
        .exit
        .into_iter()
        .map(|a| lower_action(a, delay_slots))
        .collect();
    let on = node
        .on
        .into_iter()
        .map(|tr| lower_transition(tr, guard_slots, reducer_slots, delay_slots))
        .collect();
    let always = node
        .always
        .into_iter()
        .map(|o| lower_outcome(o, guard_slots, reducer_slots, delay_slots))
        .collect();
    let after = node
        .after
        .into_iter()
        .map(|a| lower_after(a, guard_slots, reducer_slots, delay_slots))
        .collect();
    let invoke = node
        .invoke
        .into_iter()
        .map(|inv| lower_invoke(inv, guard_slots, reducer_slots, delay_slots, input_slots, tool_slots))
        .collect();
    let on_done = node
        .on_done
        .into_iter()
        .map(|o| lower_outcome(o, guard_slots, reducer_slots, delay_slots))
        .collect();

    StateNodeDef {
        kind,
        entry,
        exit,
        on,
        always,
        after,
        invoke,
        on_done,
        output: node.output.map(|j| j.0),
        tags: node.tags,
        meta: node.meta.map(|j| j.0),
        description: node.description,
    }
}

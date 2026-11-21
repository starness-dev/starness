//! Derives JSON Schema (`inputSchema`) from Starlark `record(...)` / `enum(...)` types
//! and instantiates validated Starlark `Record` values from MCP JSON tool arguments.

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, bail};
use serde_json::{Map, Value as JsonValue, json};
use starlark::environment::{FrozenModule, Module};
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::record::Record;
use starlark::values::typing::TypeCompiled;
use starlark::values::{Heap, OwnedFrozenValue, Value};

thread_local! {
    static ACTIVE_TYPE_REGISTRY: RefCell<Option<TypeRegistry>> = const { RefCell::new(None) };
}

/// Installs `registry` on the current thread while evaluating Starlark guards, reducers, or tools
/// so helpers like `update(ctx, ...)` and `artifact(event, ...)` can construct typed `Record`s.
pub fn with_active_type_registry<R>(registry: &TypeRegistry, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<TypeRegistry>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let prev = self.0.take();
            ACTIVE_TYPE_REGISTRY.with(|slot| *slot.borrow_mut() = prev);
        }
    }
    let prev = ACTIVE_TYPE_REGISTRY.with(|slot| slot.borrow_mut().replace(registry.clone()));
    let _reset = Reset(prev);
    f()
}

/// Parsed structural representation of a Starlark type expression for JSON Schema & runtime coercion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeShape {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Any,
    List(Box<TypeShape>),
    Dict(Box<TypeShape>),
    Union(Vec<TypeShape>),
    Enum(Vec<String>),
    Record(Vec<RecordFieldShape>),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFieldShape {
    pub name: String,
    pub shape: TypeShape,
    pub has_default: bool,
}

impl TypeShape {
    pub fn allows_null(&self, defs: &HashMap<String, TypeShape>) -> bool {
        match self {
            Self::Null | Self::Any => true,
            Self::Union(items) => items.iter().any(|i| i.allows_null(defs)),
            Self::Named(name) => defs.get(name).is_some_and(|d| d.allows_null(defs)),
            _ => false,
        }
    }

    pub fn to_json_schema(&self, defs: &HashMap<String, TypeShape>) -> JsonValue {
        match self {
            Self::Null => json!({ "type": "null" }),
            Self::Bool => json!({ "type": "boolean" }),
            Self::Int => json!({ "type": "integer" }),
            Self::Float => json!({ "type": "number" }),
            Self::Str => json!({ "type": "string" }),
            Self::Any => json!({}),
            Self::List(item) => json!({
                "type": "array",
                "items": item.to_json_schema(defs),
            }),
            Self::Dict(val) => json!({
                "type": "object",
                "additionalProperties": val.to_json_schema(defs),
            }),
            Self::Enum(variants) => json!({
                "type": "string",
                "enum": variants,
            }),
            Self::Union(items) => {
                let mut non_null = Vec::new();
                let mut has_null = false;
                for item in items {
                    if matches!(item, Self::Null) {
                        has_null = true;
                    } else {
                        non_null.push(item.to_json_schema(defs));
                    }
                }
                if has_null && non_null.len() == 1 {
                    let mut single = non_null.into_iter().next().unwrap();
                    if let Some(obj) = single.as_object_mut()
                        && let Some(JsonValue::String(t)) = obj.get("type").cloned()
                        && !obj.contains_key("enum")
                    {
                        obj.insert("type".to_owned(), json!([t, "null"]));
                        return single;
                    }
                    return json!({ "anyOf": [single, { "type": "null" }] });
                }
                let mut all = non_null;
                if has_null {
                    all.push(json!({ "type": "null" }));
                }
                json!({ "anyOf": all })
            }
            Self::Record(fields) => {
                let mut props = Map::new();
                let mut required = Vec::new();
                for f in fields {
                    props.insert(f.name.clone(), f.shape.to_json_schema(defs));
                    if !f.has_default && !f.shape.allows_null(defs) {
                        required.push(JsonValue::String(f.name.clone()));
                    }
                }
                json!({
                    "type": "object",
                    "properties": props,
                    "required": required,
                    "additionalProperties": false,
                })
            }
            Self::Named(name) => match defs.get(name) {
                Some(resolved) => resolved.to_json_schema(defs),
                None => json!({ "type": "string" }),
            },
        }
    }
}

/// Registry of frozen Starlark `record(...)` and `enum(...)` type constructors in a compiled chart.
#[derive(Debug, Clone, Default)]
pub struct TypeRegistry {
    pub shapes: HashMap<String, TypeShape>,
    pub ctors: HashMap<String, OwnedFrozenValue>,
}

impl TypeRegistry {
    /// Scans all loaded frozen modules for `record(...)` and `enum(...)` constructors.
    pub fn from_frozen_modules(modules: &[FrozenModule]) -> Self {
        let mut shapes = HashMap::new();
        let mut ctors = HashMap::new();

        for m in modules {
            for sym in m.names() {
                let name = sym.as_str();
                let Ok(Some(owned)) = m.get_option(name) else {
                    continue;
                };
                let rendered = owned.value().to_string();
                if (rendered.starts_with("record(") || rendered.starts_with("enum("))
                    && let Ok(shape) = parse_type_expr(&rendered)
                {
                    shapes.insert(name.to_owned(), shape);
                    ctors.insert(name.to_owned(), owned);
                }
            }
        }

        Self { shapes, ctors }
    }

    /// Resolves a `TypeShape` to its underlying `Record` or `Enum` definition if named.
    pub fn resolve<'a>(&'a self, shape: &'a TypeShape) -> &'a TypeShape {
        match shape {
            TypeShape::Named(name) => self.shapes.get(name).unwrap_or(shape),
            other => other,
        }
    }
}

/// Validates that `schema_val` is a Starlark `record(...)` type constructor and parses its [`TypeShape`].
pub fn parse_record_schema_value<'v>(
    schema_val: Value<'v>,
    heap: Heap<'v>,
) -> anyhow::Result<TypeShape> {
    let _compiled = TypeCompiled::new(schema_val, heap)
        .map_err(|e| anyhow::anyhow!("invalid `schema`: {e:#}"))?;
    let rendered = schema_val.to_string();
    if !rendered.starts_with("record(") {
        bail!(
            "`schema` must be a `record(...)` type constructor, got `{}`",
            schema_val.get_type()
        );
    }
    parse_type_expr(&rendered)
}

/// Parses a Starlark `record(...)`, `enum(...)`, or `Ty` display string into a [`TypeShape`].
pub fn parse_type_expr(raw: &str) -> anyhow::Result<TypeShape> {
    let s = raw.trim();
    if s == "None" || s == "NoneType" {
        return Ok(TypeShape::Null);
    }
    if s == "bool" {
        return Ok(TypeShape::Bool);
    }
    if s == "int" {
        return Ok(TypeShape::Int);
    }
    if s == "float" {
        return Ok(TypeShape::Float);
    }
    if s == "str" || s == "string" {
        return Ok(TypeShape::Str);
    }
    if s == "typing.Any" || s == "Any" || s == "object" {
        return Ok(TypeShape::Any);
    }

    // `record(a=field(str), b=field(int, 0), ...)`
    if let Some(inner) = strip_delimited(s, "record", '(', ')') {
        let mut fields = Vec::new();
        if !inner.trim().is_empty() {
            for entry in split_top_level(inner, ',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let Some((field_name, rhs)) = split_once_top_level(entry, '=') else {
                    bail!("unexpected record field `{entry}` in `{s}`");
                };
                let rhs = rhs.trim();
                let (ty_str, has_default) = if let Some(f_inner) = strip_delimited(rhs, "field", '(', ')')
                {
                    let parts = split_top_level(f_inner, ',');
                    if parts.is_empty() {
                        bail!("empty `field()` in `{s}`");
                    }
                    (parts[0].trim(), parts.len() > 1)
                } else {
                    (rhs, false)
                };
                fields.push(RecordFieldShape {
                    name: field_name.trim().to_owned(),
                    shape: parse_type_expr(ty_str)?,
                    has_default,
                });
            }
        }
        return Ok(TypeShape::Record(fields));
    }

    // `enum("A", "B", ...)`
    if let Some(inner) = strip_delimited(s, "enum", '(', ')') {
        let mut variants = Vec::new();
        for part in split_top_level(inner, ',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if let Ok(JsonValue::String(unquoted)) = serde_json::from_str::<JsonValue>(p) {
                variants.push(unquoted);
            } else {
                variants.push(p.trim_matches('"').to_owned());
            }
        }
        return Ok(TypeShape::Enum(variants));
    }

    // Top-level union `A | B`
    let union_parts = split_top_level(s, '|');
    if union_parts.len() > 1 {
        let mut items = Vec::with_capacity(union_parts.len());
        for part in union_parts {
            items.push(parse_type_expr(part.trim())?);
        }
        return Ok(TypeShape::Union(items));
    }

    if s == "list" {
        return Ok(TypeShape::List(Box::new(TypeShape::Any)));
    }
    if let Some(inner) = strip_delimited(s, "list", '[', ']') {
        return Ok(TypeShape::List(Box::new(parse_type_expr(inner)?)));
    }

    if s == "dict" {
        return Ok(TypeShape::Dict(Box::new(TypeShape::Any)));
    }
    if let Some(inner) = strip_delimited(s, "dict", '[', ']') {
        let kv = split_top_level(inner, ',');
        if kv.len() == 2 {
            return Ok(TypeShape::Dict(Box::new(parse_type_expr(kv[1].trim())?)));
        }
        return Ok(TypeShape::Dict(Box::new(TypeShape::Any)));
    }

    Ok(TypeShape::Named(s.to_owned()))
}

/// Instantiates a validated Starlark `Record` from `args_json` using `root_ctor`, `root_shape`, and `registry`.
///
/// Supports defaulted fields, omitted nullable fields (`T | None`), nested `record(...)` fields,
/// and `enum(...)` string variants.
pub fn instantiate_record_with_registry<'v>(
    module: &Module<'v>,
    root_ctor: &OwnedFrozenValue,
    root_shape: &TypeShape,
    registry: &TypeRegistry,
    args_json: &JsonValue,
) -> anyhow::Result<Value<'v>> {
    let obj = args_json
        .as_object()
        .context("tool arguments must be a JSON object")?;
    build_record_value(module, root_ctor, root_shape, registry, obj)
}

fn build_record_value<'v>(
    module: &Module<'v>,
    ctor: &OwnedFrozenValue,
    shape: &TypeShape,
    registry: &TypeRegistry,
    obj: &Map<String, JsonValue>,
) -> anyhow::Result<Value<'v>> {
    let TypeShape::Record(fields) = registry.resolve(shape) else {
        bail!("expected record schema, got `{shape:?}`");
    };

    let mut named_vals: Vec<(String, Value<'v>)> = Vec::with_capacity(obj.len() + fields.len());
    let mut seen = std::collections::HashSet::new();

    for (k, v) in obj {
        seen.insert(k.as_str());
        let coerced = if let Some(field_def) = fields.iter().find(|f| &f.name == k) {
            coerce_json_for_shape(module, &field_def.shape, registry, v)
                .with_context(|| format!("invalid field `{k}`"))?
        } else {
            // Pass unknown fields through so Starlark's record constructor reports an exact extra-parameter error.
            alloc_plain_json(v, module.heap())
        };
        named_vals.push((k.clone(), coerced));
    }

    // Fill omitted nullable fields (`T | None` without explicit default) with `None` so callers can omit them cleanly.
    for field_def in fields {
        if !seen.contains(field_def.name.as_str())
            && !field_def.has_default
            && field_def.shape.allows_null(&registry.shapes)
        {
            named_vals.push((field_def.name.clone(), Value::new_none()));
        }
    }

    // SAFETY: `module.frozen_heap()` retains a reference to `ctor.owner()` for `'v`.
    let ctor_val = unsafe { ctor.owned_frozen_value(module.frozen_heap()) }.to_value();
    let named_refs: Vec<(&str, Value<'v>)> = named_vals
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    let mut eval = Evaluator::new(module);
    eval.eval_function(ctor_val, &[], &named_refs)
        .map_err(|e| anyhow::anyhow!("{e:#}"))
}

fn coerce_json_for_shape<'v>(
    module: &Module<'v>,
    shape: &TypeShape,
    registry: &TypeRegistry,
    val: &JsonValue,
) -> anyhow::Result<Value<'v>> {
    if val.is_null() {
        return Ok(Value::new_none());
    }
    match shape {
        TypeShape::Named(name) => {
            if let (Some(target_shape), Some(ctor)) =
                (registry.shapes.get(name), registry.ctors.get(name))
            {
                match target_shape {
                    TypeShape::Record(_) => {
                        let obj = val
                            .as_object()
                            .with_context(|| format!("expected object for record `{name}`"))?;
                        return build_record_value(module, ctor, target_shape, registry, obj);
                    }
                    TypeShape::Enum(_) => {
                        let s = val
                            .as_str()
                            .with_context(|| format!("expected string for enum `{name}`"))?;
                        // SAFETY: `module.frozen_heap()` retains a reference to `ctor.owner()` for `'v`.
                        let ctor_val =
                            unsafe { ctor.owned_frozen_value(module.frozen_heap()) }.to_value();
                        let arg = module.heap().alloc_str(s).to_value();
                        let mut eval = Evaluator::new(module);
                        return eval
                            .eval_function(ctor_val, &[arg], &[])
                            .map_err(|e| anyhow::anyhow!("{e:#}"));
                    }
                    other => return coerce_json_for_shape(module, other, registry, val),
                }
            }
            Ok(alloc_plain_json(val, module.heap()))
        }
        TypeShape::List(item_shape) => {
            let Some(arr) = val.as_array() else {
                return Ok(alloc_plain_json(val, module.heap()));
            };
            let mut items = Vec::with_capacity(arr.len());
            for (idx, elem) in arr.iter().enumerate() {
                items.push(
                    coerce_json_for_shape(module, item_shape, registry, elem)
                        .with_context(|| format!("at index [{idx}]"))?,
                );
            }
            Ok(module.heap().alloc(AllocList(items)))
        }
        TypeShape::Dict(val_shape) => {
            let Some(map) = val.as_object() else {
                return Ok(alloc_plain_json(val, module.heap()));
            };
            let mut entries = Vec::with_capacity(map.len());
            for (k, v) in map {
                let cv = coerce_json_for_shape(module, val_shape, registry, v)
                    .with_context(|| format!("at key `{k}`"))?;
                entries.push((k.as_str(), cv));
            }
            Ok(module.heap().alloc(AllocDict(entries)))
        }
        TypeShape::Union(variants) => {
            for variant in variants {
                if matches!(variant, TypeShape::Null) {
                    continue;
                }
                if let Ok(coerced) = coerce_json_for_shape(module, variant, registry, val) {
                    return Ok(coerced);
                }
            }
            Ok(alloc_plain_json(val, module.heap()))
        }
        _ => Ok(alloc_plain_json(val, module.heap())),
    }
}

pub fn alloc_plain_json<'v>(json: &JsonValue, heap: Heap<'v>) -> Value<'v> {
    match json {
        JsonValue::Null => Value::new_none(),
        JsonValue::Bool(b) => Value::new_bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i)
            } else if let Some(u) = n.as_u64() {
                heap.alloc(u)
            } else {
                heap.alloc(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(s) => heap.alloc_str(s).to_value(),
        JsonValue::Array(items) => {
            heap.alloc(AllocList(items.iter().map(|v| alloc_plain_json(v, heap))))
        }
        JsonValue::Object(map) => heap.alloc(AllocDict(
            map.iter()
                .map(|(k, v)| (k.as_str(), alloc_plain_json(v, heap))),
        )),
    }
}

fn strip_delimited<'a>(s: &'a str, prefix: &str, open: char, close: char) -> Option<&'a str> {
    let rest = s.strip_prefix(prefix)?.trim_start();
    let inner = rest.strip_prefix(open)?.strip_suffix(close)?;
    Some(inner)
}

fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut start = 0usize;

    for (idx, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

fn split_once_top_level(s: &str, sep: char) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                return Some((&s[..idx], &s[idx + ch.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

/// Returns a shallow-updated copy of a Starlark `record` or `dict`, validating field names and types.
pub fn update_record_or_dict<'v>(
    target: Value<'v>,
    overrides: Vec<(&'v str, Value<'v>)>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    if let Some(dict) = DictRef::from_value(target) {
        let mut entries: Vec<(String, Value<'v>)> = Vec::with_capacity(dict.len() + overrides.len());
        let mut override_map: HashMap<&str, Value<'v>> = HashMap::with_capacity(overrides.len());
        for (k, v) in overrides {
            override_map.insert(k, v);
        }
        for (k, v) in dict.iter() {
            let Some(ks) = k.unpack_str() else {
                bail!("`update(...)` on a dict requires string keys, got `{k}`");
            };
            if let Some(ov) = override_map.remove(ks) {
                entries.push((ks.to_owned(), ov));
            } else {
                entries.push((ks.to_owned(), v));
            }
        }
        for (k, v) in override_map {
            entries.push((k.to_owned(), v));
        }
        return Ok(eval
            .heap()
            .alloc(AllocDict(entries.iter().map(|(k, v)| (k.as_str(), *v)))));
    }

    if Record::from_value(target).is_some() {
        let field_names = target.dir_attr();
        let mut override_map: HashMap<&str, Value<'v>> = HashMap::with_capacity(overrides.len());
        for (k, v) in overrides {
            if !field_names.iter().any(|f| f == k) {
                bail!(
                    "unknown field `{k}` in `update(...)` (expected one of: {})",
                    field_names.join(", ")
                );
            }
            override_map.insert(k, v);
        }

        let mut merged: Vec<(String, Value<'v>)> = Vec::with_capacity(field_names.len());
        for fname in &field_names {
            let val = if let Some(ov) = override_map.remove(fname.as_str()) {
                ov
            } else {
                target
                    .get_attr(fname, eval.heap())
                    .map_err(|e| anyhow::anyhow!("{e:#}"))?
                    .ok_or_else(|| anyhow::anyhow!("missing record attribute `{fname}`"))?
            };
            merged.push((fname.clone(), val));
        }

        if let Some(ctor_val) = find_record_ctor_for_instance(target, eval) {
            let named_refs: Vec<(&str, Value<'v>)> =
                merged.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            return eval
                .eval_function(ctor_val, &[], &named_refs)
                .map_err(|e| anyhow::anyhow!("{e:#}"));
        }

        return Ok(eval
            .heap()
            .alloc(AllocDict(merged.iter().map(|(k, v)| (k.as_str(), *v)))));
    }

    bail!(
        "`update(...)` expects a `record` or `dict` as its first argument, got `{}`",
        target.get_type()
    )
}

fn find_record_ctor_for_instance<'v>(
    instance: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> Option<Value<'v>> {
    // 1. Check active runtime TypeRegistry (covers multi-file imported records too).
    let from_reg = ACTIVE_TYPE_REGISTRY.with(|slot| {
        let borrow = slot.borrow();
        let reg = borrow.as_ref()?;
        for ctor in reg.ctors.values() {
            // SAFETY: `eval.frozen_heap()` retains a reference to `ctor.owner()` for `'v`.
            let candidate = unsafe { ctor.owned_frozen_value(eval.frozen_heap()) }.to_value();
            if candidate.to_string().starts_with("record(")
                && let Ok(tc) = TypeCompiled::new(candidate, eval.heap())
                && tc.matches(instance)
            {
                return Some(candidate);
            }
        }
        None
    });
    if from_reg.is_some() {
        return from_reg;
    }

    // 2. Fallback: scan the active Starlark module during top-level compile evaluation.
    for sym in eval.module().names() {
        if let Some(candidate) = eval.module().get(sym.as_str())
            && candidate.to_string().starts_with("record(")
            && let Ok(tc) = TypeCompiled::new(candidate, eval.heap())
            && tc.matches(instance)
        {
            return Some(candidate);
        }
    }
    None
}

/// Compiles an optional `@starness/llm` `schema` (`None`, raw JSON Schema `dict`, or Starlark
/// `record(...)` type constructor) into `None` or `{"name": str, "schema": dict}`.
pub fn derive_llm_schema_in_eval<'v>(
    schema_val: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    if schema_val.is_none() {
        return Ok(Value::new_none());
    }
    if DictRef::from_value(schema_val).is_some() {
        let json_val = schema_val.to_json_value()?;
        let Some(map) = json_val.as_object() else {
            bail!("invoke_llm `schema` dict must serialize to a JSON object");
        };
        if map.is_empty() {
            bail!("invoke_llm `schema` dict must not be empty");
        }
        let schema_name = map
            .get("title")
            .and_then(JsonValue::as_str)
            .and_then(sanitize_json_spec_name)
            .unwrap_or_else(|| "structured_output".to_owned());
        return Ok(alloc_plain_json(
            &json!({
                "name": schema_name,
                "schema": json_val,
            }),
            eval.heap(),
        ));
    }

    let shape = parse_record_schema_value(schema_val, eval.heap())?;
    let (defs, matched_name) = collect_active_shapes_and_name(schema_val, eval);
    let schema_name = matched_name
        .as_deref()
        .and_then(sanitize_json_spec_name)
        .unwrap_or_else(|| "structured_output".to_owned());
    let json_schema = shape.to_json_schema(&defs);
    Ok(alloc_plain_json(
        &json!({
            "name": schema_name,
            "schema": json_schema,
        }),
        eval.heap(),
    ))
}

fn sanitize_json_spec_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn collect_active_shapes_and_name<'v>(
    target_ctor: Value<'v>,
    eval: &Evaluator<'v, '_, '_>,
) -> (HashMap<String, TypeShape>, Option<String>) {
    let mut defs: HashMap<String, TypeShape> = HashMap::new();
    let mut matched_name: Option<String> = None;

    ACTIVE_TYPE_REGISTRY.with(|slot| {
        if let Some(reg) = slot.borrow().as_ref() {
            defs.extend(reg.shapes.clone());
            for (name, ctor) in &reg.ctors {
                // SAFETY: `eval.frozen_heap()` retains a reference to `ctor.owner()` for `'v`.
                let candidate = unsafe { ctor.owned_frozen_value(eval.frozen_heap()) }.to_value();
                if target_ctor.ptr_eq(candidate) {
                    matched_name = Some(name.clone());
                }
            }
        }
    });

    for sym in eval.module().names() {
        let name = sym.as_str();
        if let Some(candidate) = eval.module().get(name) {
            if target_ctor.ptr_eq(candidate) {
                matched_name = Some(name.to_owned());
            }
            let rendered = candidate.to_string();
            if (rendered.starts_with("record(") || rendered.starts_with("enum("))
                && let Ok(shape) = parse_type_expr(&rendered)
            {
                defs.insert(name.to_owned(), shape);
            }
        }
    }

    (defs, matched_name)
}

/// Instantiates a typed Starlark `Record` from a JSON object inside an active `Evaluator`,
/// coercing nested `record` and `enum` fields when an active [`TypeRegistry`] is available.
pub fn instantiate_record_in_eval<'v>(
    ctor_val: Value<'v>,
    obj_json: &JsonValue,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let shape = parse_record_schema_value(ctor_val, eval.heap())?;
    let obj = obj_json
        .as_object()
        .context("structured payload must be a JSON object")?;
    let TypeShape::Record(fields) = &shape else {
        bail!("expected `record(...)` schema");
    };

    let active_reg = ACTIVE_TYPE_REGISTRY.with(|slot| slot.borrow().clone());
    let empty_reg = TypeRegistry::default();
    let reg = active_reg.as_ref().unwrap_or(&empty_reg);

    let mut named_vals: Vec<(String, Value<'v>)> = Vec::with_capacity(obj.len() + fields.len());
    let mut seen = std::collections::HashSet::new();

    for (k, v) in obj {
        seen.insert(k.as_str());
        let coerced = if let Some(field_def) = fields.iter().find(|f| &f.name == k) {
            coerce_json_in_eval(eval, &field_def.shape, reg, v)
                .with_context(|| format!("invalid field `{k}`"))?
        } else {
            alloc_plain_json(v, eval.heap())
        };
        named_vals.push((k.clone(), coerced));
    }

    for field_def in fields {
        if !seen.contains(field_def.name.as_str())
            && !field_def.has_default
            && field_def.shape.allows_null(&reg.shapes)
        {
            named_vals.push((field_def.name.clone(), Value::new_none()));
        }
    }

    let named_refs: Vec<(&str, Value<'v>)> =
        named_vals.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    eval.eval_function(ctor_val, &[], &named_refs)
        .map_err(|e| anyhow::anyhow!("{e:#}"))
}

fn coerce_json_in_eval<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    shape: &TypeShape,
    registry: &TypeRegistry,
    val: &JsonValue,
) -> anyhow::Result<Value<'v>> {
    if val.is_null() {
        return Ok(Value::new_none());
    }
    match shape {
        TypeShape::Named(name) => {
            if let (Some(target_shape), Some(ctor)) =
                (registry.shapes.get(name), registry.ctors.get(name))
            {
                // SAFETY: `eval.frozen_heap()` retains a reference to `ctor.owner()` for `'v`.
                let ctor_val = unsafe { ctor.owned_frozen_value(eval.frozen_heap()) }.to_value();
                match target_shape {
                    TypeShape::Record(_) => {
                        return instantiate_record_in_eval(ctor_val, val, eval);
                    }
                    TypeShape::Enum(_) => {
                        let s = val
                            .as_str()
                            .with_context(|| format!("expected string for enum `{name}`"))?;
                        let arg = eval.heap().alloc_str(s).to_value();
                        return eval
                            .eval_function(ctor_val, &[arg], &[])
                            .map_err(|e| anyhow::anyhow!("{e:#}"));
                    }
                    other => return coerce_json_in_eval(eval, other, registry, val),
                }
            }
            Ok(alloc_plain_json(val, eval.heap()))
        }
        TypeShape::List(item_shape) => {
            let Some(arr) = val.as_array() else {
                return Ok(alloc_plain_json(val, eval.heap()));
            };
            let mut items = Vec::with_capacity(arr.len());
            for (idx, elem) in arr.iter().enumerate() {
                items.push(
                    coerce_json_in_eval(eval, item_shape, registry, elem)
                        .with_context(|| format!("at index [{idx}]"))?,
                );
            }
            Ok(eval.heap().alloc(AllocList(items)))
        }
        TypeShape::Dict(val_shape) => {
            let Some(map) = val.as_object() else {
                return Ok(alloc_plain_json(val, eval.heap()));
            };
            let mut entries = Vec::with_capacity(map.len());
            for (k, v) in map {
                let cv = coerce_json_in_eval(eval, val_shape, registry, v)
                    .with_context(|| format!("at key `{k}`"))?;
                entries.push((k.clone(), cv));
            }
            Ok(eval
                .heap()
                .alloc(AllocDict(entries.iter().map(|(k, v)| (k.as_str(), *v)))))
        }
        TypeShape::Union(variants) => {
            for variant in variants {
                if matches!(variant, TypeShape::Null) {
                    continue;
                }
                if let Ok(coerced) = coerce_json_in_eval(eval, variant, registry, val) {
                    return Ok(coerced);
                }
            }
            Ok(alloc_plain_json(val, eval.heap()))
        }
        _ => Ok(alloc_plain_json(val, eval.heap())),
    }
}

#[cfg(test)]
mod tests {
    use dupe::Dupe;
    use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    use super::*;

    #[test]
    fn derives_recursive_json_schema_and_instantiates_nested_records_and_enums() {
        let globals = GlobalsBuilder::extended_by(&[
            LibraryExtension::RecordType,
            LibraryExtension::EnumType,
            LibraryExtension::Typing,
        ])
        .build();

        let frozen = Module::with_temp_heap(|module| {
            let src = r#"
Severity = enum("LOW", "HIGH")
Step = record(title = str, severity = Severity)
Plan = record(
    summary = str,
    steps = list[Step],
    count = int,
    notes = field(str, "default-note"),
    reviewer = str | None,
)
"#;
            let ast = AstModule::parse("test.star", src.to_owned(), &Dialect::Standard).unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            module.freeze()
        })
        .unwrap();

        let registry = TypeRegistry::from_frozen_modules(std::slice::from_ref(&frozen));
        let plan_ctor = frozen.get("Plan").unwrap();
        let plan_shape = registry.shapes.get("Plan").unwrap();
        let schema = plan_shape.to_json_schema(&registry.shapes);

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["summary"]["type"], "string");
        assert_eq!(schema["properties"]["steps"]["type"], "array");
        assert_eq!(schema["properties"]["steps"]["items"]["type"], "object");
        assert_eq!(
            schema["properties"]["steps"]["items"]["properties"]["severity"]["enum"],
            json!(["LOW", "HIGH"])
        );
        assert_eq!(schema["required"], json!(["summary", "steps", "count"]));

        // Valid instantiation (omitting `notes` default and `reviewer` nullable)
        let valid_args = json!({
            "summary": "Ship MCP artifact tools",
            "steps": [
                {"title": "Add schema derivation", "severity": "HIGH"}
            ],
            "count": 1
        });
        let roundtrip_json = Module::with_temp_heap(|temp_mod| {
            let rec = instantiate_record_with_registry(
                &temp_mod,
                &plan_ctor,
                plan_shape,
                &registry,
                &valid_args,
            )
            .unwrap();
            Ok::<_, starlark::Error>(rec.to_json_value().unwrap())
        })
        .unwrap();

        assert_eq!(roundtrip_json["summary"], "Ship MCP artifact tools");
        assert_eq!(roundtrip_json["notes"], "default-note");
        assert_eq!(roundtrip_json["reviewer"], JsonValue::Null);
        assert_eq!(roundtrip_json["steps"][0]["severity"], "HIGH");

        // Invalid enum variant must fail cleanly
        let bad_enum = json!({
            "summary": "Bad",
            "steps": [{"title": "x", "severity": "CRITICAL"}],
            "count": 1
        });
        let err = Module::with_temp_heap(|temp_mod| {
            let res = instantiate_record_with_registry(
                &temp_mod,
                &plan_ctor,
                plan_shape,
                &registry,
                &bad_enum,
            );
            assert!(res.is_err(), "expected invalid enum variant to fail");
            Ok::<_, starlark::Error>(format!("{:#}", res.unwrap_err()))
        })
        .unwrap();
        let _ = frozen.frozen_heap().dupe();
        assert!(err.contains("CRITICAL"), "unexpected error: {err}");
    }
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use starchart_ir::{AssignRef, DelayRef, GuardRef, MachineDef};
use dupe::{Dupe, OptionDupedExt};
use starlark::environment::{FrozenModule, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::AstModule;
use starlark::typing::{AstModuleTypecheck, Interface, Ty};
use starlark::values::typing::TypeCompiled;
use starlark::values::{FrozenValue, OwnedFrozenValue, ValueLike};
use thiserror::Error;

use crate::dsl::{chart_dialect, chart_globals};
use crate::schema::{TypeRegistry, TypeShape};
use crate::stdlib::resolve_builtin_module;
use crate::types::{
    AgentToolDataGen, ArtifactMode, FrozenStarlarkMachine, starlark_input_slot_name,
    starlark_tool_slot_name,
};

/// Frozen, pre-validated MCP tool definition extracted from a `.star` statechart.
#[derive(Debug, Clone)]
pub enum CompiledAgentTool {
    Artifact {
        name: String,
        description: String,
        input_schema: serde_json::Value,
        shape: TypeShape,
        schema_ctor: OwnedFrozenValue,
        mode: ArtifactMode,
        required: bool,
        validate: Option<OwnedFrozenValue>,
    },
    Read {
        name: String,
        description: String,
        input_schema: serde_json::Value,
        shape: Option<TypeShape>,
        schema_ctor: Option<OwnedFrozenValue>,
        handler: OwnedFrozenValue,
    },
}

impl CompiledAgentTool {
    pub fn name(&self) -> &str {
        match self {
            Self::Artifact { name, .. } | Self::Read { name, .. } => name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Artifact { description, .. } | Self::Read { description, .. } => description,
        }
    }

    pub fn input_schema(&self) -> &serde_json::Value {
        match self {
            Self::Artifact { input_schema, .. } | Self::Read { input_schema, .. } => input_schema,
        }
    }
}

/// Compiled, validated statechart paired with thread-safe frozen Starlark callables.
#[derive(Debug, Clone)]
pub struct CompiledChart {
    pub ir: MachineDef,
    pub guards: HashMap<String, OwnedFrozenValue>,
    pub reducers: HashMap<String, OwnedFrozenValue>,
    pub delays: HashMap<String, OwnedFrozenValue>,
    pub inputs: HashMap<String, OwnedFrozenValue>,
    pub tools: HashMap<String, CompiledAgentTool>,
    pub type_registry: TypeRegistry,
    /// Optional frozen `RecordType` constructor if `machine(context = ...)` used a Starlark `record(...)`.
    pub context_record_type: Option<OwnedFrozenValue>,
}

/// Errors produced when parsing, typechecking, or evaluating a `.star` statechart.
#[derive(Debug, Error)]
pub enum ChartCompileError {
    #[error("failed to read `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cyclic `load()` dependency detected involving `{0}`")]
    LoadCycle(String),

    #[error("syntax error:\n{0}")]
    Parse(String),

    #[error("static typecheck failed:\n{0}")]
    Typecheck(String),

    #[error("evaluation error:\n{0}")]
    Eval(String),

    #[error("freeze error:\n{0}")]
    Freeze(String),

    #[error("module `{module}` does not export `{symbol}`")]
    MissingExport { module: String, symbol: String },

    #[error("export `{symbol}` in `{module}` has type `{actual}`, expected `StateMachine`")]
    InvalidExportType {
        module: String,
        symbol: String,
        actual: String,
    },
}

/// Resolves Starlark module paths for `load(...)` statements.
pub trait SourceResolver: Send + Sync {
    /// Resolves `load_path` requested from `from_module` into `(canonical_id, source_code)`.
    fn resolve(
        &self,
        from_module: &str,
        load_path: &str,
    ) -> Result<(String, String), ChartCompileError>;
}

/// Filesystem-backed resolver supporting relative `load("events.star", ...)` imports.
#[derive(Debug, Default, Clone)]
pub struct FsResolver;

impl SourceResolver for FsResolver {
    fn resolve(
        &self,
        from_module: &str,
        load_path: &str,
    ) -> Result<(String, String), ChartCompileError> {
        let base_dir = Path::new(from_module)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let joined = base_dir.join(load_path);
        let canonical = std::fs::canonicalize(&joined).unwrap_or(joined);
        let path_str = canonical.to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&canonical).map_err(|source| ChartCompileError::Io {
            path: path_str.clone(),
            source,
        })?;
        Ok((path_str, source))
    }
}

/// Thread-safe in-memory resolver for testing multi-file `load()` without touching disk.
#[derive(Debug, Default, Clone)]
pub struct InMemoryResolver {
    files: Arc<HashMap<String, String>>,
}

impl InMemoryResolver {
    pub fn new(files: HashMap<String, String>) -> Self {
        Self {
            files: Arc::new(files),
        }
    }
}

impl SourceResolver for InMemoryResolver {
    fn resolve(
        &self,
        from_module: &str,
        load_path: &str,
    ) -> Result<(String, String), ChartCompileError> {
        if let Some(src) = self.files.get(load_path) {
            return Ok((load_path.to_owned(), src.clone()));
        }
        let relative = Path::new(from_module)
            .parent()
            .map(|p| p.join(load_path).to_string_lossy().into_owned())
            .unwrap_or_else(|| load_path.to_owned());
        if let Some(src) = self.files.get(&relative) {
            return Ok((relative, src.clone()));
        }
        Err(ChartCompileError::Io {
            path: load_path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("module `{load_path}` not found in memory resolver"),
            ),
        })
    }
}

#[derive(Clone)]
struct MapFileLoader {
    modules: Arc<HashMap<String, FrozenModule>>,
}

impl FileLoader for MapFileLoader {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.modules
            .get(path)
            .duped()
            .ok_or_else(|| starlark::Error::new_other(anyhow::anyhow!("unknown load `{path}`")))
    }
}

#[derive(Clone)]
struct CachedModule {
    interface: Interface,
    frozen: FrozenModule,
}

/// Compiles a `.star` file from disk, resolving any `load(...)` statements relative to the file.
pub fn compile_starlark_file(
    path: impl AsRef<Path>,
    export_symbol: Option<&str>,
) -> Result<CompiledChart, ChartCompileError> {
    let raw_path = path.as_ref();
    let canonical: PathBuf =
        std::fs::canonicalize(raw_path).unwrap_or_else(|_| raw_path.to_path_buf());
    let module_id = canonical.to_string_lossy().into_owned();
    let source = std::fs::read_to_string(&canonical).map_err(|source| ChartCompileError::Io {
        path: module_id.clone(),
        source,
    })?;
    compile_with_resolver(&module_id, &source, export_symbol.unwrap_or("chart"), &FsResolver)
}

/// Compiles an inline Starlark string with no external `load(...)` dependencies.
pub fn compile_starlark_str(
    filename: &str,
    source: &str,
    export_symbol: Option<&str>,
) -> Result<CompiledChart, ChartCompileError> {
    compile_with_resolver(
        filename,
        source,
        export_symbol.unwrap_or("chart"),
        &InMemoryResolver::default(),
    )
}

/// Compiles a Starlark module with a custom [`SourceResolver`] for multi-file `load(...)` support.
pub fn compile_with_resolver(
    module_id: &str,
    source: &str,
    export_symbol: &str,
    resolver: &dyn SourceResolver,
) -> Result<CompiledChart, ChartCompileError> {
    let cache: Mutex<HashMap<String, CachedModule>> = Mutex::new(HashMap::new());
    let mut visiting: HashSet<String> = HashSet::new();
    let all_loaded_modules: Mutex<Vec<FrozenModule>> = Mutex::new(Vec::new());

    let root = compile_module_recursive(
        module_id,
        source,
        resolver,
        &cache,
        &mut visiting,
        &all_loaded_modules,
    )?;

    extract_compiled_chart(
        module_id,
        export_symbol,
        &root.frozen,
        &all_loaded_modules.into_inner().unwrap(),
    )
}

fn compile_module_recursive(
    module_id: &str,
    source: &str,
    resolver: &dyn SourceResolver,
    cache: &Mutex<HashMap<String, CachedModule>>,
    visiting: &mut HashSet<String>,
    all_loaded_modules: &Mutex<Vec<FrozenModule>>,
) -> Result<CachedModule, ChartCompileError> {
    if let Some(existing) = cache.lock().unwrap().get(module_id).cloned() {
        return Ok(existing);
    }
    if !visiting.insert(module_id.to_owned()) {
        return Err(ChartCompileError::LoadCycle(module_id.to_owned()));
    }

    let globals = chart_globals();
    let dialect = chart_dialect();

    let ast = AstModule::parse(module_id, source.to_owned(), &dialect)
        .map_err(|e| ChartCompileError::Parse(format!("{e:#}")))?;

    let mut load_interfaces: HashMap<String, Interface> = HashMap::new();
    let mut load_modules: HashMap<String, FrozenModule> = HashMap::new();

    for load in ast.loads() {
        let load_str = load.module_id;
        let (dep_id, dep_source) = match resolve_builtin_module(load_str) {
            Some((builtin_id, builtin_src)) => (builtin_id.to_owned(), builtin_src.to_owned()),
            None => resolver.resolve(module_id, load_str)?,
        };
        let dep_compiled = compile_module_recursive(
            &dep_id,
            &dep_source,
            resolver,
            cache,
            visiting,
            all_loaded_modules,
        )?;
        load_interfaces.insert(load_str.to_owned(), dep_compiled.interface);
        load_modules.insert(load_str.to_owned(), dep_compiled.frozen);
    }

    // Pass 1a: Ahead-of-time static typechecking of module scopes and `def` bodies.
    let (mut type_errors, _typemap, static_interface, _approx) =
        ast.clone().typecheck(&globals, &load_interfaces);

    // Pass 1b: Starlark's `AstModule::typecheck` only solves constraints inside `def` blocks.
    // Synthesize a top-level checker function over top-level statements so calls like
    // `transition(...)` and `machine(...)` at module scope are statically typechecked too
    // before any evaluation occurs.
    if type_errors.is_empty()
        && let Some(top_check_src) = build_top_level_typecheck_source(source)
        && let Ok(top_ast) = AstModule::parse(module_id, top_check_src, &dialect)
    {
        let (top_errors, _, _, _) = top_ast.typecheck(&globals, &load_interfaces);
        type_errors.extend(top_errors);
    }

    if !type_errors.is_empty() {
        let rendered = type_errors
            .into_iter()
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        visiting.remove(module_id);
        return Err(ChartCompileError::Typecheck(rendered));
    }

    // Pass 2: Evaluate in a temporary heap and freeze into an immutable FrozenModule.
    let file_loader = MapFileLoader {
        modules: Arc::new(load_modules),
    };

    let frozen = Module::with_temp_heap(|module| {
        {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&file_loader);
            eval.eval_module(ast, &globals)
                .map_err(ChartCompileError::from_starlark_eval)?;
        }
        module
            .freeze()
            .map_err(|e| ChartCompileError::Freeze(format!("{e:?}")))
    })?;

    let mut precise_bindings: HashMap<String, Ty> = HashMap::new();
    for name in frozen.names() {
        let name_str = name.as_str();
        if let Ok(Some(owned)) = frozen.get_option(name_str) {
            let precise_ty = Ty::of_value(owned.value());
            let final_ty = match static_interface.get(name_str) {
                Some(st) if precise_ty == Ty::any() => st.dupe(),
                _ => precise_ty,
            };
            precise_bindings.insert(name_str.to_owned(), final_ty);
        }
    }
    let interface = Interface::new(precise_bindings);

    visiting.remove(module_id);

    let compiled = CachedModule {
        interface,
        frozen: frozen.dupe(),
    };
    all_loaded_modules.lock().unwrap().push(frozen);
    cache
        .lock()
        .unwrap()
        .insert(module_id.to_owned(), compiled.clone());
    Ok(compiled)
}

impl ChartCompileError {
    fn from_starlark_eval(err: starlark::Error) -> Self {
        Self::Eval(format!("{err:#}"))
    }
}

fn extract_compiled_chart(
    module_id: &str,
    export_symbol: &str,
    frozen_module: &FrozenModule,
    loaded_modules: &[FrozenModule],
) -> Result<CompiledChart, ChartCompileError> {
    let exported: OwnedFrozenValue = frozen_module
        .get_option(export_symbol)
        .map_err(|e| ChartCompileError::Eval(format!("{e:#}")))?
        .ok_or_else(|| ChartCompileError::MissingExport {
            module: module_id.to_owned(),
            symbol: export_symbol.to_owned(),
        })?;

    let Some(machine) = exported.value().downcast_ref::<FrozenStarlarkMachine>() else {
        return Err(ChartCompileError::InvalidExportType {
            module: module_id.to_owned(),
            symbol: export_symbol.to_owned(),
            actual: exported.value().get_type().to_owned(),
        });
    };

    let heap = frozen_module.frozen_heap();

    let mut guards = HashMap::with_capacity(machine.guard_slots.len());
    for (slot, frozen_fn) in machine.guard_slots.iter().copied().enumerate() {
        let name = GuardRef::starlark_slot_name(slot);
        // SAFETY: `frozen_fn` was frozen into `frozen_module`'s heap during `module.freeze()`.
        let owned = unsafe { OwnedFrozenValue::new(heap.dupe(), frozen_fn) };
        guards.insert(name, owned);
    }

    let mut reducers = HashMap::with_capacity(machine.reducer_slots.len());
    for (slot, frozen_fn) in machine.reducer_slots.iter().copied().enumerate() {
        let name = AssignRef::starlark_slot_name(slot);
        // SAFETY: `frozen_fn` was frozen into `frozen_module`'s heap during `module.freeze()`.
        let owned = unsafe { OwnedFrozenValue::new(heap.dupe(), frozen_fn) };
        reducers.insert(name, owned);
    }

    let mut delays = HashMap::with_capacity(machine.delay_slots.len());
    for (slot, frozen_fn) in machine.delay_slots.iter().copied().enumerate() {
        let name = DelayRef::starlark_slot_name(slot);
        // SAFETY: `frozen_fn` was frozen into `frozen_module`'s heap during `module.freeze()`.
        let owned = unsafe { OwnedFrozenValue::new(heap.dupe(), frozen_fn) };
        delays.insert(name, owned);
    }

    let mut inputs = HashMap::with_capacity(machine.input_slots.len());
    for (slot, frozen_fn) in machine.input_slots.iter().copied().enumerate() {
        let name = starlark_input_slot_name(slot);
        // SAFETY: `frozen_fn` was frozen into `frozen_module`'s heap during `module.freeze()`.
        let owned = unsafe { OwnedFrozenValue::new(heap.dupe(), frozen_fn) };
        inputs.insert(name, owned);
    }

    let type_registry = TypeRegistry::from_frozen_modules(loaded_modules);

    let mut tools = HashMap::with_capacity(machine.tool_slots.len());
    for (slot, frozen_tool) in machine.tool_slots.iter().enumerate() {
        let slot_key = starlark_tool_slot_name(slot);
        let compiled_tool = match frozen_tool {
            AgentToolDataGen::Artifact {
                name,
                description,
                shape,
                schema_ctor,
                mode,
                required,
                validate,
            } => {
                let input_schema = shape.0.to_json_schema(&type_registry.shapes);
                // SAFETY: `schema_ctor` and `validate` were frozen into `frozen_module`'s heap.
                let ctor_owned = unsafe { OwnedFrozenValue::new(heap.dupe(), *schema_ctor) };
                let validate_owned = validate
                    .map(|v| unsafe { OwnedFrozenValue::new(heap.dupe(), v) });
                CompiledAgentTool::Artifact {
                    name: name.clone(),
                    description: description.clone(),
                    input_schema,
                    shape: shape.0.clone(),
                    schema_ctor: ctor_owned,
                    mode: *mode,
                    required: *required,
                    validate: validate_owned,
                }
            }
            AgentToolDataGen::Read {
                name,
                description,
                shape,
                schema_ctor,
                handler,
            } => {
                let input_schema = match shape {
                    Some(s) => s.0.to_json_schema(&type_registry.shapes),
                    None => serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false,
                    }),
                };
                // SAFETY: `schema_ctor` and `handler` were frozen into `frozen_module`'s heap.
                let ctor_owned = schema_ctor
                    .map(|v| unsafe { OwnedFrozenValue::new(heap.dupe(), v) });
                let handler_owned = unsafe { OwnedFrozenValue::new(heap.dupe(), *handler) };
                CompiledAgentTool::Read {
                    name: name.clone(),
                    description: description.clone(),
                    input_schema,
                    shape: shape.as_ref().map(|s| s.0.clone()),
                    schema_ctor: ctor_owned,
                    handler: handler_owned,
                }
            }
        };
        tools.insert(slot_key, compiled_tool);
    }

    let context_record_type = machine
        .initial_record_context
        .and_then(|initial_rec| find_matching_record_type(initial_rec, loaded_modules));

    Ok(CompiledChart {
        ir: machine.ir.0.clone(),
        guards,
        reducers,
        delays,
        inputs,
        tools,
        type_registry,
        context_record_type,
    })
}

fn find_matching_record_type(
    record_instance: FrozenValue,
    modules: &[FrozenModule],
) -> Option<OwnedFrozenValue> {
    let mut candidates: Vec<OwnedFrozenValue> = Vec::new();
    for m in modules {
        for name in m.names() {
            if let Ok(Some(candidate)) = m.get_option(name.as_str())
                && candidate.value().get_type() == "function"
            {
                candidates.push(candidate);
            }
        }
    }

    Module::with_temp_heap(|temp_module| {
        let rec_val = record_instance.to_value();
        for candidate in candidates {
            // SAFETY: `temp_module.frozen_heap()` holds a reference to `candidate`'s owner heap for `'v`.
            let fn_val =
                unsafe { candidate.owned_frozen_value(temp_module.frozen_heap()) }.to_value();
            if let Ok(compiled_ty) = TypeCompiled::new(fn_val, temp_module.heap())
                && compiled_ty.matches(rec_val)
            {
                return Ok::<Option<OwnedFrozenValue>, starlark::Error>(Some(candidate));
            }
        }
        Ok(None)
    })
    .ok()
    .flatten()
}

/// Builds a synthetic Starlark module that keeps `load(...)` and top-level bindings intact
/// while also placing top-level non-`load`, non-`def` statements inside a synthetic `def`
/// so `AstModule::typecheck` statically validates top-level DSL calls before evaluation.
fn build_top_level_typecheck_source(source: &str) -> Option<String> {
    let mut body_lines: Vec<String> = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        let is_top_level = line.len() == trimmed.len();

        // Skip top-level `load(...)` (including multi-line loads) and top-level `def ...:` blocks.
        if is_top_level && trimmed.starts_with("load(") {
            let mut paren_depth: i32 = 0;
            while i < lines.len() {
                for ch in lines[i].chars() {
                    match ch {
                        '(' => paren_depth += 1,
                        ')' => paren_depth -= 1,
                        _ => {}
                    }
                }
                i += 1;
                if paren_depth <= 0 {
                    break;
                }
            }
            continue;
        }

        if is_top_level && trimmed.starts_with("def ") {
            let mut paren_depth: i32 = 0;
            let mut header_done = false;
            while i < lines.len() {
                let l = lines[i];
                if !header_done {
                    for ch in l.chars() {
                        match ch {
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            _ => {}
                        }
                    }
                    if paren_depth <= 0 && l.trim_end().ends_with(':') {
                        header_done = true;
                    }
                    i += 1;
                } else {
                    let lt = l.trim_start();
                    if lt.is_empty() || lt.starts_with('#') || l.len() > lt.len() {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }

        body_lines.push(format!("    {line}"));
        i += 1;
    }

    if body_lines.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(source.len() + body_lines.len() * 8 + 64);
    out.push_str(source);
    out.push_str("\n\ndef __starchart_top_level_typecheck__():\n");
    for bl in body_lines {
        out.push_str(&bl);
        out.push('\n');
    }
    Some(out)
}

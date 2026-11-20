use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use indexmap::IndexMap;
use serde_json::Value;
use starchart_ir::{ActionDef, StateId, StateKind, StateNodeDef};
use starchart_runtime::builtins::acp::serve_chart_as_acp_agent;
use starchart_runtime::{HostCallbacks, StarchartActor, StarchartSyncRunner, render_mermaid};
use starchart_starlark::{
    CompiledChart, chart_dialect, chart_globals, compile_starlark_file, is_builtin_service,
    resolve_builtin_module,
};
use starlark::analysis::EvalMessage;
use starlark::docs::DocModule;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::syntax::AstModule;
use starlark::typing::{AstModuleTypecheck, Interface, Ty};
use starlark_lsp::error::eval_message_to_lsp_diagnostic;
use starlark_lsp::server::{
    LspContext, LspEvalResult, LspUri, StringLiteralResult, stdio_server,
};

#[derive(Debug, Parser)]
#[command(
    name = "starchart",
    version,
    about = "Statically typechecked Starlark Harel statechart engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Parse, statically typecheck, evaluate, and validate `.star` statechart files.
    Check {
        /// One or more `.star` statechart files to validate.
        #[arg(required = true)]
        files: Vec<PathBuf>,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,
    },

    /// Render a Mermaid `stateDiagram-v2` graph from a `.star` statechart file.
    Diagram {
        /// Path to the `.star` statechart file.
        file: PathBuf,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,
    },

    /// Execute a `.star` statechart with built-in HTTP, command, and ACP drivers.
    Run {
        /// Path to the `.star` statechart file.
        file: PathBuf,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,

        /// Optional JSON object merged into the machine's initial `context`.
        #[arg(long)]
        context: Option<String>,

        /// Force asynchronous Tokio execution via `StarchartActor`.
        #[arg(long = "async")]
        force_async: bool,

        /// Events to dispatch in order (`EVENT_NAME`, `after:<ms>`, or JSON `'{"type":"APPROVED","amount":250}'`).
        #[arg(short, long = "event")]
        events: Vec<String>,
    },

    /// Expose a `.star` statechart workflow over `stdio` as an Agent Client Protocol (ACP v1) agent.
    Acp {
        /// Path to the `.star` statechart file.
        file: PathBuf,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,
    },

    /// Start the Starlark Language Server Protocol (LSP) server over stdio.
    Lsp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Check { files, export } => run_check(&files, &export),
        Commands::Diagram { file, export } => run_diagram(&file, &export),
        Commands::Run {
            file,
            export,
            context,
            force_async,
            events,
        } => run_execute(&file, &export, context.as_deref(), force_async, &events),
        Commands::Acp { file, export } => run_acp_server(&file, &export),
        Commands::Lsp => run_lsp(),
    }
}

fn run_check(files: &[PathBuf], export: &str) -> ExitCode {
    let mut had_error = false;
    for file in files {
        match compile_starlark_file(file, Some(export)) {
            Ok(compiled) => {
                println!(
                    "OK {} (machine `{}`, initial `{}`)",
                    file.display(),
                    compiled.ir.id,
                    compiled.ir.initial
                );
            }
            Err(err) => {
                had_error = true;
                eprintln!("ERROR {}:\n{err}", file.display());
            }
        }
    }
    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn run_diagram(file: &Path, export: &str) -> ExitCode {
    let compiled = match compile_starlark_file(file, Some(export)) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ERROR {}:\n{err}", file.display());
            return ExitCode::from(1);
        }
    };

    match render_mermaid(&compiled) {
        Ok(mermaid) => {
            println!("{mermaid}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("ERROR rendering diagram for {}:\n{err}", file.display());
            ExitCode::from(1)
        }
    }
}

fn run_execute(
    file: &Path,
    export: &str,
    context_override: Option<&str>,
    force_async: bool,
    raw_events: &[String],
) -> ExitCode {
    let mut compiled = match compile_starlark_file(file, Some(export)) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ERROR {}:\n{err}", file.display());
            return ExitCode::from(1);
        }
    };

    if let Some(patch_str) = context_override {
        match serde_json::from_str::<Value>(patch_str) {
            Ok(Value::Object(patch_map)) => {
                if let Value::Object(ctx_map) = &mut compiled.ir.context {
                    for (k, v) in patch_map {
                        ctx_map.insert(k, v);
                    }
                }
            }
            Ok(_) => {
                eprintln!("ERROR `--context` must be a JSON object");
                return ExitCode::from(1);
            }
            Err(err) => {
                eprintln!("ERROR invalid `--context` JSON: {err}");
                return ExitCode::from(1);
            }
        }
    }

    let mut action_names = HashSet::new();
    let mut service_names = HashSet::new();
    collect_actions_and_services(&compiled.ir.states, &mut action_names, &mut service_names);

    let uses_builtin_async = service_names.iter().any(|s| is_builtin_service(s));
    let uses_simulated_clock = raw_events
        .iter()
        .any(|e| e.trim().starts_with("after:"));

    if (force_async || uses_builtin_async) && !uses_simulated_clock {
        return run_execute_async(compiled, action_names, service_names, raw_events);
    }

    run_execute_sync(compiled, action_names, service_names, raw_events)
}

fn build_cli_callbacks(
    action_names: HashSet<String>,
    service_names: HashSet<String>,
    include_builtins: bool,
) -> HostCallbacks {
    let mut callbacks = if include_builtins {
        HostCallbacks::with_builtins()
    } else {
        HostCallbacks::new()
    };

    let mut sorted_actions: Vec<String> = action_names.into_iter().collect();
    sorted_actions.sort();
    for action_name in sorted_actions {
        let label = action_name.clone();
        callbacks = callbacks.on_action(action_name, move |_ctx, ev| {
            println!("  -> action `{label}` invoked (on `{}`)", ev.event_type);
            Ok(())
        });
    }

    let mut sorted_services: Vec<String> = service_names.into_iter().collect();
    sorted_services.sort();
    for service_name in sorted_services {
        if include_builtins && is_builtin_service(&service_name) {
            continue;
        }
        let label = service_name.clone();
        callbacks = callbacks.on_sync_service(service_name, move |_ctx, input| {
            println!("  -> service `{label}` invoked (input={input})");
            Ok(serde_json::json!({}))
        });
    }

    callbacks
}

fn run_execute_async(
    compiled: CompiledChart,
    action_names: HashSet<String>,
    service_names: HashSet<String>,
    raw_events: &[String],
) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            eprintln!("ERROR building Tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    rt.block_on(async move {
        let callbacks = build_cli_callbacks(action_names, service_names, true);
        let actor = match StarchartActor::spawn(&compiled, callbacks) {
            Ok(a) => a,
            Err(err) => {
                eprintln!("ERROR starting `{}`: {err}", compiled.ir.id);
                return ExitCode::from(1);
            }
        };

        let init_ctx = actor.context().await.unwrap_or(Value::Null);
        println!(
            "[init] machine=`{}` active={:?} context={}",
            compiled.ir.id,
            actor.active_states(),
            init_ctx
        );

        for (idx, raw) in raw_events.iter().enumerate() {
            let (event_type, payload) = match parse_cli_event(raw) {
                Ok(pair) => pair,
                Err(err) => {
                    eprintln!("ERROR parsing event #{}: {err}", idx + 1);
                    return ExitCode::from(1);
                }
            };

            let res = if payload.is_null() {
                actor.send(&event_type).await
            } else {
                actor.send_with_payload(&event_type, payload).await
            };
            if let Err(err) = res {
                eprintln!("ERROR dispatching `{event_type}`: {err}");
                return ExitCode::from(1);
            }

            let ctx = actor.context().await.unwrap_or(Value::Null);
            println!(
                "[step {}] event=`{}` active={:?} done={} output={} context={}",
                idx + 1,
                event_type,
                actor.active_states(),
                actor.is_done(),
                actor.output(),
                ctx
            );
        }

        // Stream live state transitions while autonomous async `invoke` pipelines run.
        if !actor.is_done() {
            let mut rx = actor.subscribe();
            let mut prev_states = actor.active_states();
            loop {
                let view = rx.borrow_and_update().clone();
                let current_states = actor.active_states();
                if current_states != prev_states {
                    println!(
                        "[transition] machine=`{}` active={:?}",
                        compiled.ir.id, current_states
                    );
                    prev_states = current_states;
                }
                if let Some(err) = view.error {
                    eprintln!("ERROR executing `{}`: {err}", compiled.ir.id);
                    return ExitCode::from(1);
                }
                if view.status != xstate_statemachine::core::Status::Running {
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }

        let final_ctx = actor.context().await.unwrap_or(Value::Null);
        println!(
            "[done] machine=`{}` active={:?} done={} output={} context={}",
            compiled.ir.id,
            actor.active_states(),
            actor.is_done(),
            actor.output(),
            final_ctx
        );
        actor.stop().await;
        ExitCode::SUCCESS
    })
}

fn run_execute_sync(
    compiled: CompiledChart,
    action_names: HashSet<String>,
    service_names: HashSet<String>,
    raw_events: &[String],
) -> ExitCode {
    let callbacks = build_cli_callbacks(action_names, service_names, false);

    let mut runner = match StarchartSyncRunner::start(&compiled, callbacks) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("ERROR starting `{}`: {err}", compiled.ir.id);
            return ExitCode::from(1);
        }
    };

    println!(
        "[init] machine=`{}` active={:?} context={}",
        compiled.ir.id,
        runner.active_states(),
        runner.context()
    );

    for (idx, raw) in raw_events.iter().enumerate() {
        if let Some(ms_str) = raw.trim().strip_prefix("after:") {
            let ms: f64 = match ms_str.trim().parse() {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("ERROR parsing timer advance `after:{ms_str}`: {err}");
                    return ExitCode::from(1);
                }
            };
            if let Err(err) = runner.advance_ms(ms) {
                eprintln!("ERROR advancing clock by {ms}ms: {err}");
                return ExitCode::from(1);
            }
            println!(
                "[step {}] clock=+{ms}ms active={:?} done={} context={}",
                idx + 1,
                runner.active_states(),
                runner.is_done(),
                runner.context()
            );
            continue;
        }

        let (event_type, payload) = match parse_cli_event(raw) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("ERROR parsing event #{}: {err}", idx + 1);
                return ExitCode::from(1);
            }
        };

        let send_res = if payload.is_null() {
            runner.send(&event_type)
        } else {
            runner.send_with_payload(&event_type, payload.clone())
        };

        if let Err(err) = send_res {
            eprintln!("ERROR dispatching `{event_type}`: {err}");
            return ExitCode::from(1);
        }

        println!(
            "[step {}] event=`{}` active={:?} tags={:?} done={} output={} context={}",
            idx + 1,
            event_type,
            runner.active_states(),
            runner.active_tags(),
            runner.is_done(),
            runner.output(),
            runner.context()
        );
        for emitted in runner.take_emitted() {
            println!("  -> emitted {emitted}");
        }
    }

    ExitCode::SUCCESS
}

fn run_acp_server(file: &Path, export: &str) -> ExitCode {
    let compiled = match compile_starlark_file(file, Some(export)) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ERROR {}:\n{err}", file.display());
            return ExitCode::from(1);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            eprintln!("ERROR building Tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(serve_chart_as_acp_agent(
        compiled,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ACP server error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn push_named(actions: &[ActionDef], out: &mut HashSet<String>) {
    for act in actions {
        if let ActionDef::Named(name) = act {
            out.insert(name.clone());
        }
    }
}

fn collect_actions_and_services(
    states: &IndexMap<StateId, StateNodeDef>,
    actions: &mut HashSet<String>,
    services: &mut HashSet<String>,
) {
    for node in states.values() {
        push_named(&node.entry, actions);
        push_named(&node.exit, actions);
        for tr in &node.on {
            push_named(&tr.actions, actions);
        }
        for outcome in node.always.iter().chain(node.on_done.iter()) {
            push_named(&outcome.actions, actions);
        }
        for after_def in &node.after {
            for outcome in &after_def.outcomes {
                push_named(&outcome.actions, actions);
            }
        }
        for inv in &node.invoke {
            services.insert(inv.src.clone());
            for outcome in inv.on_done.iter().chain(inv.on_error.iter()) {
                push_named(&outcome.actions, actions);
            }
        }
        match &node.kind {
            StateKind::Compound {
                states: children, ..
            }
            | StateKind::Parallel { states: children } => {
                collect_actions_and_services(children, actions, services);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

fn parse_cli_event(raw: &str) -> anyhow::Result<(String, Value)> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        let mut val: Value = serde_json::from_str(trimmed)?;
        let obj = val
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("JSON event must be an object"))?;
        let ty = obj
            .remove("type")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or_else(|| anyhow::anyhow!("JSON event object must include string field `type`"))?;
        let payload = if obj.is_empty() {
            Value::Null
        } else {
            Value::Object(std::mem::take(obj))
        };
        Ok((ty, payload))
    } else {
        Ok((trimmed.to_owned(), Value::Null))
    }
}

struct StarchartLspContext {
    globals: starlark::environment::Globals,
    doc_module: DocModule,
    builtin_interfaces: HashMap<String, Interface>,
}

impl StarchartLspContext {
    fn new() -> Self {
        let globals = chart_globals();
        let doc_module = globals.documentation();
        let builtin_interfaces = build_builtin_lsp_interfaces(&globals);
        Self {
            globals,
            doc_module,
            builtin_interfaces,
        }
    }
}

fn build_builtin_lsp_interfaces(
    globals: &starlark::environment::Globals,
) -> HashMap<String, Interface> {
    let dialect = chart_dialect();
    let mut map = HashMap::new();
    for alias in [
        "@starchart/http",
        "@starchart/http.star",
        "@starchart/exec",
        "@starchart/exec.star",
        "@starchart/acp",
        "@starchart/acp.star",
        "@starchart/agents",
        "@starchart/agents.star",
    ] {
        if let Some((canon_id, src)) = resolve_builtin_module(alias)
            && let Ok(ast) = AstModule::parse(canon_id, src.to_owned(), &dialect)
            && let Ok(frozen) = Module::with_temp_heap(|m| {
                {
                    let mut eval = Evaluator::new(&m);
                    let _ = eval.eval_module(ast, globals);
                }
                m.freeze()
            })
        {
            let mut bindings = HashMap::new();
            for name in frozen.names() {
                if let Ok(Some(owned)) = frozen.get_option(name.as_str()) {
                    bindings.insert(name.as_str().to_owned(), Ty::of_value(owned.value()));
                }
            }
            map.insert(alias.to_owned(), Interface::new(bindings));
        }
    }
    map
}

impl LspContext for StarchartLspContext {
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult {
        let dialect = chart_dialect();
        let filename = uri.path().to_string_lossy().into_owned();

        match AstModule::parse(&filename, content, &dialect) {
            Ok(ast) => {
                let mut loads = HashMap::<String, Interface>::new();
                for load in ast.loads() {
                    if let Some(intf) = self.builtin_interfaces.get(load.module_id) {
                        loads.insert(load.module_id.to_owned(), intf.clone());
                    }
                }
                let (type_errors, _, _, _) = ast.clone().typecheck(&self.globals, &loads);
                let diagnostics = type_errors
                    .into_iter()
                    .map(|err| {
                        eval_message_to_lsp_diagnostic(EvalMessage::from_error(uri.path(), &err))
                    })
                    .collect();
                LspEvalResult {
                    diagnostics,
                    ast: Some(ast),
                }
            }
            Err(err) => LspEvalResult {
                diagnostics: vec![eval_message_to_lsp_diagnostic(EvalMessage::from_error(
                    uri.path(),
                    &err,
                ))],
                ast: None,
            },
        }
    }

    fn resolve_load(
        &self,
        path: &str,
        current_file: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Result<LspUri, String> {
        if let Some((canon_id, _)) = resolve_builtin_module(path) {
            return Ok(LspUri::Starlark(PathBuf::from(format!("/{canon_id}"))));
        }
        let base = current_file
            .path()
            .parent()
            .or(workspace_root)
            .unwrap_or_else(|| Path::new("."));
        let resolved = std::fs::canonicalize(base.join(path)).unwrap_or_else(|_| base.join(path));
        Ok(LspUri::File(resolved))
    }

    fn render_as_load(
        &self,
        target: &LspUri,
        current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<String, String> {
        if let LspUri::Starlark(p) = target {
            return Ok(p.to_string_lossy().trim_start_matches('/').to_owned());
        }
        if let Some(base) = current_file.path().parent()
            && let Ok(rel) = target.path().strip_prefix(base)
        {
            return Ok(rel.to_string_lossy().into_owned());
        }
        Ok(target.path().to_string_lossy().into_owned())
    }

    fn resolve_string_literal(
        &self,
        _literal: &str,
        _current_file: &LspUri,
        _workspace_root: Option<&Path>,
    ) -> Result<Option<StringLiteralResult>, String> {
        Ok(None)
    }

    fn get_load_contents(&self, uri: &LspUri) -> Result<Option<String>, String> {
        match uri {
            LspUri::Starlark(path) => {
                let key = path.to_string_lossy();
                Ok(resolve_builtin_module(key.as_ref()).map(|(_, src)| src.to_owned()))
            }
            LspUri::File(path) => match std::fs::read_to_string(path) {
                Ok(contents) => Ok(Some(contents)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            _ => Ok(None),
        }
    }

    fn get_environment(&self, _uri: &LspUri) -> DocModule {
        self.doc_module.clone()
    }

    fn get_uri_for_global_symbol(
        &self,
        _current_file: &LspUri,
        _symbol: &str,
    ) -> Result<Option<LspUri>, String> {
        Ok(None)
    }
}

fn run_lsp() -> ExitCode {
    match stdio_server(StarchartLspContext::new()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("LSP server error: {err:#}");
            ExitCode::from(1)
        }
    }
}

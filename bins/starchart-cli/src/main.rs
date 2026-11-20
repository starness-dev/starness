use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use starchart_ir::{StateId, StateKind, StateNodeDef};
use starchart_runtime::{ChartSyncRunner, HostCallbacks, render_mermaid};
use starchart_starlark::{chart_dialect, chart_globals, compile_starlark_file};
use clap::{Parser, Subcommand};
use indexmap::IndexMap;
use serde_json::Value;
use starlark::analysis::EvalMessage;
use starlark::docs::DocModule;
use starlark::syntax::AstModule;
use starlark::typing::{AstModuleTypecheck, Interface};
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

    /// Execute a `.star` statechart interactively against a sequence of events.
    Run {
        /// Path to the `.star` statechart file.
        file: PathBuf,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,

        /// Events to dispatch in order (either `EVENT_NAME` or JSON `'{"type":"APPROVED","amount":250}'`).
        #[arg(short, long = "event")]
        events: Vec<String>,
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
            events,
        } => run_execute(&file, &export, &events),
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

fn run_execute(file: &Path, export: &str, raw_events: &[String]) -> ExitCode {
    let compiled = match compile_starlark_file(file, Some(export)) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ERROR {}:\n{err}", file.display());
            return ExitCode::from(1);
        }
    };

    // Register trace logging on every declared action so CLI users see side-effect invocations.
    let mut action_names = HashSet::new();
    collect_actions(&compiled.ir.states, &mut action_names);
    let mut callbacks = HostCallbacks::new();
    let mut sorted_actions: Vec<String> = action_names.into_iter().collect();
    sorted_actions.sort();
    for action_name in sorted_actions {
        let label = action_name.clone();
        callbacks = callbacks.on_action(action_name, move |_ctx, ev| {
            println!("  -> action `{label}` invoked (on `{}`)", ev.event_type);
            Ok(())
        });
    }

    let mut runner = match ChartSyncRunner::start(&compiled, callbacks) {
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
            "[step {}] event=`{}` active={:?} done={} context={}",
            idx + 1,
            event_type,
            runner.active_states(),
            runner.is_done(),
            runner.context()
        );
    }

    ExitCode::SUCCESS
}

fn collect_actions(states: &IndexMap<StateId, StateNodeDef>, out: &mut HashSet<String>) {
    for node in states.values() {
        out.extend(node.entry.iter().cloned());
        out.extend(node.exit.iter().cloned());
        for tr in &node.on {
            out.extend(tr.actions.iter().cloned());
        }
        match &node.kind {
            StateKind::Compound { states: children, .. }
            | StateKind::Parallel { states: children } => collect_actions(children, out),
            StateKind::Atomic | StateKind::Final => {}
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
}

impl StarchartLspContext {
    fn new() -> Self {
        let globals = chart_globals();
        let doc_module = globals.documentation();
        Self {
            globals,
            doc_module,
        }
    }
}

impl LspContext for StarchartLspContext {
    fn parse_file_with_contents(&self, uri: &LspUri, content: String) -> LspEvalResult {
        let dialect = chart_dialect();
        let filename = uri.path().to_string_lossy().into_owned();

        match AstModule::parse(&filename, content, &dialect) {
            Ok(ast) => {
                let (type_errors, _, _, _) =
                    ast.clone().typecheck(&self.globals, &HashMap::<String, Interface>::new());
                let diagnostics = type_errors
                    .into_iter()
                    .map(|err| eval_message_to_lsp_diagnostic(EvalMessage::from_error(uri.path(), &err)))
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

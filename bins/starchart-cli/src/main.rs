use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, Subcommand};
use indexmap::IndexMap;
use serde_json::Value;
use starchart_ir::{ActionDef, MachineDef, StateId, StateKind, StateNodeDef};
use starchart_runtime::builtins::acp::serve_chart_as_acp_agent;
use starchart_runtime::{
    HostCallbacks, ServeOptions, StarchartActor, StarchartSyncRunner, TraceReceiver,
    bind_serve_listener, render_mermaid, serve_chart_http,
};
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

        /// Stream live newline-delimited events (`EVENT_NAME`, `sleep:<ms>`, or JSON `'{"type":"..."}'`)
        /// from `stdin` while the statechart executes.
        #[arg(long = "events-stdin", alias = "stdin")]
        events_stdin: bool,

        /// Write the full execution trace as JSON Lines to PATH (`-` for stdout; human-readable
        /// progress then moves to stderr). One record per line: `started`, `transition`,
        /// `service_started`, `service_progress`, `service_finished`, `error`, `stopped`, ...
        #[arg(long, value_name = "PATH")]
        trace: Option<String>,
    },

    /// Run a `.star` statechart as an HTTP + SSE daemon (`POST /events`, `GET /state`, `GET /diagram`, `GET /trace`).
    Serve {
        /// Path to the `.star` statechart file.
        file: PathBuf,

        /// Exported variable name holding the `StateMachine` (defaults to `chart`).
        #[arg(long, default_value = "chart")]
        export: String,

        /// TCP address to bind (e.g. `127.0.0.1:8080` or `127.0.0.1:0` for an ephemeral port).
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// Optional JSON object merged into the machine's initial `context` (when starting fresh).
        #[arg(long)]
        context: Option<String>,

        /// Optional JSON snapshot file to persist state across restarts and resume automatically.
        #[arg(long, value_name = "PATH")]
        state_file: Option<PathBuf>,

        /// Shut down the HTTP daemon automatically once the workflow reaches a `final_state`.
        #[arg(long)]
        exit_on_done: bool,
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

    /// Internal stdio <-> localhost TCP bridge for turn-scoped MCP tools.
    #[command(name = "mcp-bridge", hide = true)]
    McpBridge {
        #[arg(long)]
        addr: String,

        #[arg(long)]
        token: String,
    },
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
            events_stdin,
            trace,
        } => run_execute(
            &file,
            &export,
            context.as_deref(),
            force_async,
            &events,
            events_stdin,
            trace.as_deref(),
        ),
        Commands::Serve {
            file,
            export,
            bind,
            context,
            state_file,
            exit_on_done,
        } => run_serve(
            &file,
            &export,
            &bind,
            context.as_deref(),
            state_file,
            exit_on_done,
        ),
        Commands::Acp { file, export } => run_acp_server(&file, &export),
        Commands::Lsp => run_lsp(),
        Commands::McpBridge { addr, token } => run_mcp_bridge(&addr, &token),
    }
}

fn run_mcp_bridge(addr: &str, token: &str) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            eprintln!("error: failed to initialize Tokio runtime for mcp-bridge: {err}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(starchart_runtime::builtins::mcp::run_stdio_bridge(addr, token)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: mcp-bridge failed: {err:#}");
            ExitCode::FAILURE
        }
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

#[allow(clippy::too_many_arguments)]
fn run_execute(
    file: &Path,
    export: &str,
    context_override: Option<&str>,
    force_async: bool,
    raw_events: &[String],
    events_stdin: bool,
    trace_target: Option<&str>,
) -> ExitCode {
    let trace_out = match trace_target.map(open_trace_sink).transpose() {
        Ok(sink) => sink,
        Err(err) => {
            eprintln!("ERROR opening `--trace` output: {err}");
            return ExitCode::from(1);
        }
    };
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

    if (force_async || events_stdin || uses_builtin_async) && !uses_simulated_clock {
        return run_execute_async(
            compiled,
            action_names,
            service_names,
            raw_events,
            events_stdin,
            trace_out,
        );
    }

    run_execute_sync(compiled, action_names, service_names, raw_events, trace_out)
}

/// When set, human-readable `run` output goes to stderr (stdout carries the JSONL trace).
static HUMAN_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// Prints human-readable `run` output to stdout, or stderr while `--trace -` owns stdout.
macro_rules! say {
    ($($arg:tt)*) => {
        if HUMAN_TO_STDERR.load(Ordering::Relaxed) {
            eprintln!($($arg)*);
        } else {
            println!($($arg)*);
        }
    };
}
use say;

type TraceSink = Box<dyn Write + Send>;

fn open_trace_sink(target: &str) -> std::io::Result<TraceSink> {
    if target == "-" {
        HUMAN_TO_STDERR.store(true, Ordering::Relaxed);
        Ok(Box::new(std::io::stdout()))
    } else {
        Ok(Box::new(std::io::BufWriter::new(std::fs::File::create(target)?)))
    }
}

/// Drains `rx` on a dedicated thread, writing one JSON line per record (flushed per line so
/// `tail -f` / piping into another tool sees records live). Ends when the hub is closed.
fn spawn_trace_writer(
    mut rx: TraceReceiver,
    mut sink: TraceSink,
) -> std::thread::JoinHandle<std::io::Result<()>> {
    std::thread::spawn(move || {
        while let Some(record) = rx.blocking_recv() {
            writeln!(sink, "{}", record.to_json_line())?;
            sink.flush()?;
        }
        sink.flush()
    })
}

fn finish_trace_writer(writer: Option<std::thread::JoinHandle<std::io::Result<()>>>) {
    if let Some(handle) = writer {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => eprintln!("ERROR writing trace: {err}"),
            Err(_) => eprintln!("ERROR trace writer thread panicked"),
        }
    }
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
            say!("  -> action `{label}` invoked (on `{}`)", ev.event_type);
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
            say!("  -> service `{label}` invoked (input={input})");
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
    events_stdin: bool,
    trace_out: Option<TraceSink>,
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
        let trace_writer = trace_out.map(|sink| spawn_trace_writer(actor.subscribe_traces(), sink));
        let code = drive_async(&compiled, &actor, raw_events, events_stdin).await;
        actor.stop().await;
        actor.trace_hub().close();
        tokio::task::spawn_blocking(move || finish_trace_writer(trace_writer))
            .await
            .ok();
        code
    })
}

/// Spawns a detached OS thread reading lines from `stdin` so an open TTY/pipe never blocks Tokio shutdown.
fn spawn_stdin_line_reader() -> tokio::sync::mpsc::UnboundedReceiver<std::io::Result<String>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn actions_have_delayed_send(actions: &[ActionDef]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            ActionDef::Raise {
                delay: Some(_),
                ..
            } | ActionDef::SendTo {
                delay: Some(_),
                ..
            }
        )
    })
}

/// Returns `true` if any currently active state (or its active ancestor superstates) owns an
/// in-flight `invoke`, an `after` timer, or a delayed `raise_event` / `send_to`.
fn active_states_have_autonomous_work(ir: &MachineDef, active_paths: &[String]) -> bool {
    for raw_path in active_paths {
        let trimmed = raw_path
            .strip_prefix(&format!("{}.", ir.id))
            .unwrap_or(raw_path.as_str());
        let mut current_map = &ir.states;
        for seg in trimmed.split('.') {
            let Some(node) = current_map.get(&StateId::from(seg)) else {
                break;
            };
            if !node.invoke.is_empty()
                || !node.after.is_empty()
                || actions_have_delayed_send(&node.entry)
                || node
                    .on
                    .iter()
                    .any(|tr| actions_have_delayed_send(&tr.actions))
            {
                return true;
            }
            match &node.kind {
                StateKind::Compound { states, .. } | StateKind::Parallel { states } => {
                    current_map = states;
                }
                StateKind::Atomic | StateKind::Final | StateKind::History { .. } => break,
            }
        }
    }
    false
}

async fn drive_async(
    compiled: &CompiledChart,
    actor: &StarchartActor,
    raw_events: &[String],
    events_stdin: bool,
) -> ExitCode {
    let init_ctx = actor.context().await.unwrap_or(Value::Null);
    say!(
        "[init] machine=`{}` active={:?} context={}",
        compiled.ir.id,
        actor.active_states(),
        init_ctx
    );

    let mut step_num: usize = 0;
    for raw in raw_events {
        step_num += 1;
        if let Some(ms_str) = raw.trim().strip_prefix("sleep:") {
            let ms: u64 = match ms_str.trim().parse() {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("ERROR parsing `sleep:{ms_str}` in event #{step_num}: {err}");
                    return ExitCode::from(1);
                }
            };
            tokio::time::sleep(Duration::from_millis(ms)).await;
            continue;
        }

        let (event_type, payload) = match parse_cli_event(raw) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("ERROR parsing event #{step_num}: {err}");
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
        say!(
            "[step {}] event=`{}` active={:?} done={} output={} context={}",
            step_num,
            event_type,
            actor.active_states(),
            actor.is_done(),
            actor.output(),
            ctx
        );
    }

    if !actor.is_done() {
        let mut rx = actor.subscribe();
        let mut stdin_rx = events_stdin.then(spawn_stdin_line_reader);
        let mut stdin_open = events_stdin;
        let mut prev_states = actor.active_states();

        loop {
            let view = rx.borrow_and_update().clone();
            let current_states = actor.active_states();
            if current_states != prev_states {
                say!(
                    "[transition] machine=`{}` active={:?}",
                    compiled.ir.id, current_states
                );
                prev_states = current_states.clone();
            }
            if let Some(err) = view.error {
                eprintln!("ERROR executing `{}`: {err}", compiled.ir.id);
                return ExitCode::from(1);
            }
            if view.status != xstate_statemachine::core::Status::Running {
                break;
            }
            if !stdin_open && !active_states_have_autonomous_work(&compiled.ir, &current_states) {
                break;
            }

            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                maybe_line = async {
                    match &mut stdin_rx {
                        Some(ch) => ch.recv().await,
                        None => std::future::pending().await,
                    }
                }, if stdin_open => {
                    match maybe_line {
                        Some(Ok(line)) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() || trimmed.starts_with('#') {
                                continue;
                            }
                            if let Some(ms_str) = trimmed.strip_prefix("sleep:") {
                                match ms_str.trim().parse::<u64>() {
                                    Ok(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                                    Err(err) => eprintln!("WARN ignoring invalid `sleep:{ms_str}` on stdin: {err}"),
                                }
                                continue;
                            }
                            let (event_type, payload) = match parse_cli_event(trimmed) {
                                Ok(pair) => pair,
                                Err(err) => {
                                    eprintln!("WARN ignoring invalid stdin event `{trimmed}`: {err}");
                                    continue;
                                }
                            };
                            step_num += 1;
                            let res = if payload.is_null() {
                                actor.send(&event_type).await
                            } else {
                                actor.send_with_payload(&event_type, payload).await
                            };
                            if let Err(err) = res {
                                eprintln!("ERROR dispatching stdin event `{event_type}`: {err}");
                                return ExitCode::from(1);
                            }
                            let ctx = actor.context().await.unwrap_or(Value::Null);
                            prev_states = actor.active_states();
                            say!(
                                "[stdin {}] event=`{}` active={:?} done={} output={} context={}",
                                step_num,
                                event_type,
                                prev_states,
                                actor.is_done(),
                                actor.output(),
                                ctx
                            );
                            if actor.is_done() {
                                break;
                            }
                        }
                        Some(Err(err)) => {
                            eprintln!("ERROR reading stdin event stream: {err}");
                            stdin_open = false;
                            stdin_rx = None;
                        }
                        None => {
                            stdin_open = false;
                            stdin_rx = None;
                        }
                    }
                }
            }
        }
    }

    let final_ctx = actor.context().await.unwrap_or(Value::Null);
    say!(
        "[done] machine=`{}` active={:?} done={} output={} context={}",
        compiled.ir.id,
        actor.active_states(),
        actor.is_done(),
        actor.output(),
        final_ctx
    );
    ExitCode::SUCCESS
}

fn run_execute_sync(
    compiled: CompiledChart,
    action_names: HashSet<String>,
    service_names: HashSet<String>,
    raw_events: &[String],
    trace_out: Option<TraceSink>,
) -> ExitCode {
    let callbacks = build_cli_callbacks(action_names, service_names, false);

    let mut runner = match StarchartSyncRunner::start(&compiled, callbacks) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("ERROR starting `{}`: {err}", compiled.ir.id);
            return ExitCode::from(1);
        }
    };
    let trace_writer = trace_out.map(|sink| spawn_trace_writer(runner.subscribe_traces(), sink));
    let code = drive_sync(&compiled, &mut runner, raw_events);
    runner.trace_hub().close();
    drop(runner);
    finish_trace_writer(trace_writer);
    code
}

fn drive_sync(
    compiled: &CompiledChart,
    runner: &mut StarchartSyncRunner,
    raw_events: &[String],
) -> ExitCode {

    say!(
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
            say!(
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

        say!(
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
            say!("  -> emitted {emitted}");
        }
    }

    ExitCode::SUCCESS
}

fn run_serve(
    file: &Path,
    export: &str,
    bind: &str,
    context_override: Option<&str>,
    state_file: Option<PathBuf>,
    exit_on_done: bool,
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
    let callbacks = build_cli_callbacks(action_names, service_names, true);

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

    let options = ServeOptions {
        bind_addr: bind.to_owned(),
        state_file,
        exit_on_done,
    };

    rt.block_on(async move {
        let (listener, local_addr) = match bind_serve_listener(&options.bind_addr).await {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("ERROR {err:#}");
                return ExitCode::from(1);
            }
        };
        println!(
            "[serve] machine=`{}` listening on http://{local_addr}",
            compiled.ir.id
        );
        let _ = std::io::stdout().flush();

        match serve_chart_http(compiled, callbacks, listener, options).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("ERROR serving statechart: {err:#}");
                ExitCode::from(1)
            }
        }
    })
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

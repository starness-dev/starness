//! Lossless, ordered execution traces for running statecharts.
//!
//! Every [`StarchartSyncRunner`](crate::StarchartSyncRunner) and
//! [`StarchartActor`](crate::StarchartActor) owns a [`TraceHub`]. The hub records:
//!
//! * engine lifecycle (`started`, every `transition`, `event_dropped`, `error`, `stopped`), and
//! * invoked-service lifecycle (`service_started`, streaming `service_progress`, `service_finished`).
//!
//! Subscribers attach with [`TraceHub::subscribe`] from any task or thread. Each subscriber gets
//! its own unbounded queue, so a slow consumer never loses records and never slows the machine
//! down (it only buffers). Records retained in the hub's history (default: the last
//! [`DEFAULT_HISTORY_CAPACITY`] records) are replayed first, so a subscriber that attaches after
//! the machine started still sees `started` and everything since, with no gaps or duplicates.
//!
//! Services report streaming output with [`report_progress`]. The built-in `@starchart/acp`,
//! `@starchart/exec`, and `@starchart/http` drivers already do; custom
//! [`HostCallbacks`](crate::HostCallbacks) services can call it too. It is a cheap no-op when the
//! current service is not being traced.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use xstate_statemachine::core::engine::{ActiveState, StepReport};
use xstate_statemachine::core::{DynContext, Error as XStateError, Event, Machine, Observer};

/// Number of records a [`TraceHub`] keeps for replay to late subscribers.
pub const DEFAULT_HISTORY_CAPACITY: usize = 10_000;

/// Key of the metadata envelope wrapped around every `invoke` input at compile time.
pub(crate) const INVOKE_ENVELOPE_KEY: &str = "__starchart_invoke__";

/// Category of a streaming [`TraceEvent::ServiceProgress`] chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    /// Assistant message text streamed by an agent.
    AgentText,
    /// Reasoning / "thinking" text streamed by an agent.
    AgentThought,
    /// An agent tool call starting or changing status.
    ToolCall,
    /// An agent's execution plan update.
    Plan,
    /// A structured artifact submitted by an agent via a turn-scoped MCP `artifact_tool`.
    Artifact,
    /// One line of subprocess standard output.
    Stdout,
    /// One line of subprocess standard error.
    Stderr,
    /// HTTP request / response summary.
    Http,
    /// Free-form log line from a custom service.
    Log,
}

/// Snapshot of an event as seen by the trace.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceEventInfo {
    /// Event type, e.g. `"APPROVED"` or `"xstate.done.actor.planner"`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event payload (`null` when absent).
    pub payload: Value,
    /// Provenance: `external`, `internal`, `done_actor`, `error_actor`, `after`, `init`, ...
    pub origin: Value,
}

/// One kind of trace record. Serialized with a `kind` tag (`"transition"`, `"service_progress"`, ...).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // records are short-lived and cloned per subscriber anyway
pub enum TraceEvent {
    /// The machine entered its initial configuration.
    Started {
        states: Vec<String>,
        actions: Vec<String>,
        context: Value,
        status: String,
    },
    /// One event was processed to completion (a macrostep).
    Transition {
        event: TraceEventInfo,
        /// Active leaf states before the step.
        from: Vec<String>,
        /// Active leaf states after the step.
        to: Vec<String>,
        /// Actions and reducers executed, in order (reducers shown as `assign <fn>`).
        actions: Vec<String>,
        /// Whether configuration or context changed.
        changed: bool,
        /// `true` when no transition matched the event.
        unhandled: bool,
        /// Context after the step.
        context: Value,
        status: String,
        /// Machine output (non-null once done with an output).
        output: Value,
        /// Machine error message (once errored).
        error: Option<String>,
    },
    /// An event was dropped without being processed.
    EventDropped { event_type: String, reason: String },
    /// A step failed.
    Error { message: String },
    /// The machine was stopped.
    Stopped { states: Vec<String>, status: String },
    /// An invoked service started.
    ServiceStarted {
        invoke_id: String,
        state: String,
        src: String,
        input: Value,
    },
    /// A chunk of streaming output from a running service.
    ServiceProgress {
        invoke_id: String,
        state: String,
        src: String,
        stream: ProgressKind,
        chunk: String,
    },
    /// An invoked service completed or failed.
    ServiceFinished {
        invoke_id: String,
        state: String,
        src: String,
        ok: bool,
        output: Option<Value>,
        error: Option<String>,
        duration_ms: u64,
    },
}

/// A sequenced, timestamped [`TraceEvent`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceRecord {
    /// Monotonic sequence number within one machine run, starting at `1`.
    pub seq: u64,
    /// Milliseconds since the hub was created.
    pub t_ms: u64,
    /// Machine id.
    pub machine: String,
    /// Optional instance id when running inside a multi-instance host (`starchart serve`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(flatten)]
    pub event: TraceEvent,
}

impl TraceRecord {
    /// Serializes the record as one JSON line (no trailing newline).
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(r#"{{"seq":{},"kind":"serialize_error","error":"{e}"}}"#, self.seq)
        })
    }
}

struct HubInner {
    seq: u64,
    history: VecDeque<TraceRecord>,
    history_cap: usize,
    subscribers: Vec<mpsc::UnboundedSender<TraceRecord>>,
    closed: bool,
}

/// Fan-out point for one machine run's [`TraceRecord`]s.
pub struct TraceHub {
    machine_id: String,
    instance_id: Option<String>,
    started: Instant,
    action_labels: HashMap<String, String>,
    final_outputs: HashMap<String, Value>,
    inner: Mutex<HubInner>,
}

impl std::fmt::Debug for TraceHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceHub")
            .field("machine_id", &self.machine_id)
            .field("instance_id", &self.instance_id)
            .finish_non_exhaustive()
    }
}

impl TraceHub {
    /// Creates a hub for `machine_id`. `action_labels` maps internal action names
    /// (e.g. `__starlark_assign_0`) to readable labels (e.g. `assign save_plan`).
    pub fn new(machine_id: impl Into<String>, action_labels: HashMap<String, String>) -> Arc<Self> {
        Self::with_history_capacity(machine_id, action_labels, DEFAULT_HISTORY_CAPACITY)
    }

    /// Like [`TraceHub::new`] with a custom replay history size (`0` disables replay).
    pub fn with_history_capacity(
        machine_id: impl Into<String>,
        action_labels: HashMap<String, String>,
        history_cap: usize,
    ) -> Arc<Self> {
        Self::with_final_outputs(machine_id, action_labels, HashMap::new(), history_cap)
    }

    /// Creates a hub with both `action_labels` and top-level `final_state(output = ...)` fallbacks.
    pub fn with_final_outputs(
        machine_id: impl Into<String>,
        action_labels: HashMap<String, String>,
        final_outputs: HashMap<String, Value>,
        history_cap: usize,
    ) -> Arc<Self> {
        Self::with_instance_context(
            machine_id,
            None,
            0,
            action_labels,
            final_outputs,
            history_cap,
        )
    }

    /// Creates a hub optionally scoped to a specific `instance_id` and starting sequence offset.
    pub fn with_instance_context(
        machine_id: impl Into<String>,
        instance_id: Option<String>,
        base_seq: u64,
        action_labels: HashMap<String, String>,
        final_outputs: HashMap<String, Value>,
        history_cap: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            machine_id: machine_id.into(),
            instance_id,
            started: Instant::now(),
            action_labels,
            final_outputs,
            inner: Mutex::new(HubInner {
                seq: base_seq,
                history: VecDeque::new(),
                history_cap,
                subscribers: Vec::new(),
                closed: false,
            }),
        })
    }

    /// Machine id this hub traces.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// Optional instance id this hub traces.
    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }

    fn lock(&self) -> MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `event`, assigning the next sequence number, and delivers it to every subscriber.
    pub fn emit(&self, event: TraceEvent) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        inner.seq += 1;
        let record = TraceRecord {
            seq: inner.seq,
            t_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            machine: self.machine_id.clone(),
            instance_id: self.instance_id.clone(),
            event,
        };
        inner
            .subscribers
            .retain(|tx| tx.send(record.clone()).is_ok());
        if inner.history_cap > 0 {
            if inner.history.len() == inner.history_cap {
                inner.history.pop_front();
            }
            inner.history.push_back(record);
        }
    }

    /// Subscribes to this run's trace. Retained history is replayed first, then live records
    /// follow in order with no gaps or duplicates. After [`TraceHub::close`] the receiver
    /// yields the replayed history and then ends.
    pub fn subscribe(&self) -> TraceReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.lock();
        for record in &inner.history {
            let _ = tx.send(record.clone());
        }
        if !inner.closed {
            inner.subscribers.push(tx);
        }
        TraceReceiver { rx }
    }

    /// Returns a copy of the retained history.
    pub fn history(&self) -> Vec<TraceRecord> {
        self.lock().history.iter().cloned().collect()
    }

    /// Stops accepting records and ends every subscriber's stream once drained.
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.subscribers.clear();
    }

    pub(crate) fn label_actions<'a>(&self, names: impl Iterator<Item = &'a str>) -> Vec<String> {
        names
            .map(|n| {
                self.action_labels
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| n.to_owned())
            })
            .collect()
    }

    pub(crate) fn strip_machine_prefix(&self, ids: Vec<String>) -> Vec<String> {
        let prefix = format!("{}.", self.machine_id);
        ids.into_iter()
            .map(|s| s.strip_prefix(&prefix).map(str::to_owned).unwrap_or(s))
            .collect()
    }
}

/// Receiving end of a trace subscription. Usable from async code ([`TraceReceiver::recv`])
/// or plain threads ([`TraceReceiver::blocking_recv`]).
#[derive(Debug)]
pub struct TraceReceiver {
    rx: mpsc::UnboundedReceiver<TraceRecord>,
}

impl TraceReceiver {
    /// Waits for the next record; `None` once the hub is closed and the queue is drained.
    pub async fn recv(&mut self) -> Option<TraceRecord> {
        self.rx.recv().await
    }

    /// Blocking variant of [`TraceReceiver::recv`] for non-async threads.
    /// Panics if called from inside an async runtime context.
    pub fn blocking_recv(&mut self) -> Option<TraceRecord> {
        self.rx.blocking_recv()
    }

    /// Returns the next record if one is queued.
    pub fn try_recv(&mut self) -> Option<TraceRecord> {
        self.rx.try_recv().ok()
    }

    /// Drains every currently queued record.
    pub fn drain(&mut self) -> Vec<TraceRecord> {
        let mut out = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            out.push(r);
        }
        out
    }
}

/// Engine observer translating `xstate-core` lifecycle hooks into [`TraceRecord`]s.
pub(crate) struct TraceObserver {
    hub: Arc<TraceHub>,
    last_states: Mutex<Vec<String>>,
}

impl TraceObserver {
    pub(crate) fn new(hub: Arc<TraceHub>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            last_states: Mutex::new(Vec::new()),
        })
    }

    fn swap_states(&self, next: &[String]) -> Vec<String> {
        let mut guard = self
            .last_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *guard, next.to_vec())
    }
}

fn event_info(event: &Event) -> TraceEventInfo {
    TraceEventInfo {
        event_type: event.ty.to_string(),
        payload: event.payload.clone(),
        origin: serde_json::to_value(event.origin).unwrap_or(Value::Null),
    }
}

impl Observer<DynContext> for TraceObserver {
    fn on_start(&self, machine: &Machine, state: &ActiveState<DynContext>, report: &StepReport) {
        let states = self.hub.strip_machine_prefix(state.leaf_ids(machine));
        self.swap_states(&states);
        self.hub.emit(TraceEvent::Started {
            states,
            actions: self.hub.label_actions(report.actions.iter().map(|a| &**a)),
            context: state.context().clone(),
            status: state.status().as_str().to_owned(),
        });
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<DynContext>,
    ) {
        let to = self.hub.strip_machine_prefix(state.leaf_ids(machine));
        let from = self.swap_states(&to);
        let mut output = state.output().clone();
        if output.is_null() && state.status() == xstate_statemachine::core::Status::Done {
            for st in &to {
                if let Some(leaf_out) = self.hub.final_outputs.get(st) {
                    output = leaf_out.clone();
                    break;
                }
            }
        }
        self.hub.emit(TraceEvent::Transition {
            event: event_info(event),
            from,
            to,
            actions: self.hub.label_actions(report.actions.iter().map(|a| &**a)),
            changed: report.changed,
            unhandled: report.unhandled,
            context: state.context().clone(),
            status: state.status().as_str().to_owned(),
            output,
            error: state.error().map(str::to_owned),
        });
    }

    fn on_event_dropped(&self, _machine: &Machine, event: &Event, reason: &'static str) {
        self.hub.emit(TraceEvent::EventDropped {
            event_type: event.ty.to_string(),
            reason: reason.to_owned(),
        });
    }

    fn on_error(&self, _machine: &Machine, error: &XStateError) {
        self.hub.emit(TraceEvent::Error {
            message: error.to_string(),
        });
    }

    fn on_stop(&self, machine: &Machine, state: &ActiveState<DynContext>) {
        self.hub.emit(TraceEvent::Stopped {
            states: self.hub.strip_machine_prefix(state.leaf_ids(machine)),
            status: state.status().as_str().to_owned(),
        });
    }
}

/// Identity of one `invoke`, recovered from the compile-time input envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvokeMeta {
    pub invoke_id: String,
    pub state: String,
    pub src: String,
}

/// Splits a raw service input into its invoke metadata (if present) and the user's input.
pub(crate) fn split_invoke_envelope(raw: &Value, src: &str) -> (Option<InvokeMeta>, Value) {
    if let Value::Object(map) = raw
        && let Some(Value::Object(meta)) = map.get(INVOKE_ENVELOPE_KEY)
    {
        let invoke_id = meta
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(src)
            .to_owned();
        let state = meta
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let inner = map.get("input").cloned().unwrap_or(Value::Null);
        return (
            Some(InvokeMeta {
                invoke_id,
                state,
                src: src.to_owned(),
            }),
            inner,
        );
    }
    (None, raw.clone())
}

/// Active service scope: where [`report_progress`] chunks are delivered.
#[derive(Clone)]
pub(crate) struct ServiceScope {
    pub hub: Arc<TraceHub>,
    pub meta: InvokeMeta,
}

impl ServiceScope {
    pub(crate) fn started(&self, input: &Value) {
        self.hub.emit(TraceEvent::ServiceStarted {
            invoke_id: self.meta.invoke_id.clone(),
            state: self.meta.state.clone(),
            src: self.meta.src.clone(),
            input: input.clone(),
        });
    }

    pub(crate) fn finished(&self, result: Result<&Value, String>, started: Instant) {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (ok, output, error) = match result {
            Ok(v) => (true, Some(v.clone()), None),
            Err(e) => (false, None, Some(e)),
        };
        self.hub.emit(TraceEvent::ServiceFinished {
            invoke_id: self.meta.invoke_id.clone(),
            state: self.meta.state.clone(),
            src: self.meta.src.clone(),
            ok,
            output,
            error,
            duration_ms,
        });
    }

    fn progress(&self, stream: ProgressKind, chunk: String) {
        self.hub.emit(TraceEvent::ServiceProgress {
            invoke_id: self.meta.invoke_id.clone(),
            state: self.meta.state.clone(),
            src: self.meta.src.clone(),
            stream,
            chunk,
        });
    }
}

tokio::task_local! {
    static ASYNC_SCOPE: ServiceScope;
}

thread_local! {
    static SYNC_SCOPE: RefCell<Option<ServiceScope>> = const { RefCell::new(None) };
}

/// Runs `fut` with `scope` as the current service scope for [`report_progress`].
pub(crate) async fn with_async_scope<F: std::future::Future>(scope: ServiceScope, fut: F) -> F::Output {
    ASYNC_SCOPE.scope(scope, fut).await
}

/// Runs `f` with `scope` as the current service scope for [`report_progress`] on this thread.
pub(crate) fn with_sync_scope<R>(scope: ServiceScope, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<ServiceScope>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let prev = self.0.take();
            SYNC_SCOPE.with(|s| *s.borrow_mut() = prev);
        }
    }
    let prev = SYNC_SCOPE.with(|s| s.borrow_mut().replace(scope));
    let _reset = Reset(prev);
    f()
}

pub(crate) fn current_scope() -> Option<ServiceScope> {
    ASYNC_SCOPE
        .try_with(Clone::clone)
        .ok()
        .or_else(|| SYNC_SCOPE.with(|s| s.borrow().clone()))
}

/// Returns `true` when called from inside a traced service, so callers can skip
/// building progress strings that nobody will see.
pub fn progress_enabled() -> bool {
    ASYNC_SCOPE.try_with(|_| ()).is_ok() || SYNC_SCOPE.with(|s| s.borrow().is_some())
}

/// Reports one chunk of streaming output from the currently running service.
///
/// Call this from inside a service registered with
/// [`HostCallbacks::on_async_service`](crate::HostCallbacks::on_async_service) or
/// [`HostCallbacks::on_sync_service`](crate::HostCallbacks::on_sync_service). Outside a traced
/// service (or from a task spawned with `tokio::spawn`, which does not inherit the scope) it
/// does nothing.
pub fn report_progress(stream: ProgressKind, chunk: impl Into<String>) {
    if let Some(scope) = current_scope() {
        scope.progress(stream, chunk.into());
    }
}

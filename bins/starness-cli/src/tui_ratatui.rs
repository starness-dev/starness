//! Fullscreen Ratatui TUI (`starness run -i`) with Bubble Tea–style Model/Update/View loop.
//!
//! Renders all 15 `@starness/ui` `json-render` widgets (`Stack`, `Card`, `Markdown`, `Alert`,
//! `KeyValue`, `Table`, `Badge`, `TextInput`, `Textarea`, `Select`, `MultiSelect`, `Button`,
//! `Confirm`, `MarkdownReview`, `DiffReview`) plus a zero-config `allowed_events` picker,
//! live streaming trace drawer (`t`), live IR statechart diagram drawer (`g`), and context JSON
//! drawer (`x`).

use std::cell::RefCell;
use std::io;
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, Paragraph, Row, Table as RatatuiTable,
    TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use serde_json::{Map, Value, json};
use starness_ir::{MachineDef, StateId, StateKind, StateNodeDef, UiViewSheet};
use starness_runtime::{
    ProgressKind, StarnessActor, TraceEvent, TraceRecord, active_states_have_autonomous_work,
};
use starness_starlark::CompiledChart;
use xstate_statemachine::core::Status;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// Cohesive 256-color palette (works cleanly in macOS Terminal.app, iTerm2, Ghostty, tmux, etc.
// without 24-bit RGB SGR sequences that Terminal.app misparses as dim/black).
const BORDER_MUTED: Color = Color::Indexed(240);
const BORDER_FOCUS: Color = Color::Indexed(75);
const ACCENT: Color = Color::Indexed(75);
const TEXT_PRIMARY: Color = Color::Reset;
const TEXT_SECONDARY: Color = Color::Indexed(250);
const TEXT_MUTED: Color = Color::Indexed(244);
const CODE_FG: Color = Color::Indexed(117);
const CODE_BG: Color = Color::Indexed(236);
const CURSOR_BG: Color = Color::Indexed(237);
const COMMENT_ACCENT: Color = Color::Indexed(221);
const NOTE_ACCENT: Color = Color::Indexed(111);
const SUCCESS: Color = Color::Indexed(114);
const WARNING: Color = Color::Indexed(221);
const DANGER: Color = Color::Indexed(210);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawerMode {
    None,
    Trace,
    Diagram,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalFocus {
    Input,
    Save,
    Cancel,
}

#[derive(Debug, Clone)]
enum InputModal {
    MarkdownComment {
        comments_path: String,
        line: usize,
        heading: String,
        quote: String,
        edit_index: Option<usize>,
        buffer: String,
    },
    DiffComment {
        comments_path: String,
        file: String,
        line: usize,
        quote: String,
        edit_index: Option<usize>,
        buffer: String,
    },
    TextField {
        label: String,
        bind: String,
        buffer: String,
    },
    FallbackPayload {
        event: String,
        buffer: String,
    },
}

#[derive(Debug, Clone)]
struct ActionBarItem {
    label: String,
    event: String,
    variant: String,
    payload: Map<String, Value>,
    comments_path: Option<String>,
    show_comment_count: bool,
}

#[derive(Debug, Clone)]
struct TuiToolCall {
    id: Option<String>,
    name: String,
    status: String,
    arguments: Option<String>,
    output: Option<String>,
}

#[derive(Debug, Clone)]
struct TuiChatMessage {
    role: String,
    text: String,
    reasoning: Option<String>,
    tool_calls: Vec<TuiToolCall>,
    tool_name: Option<String>,
}

#[derive(Debug, Clone)]
struct TuiTaskItem {
    title: String,
    status: String,
    detail: Option<String>,
}

#[derive(Debug, Clone)]
enum FocusWidget {
    MarkdownReview {
        props: Map<String, Value>,
        selected_line: usize,
    },
    DiffReview {
        props: Map<String, Value>,
        selected_line: usize,
    },
    Table {
        title: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        selected_row: usize,
    },
    Conversation {
        title: Option<String>,
        messages: Vec<TuiChatMessage>,
        empty_text: String,
        scroll: usize,
    },
    PromptInput {
        label: Option<String>,
        placeholder: String,
        submit_event: String,
        submit_label: String,
        field_name: String,
        interrupt_event: Option<String>,
        interrupt_label: String,
        buffer: String,
    },
    ToolCall {
        name: String,
        status: String,
        arguments: Option<String>,
        output: Option<String>,
        error: Option<String>,
        expanded: bool,
    },
    Reasoning {
        title: String,
        content: String,
        duration_label: Option<String>,
        expanded: bool,
    },
    Terminal {
        title: Option<String>,
        command: Option<String>,
        stdout: String,
        stderr: String,
        exit_code: Option<i64>,
        scroll: usize,
    },
    TextInput {
        label: String,
        bind: String,
    },
    Select {
        label: String,
        bind: String,
        options: Vec<String>,
        cursor: usize,
    },
    MultiSelect {
        label: String,
        bind: String,
        options: Vec<(String, String, Option<String>)>,
        cursor: usize,
    },
    ActionBar {
        buttons: Vec<ActionBarItem>,
        cursor: usize,
    },
    FallbackPicker {
        events: Vec<String>,
        cursor: usize,
    },
}

#[derive(Debug, Clone)]
struct DisplayItem {
    kind: DisplayKind,
}

#[derive(Debug, Clone)]
enum DisplayKind {
    Heading(String),
    Badge(String, String),
    Alert {
        title: String,
        message: String,
        variant: String,
    },
    KeyValue {
        title: Option<String>,
        entries: Vec<(String, String)>,
    },
    Markdown {
        title: Option<String>,
        content: String,
    },
    TaskList {
        title: Option<String>,
        tasks: Vec<TuiTaskItem>,
    },
}

#[derive(Debug, Clone)]
struct MdLine {
    line_num: usize,
    heading: String,
    is_heading_line: bool,
    raw: String,
    in_code_fence: bool,
}

#[derive(Debug, Clone)]
struct DiffLineItem {
    file: String,
    line_num: usize,
    kind: char, // '+', '-', '@', ' '
    text: String,
}

#[derive(Debug, Clone)]
enum AppAction {
    None,
    Dispatch { event_type: String, payload: Value },
    Quit,
}

struct AppModel {
    ir: MachineDef,
    ui_sheet: Option<UiViewSheet>,
    active_states: Vec<String>,
    context: Value,
    draft_state: Value,
    status: Status,
    output: Value,
    error: Option<String>,
    drawer: DrawerMode,
    spinner_tick: usize,
    traces: Vec<String>,
    live_progress: Vec<(ProgressKind, String)>,
    display_items: Vec<DisplayItem>,
    widgets: Vec<FocusWidget>,
    focused: usize,
    saved_focused: usize,
    saved_table_row: usize,
    scroll_offset: u16,
    modal: Option<InputModal>,
    modal_focus: ModalFocus,
    status_flash: Option<String>,
    last_state_key: String,
    link_hotspot: RefCell<Option<(Rect, String)>>,
}

impl AppModel {
    fn new(
        ir: MachineDef,
        ui_sheet: Option<UiViewSheet>,
        active_states: Vec<String>,
        context: Value,
    ) -> Self {
        let mut model = Self {
            ir,
            ui_sheet,
            active_states: Vec::new(),
            context: Value::Null,
            draft_state: json!({}),
            status: Status::Running,
            output: Value::Null,
            error: None,
            drawer: DrawerMode::None,
            spinner_tick: 0,
            traces: Vec::new(),
            live_progress: Vec::new(),
            display_items: Vec::new(),
            widgets: Vec::new(),
            focused: 0,
            saved_focused: 0,
            saved_table_row: 0,
            scroll_offset: 0,
            modal: None,
            modal_focus: ModalFocus::Input,
            status_flash: None,
            last_state_key: String::new(),
            link_hotspot: RefCell::new(None),
        };
        model.sync_machine_state(active_states, context, Status::Running, Value::Null, None);
        model
    }

    fn is_autonomous(&self) -> bool {
        self.status == Status::Running
            && active_states_have_autonomous_work(&self.ir, &self.active_states)
    }

    fn has_active_invoke(&self) -> bool {
        self.status == Status::Running && active_states_have_invoke(&self.ir, &self.active_states)
    }

    fn sync_machine_state(
        &mut self,
        active_states: Vec<String>,
        context: Value,
        status: Status,
        output: Value,
        error: Option<String>,
    ) {
        let ctx_fingerprint = serde_json::to_string(&context).unwrap_or_default();
        let next_key = format!("{}|{}|{status:?}", active_states.join(","), ctx_fingerprint);
        self.active_states = active_states;
        self.context = context;
        self.status = status;
        self.output = output;
        self.error = error;

        if self.last_state_key != next_key {
            self.last_state_key = next_key;
            self.draft_state = if self.context.is_object() {
                self.context.clone()
            } else {
                json!({})
            };
            self.rebuild_widgets();
        }
    }

    fn rebuild_widgets(&mut self) {
        if !self.widgets.is_empty() {
            self.saved_focused = self.focused;
            for w in &self.widgets {
                if let FocusWidget::Table { selected_row, .. } = w {
                    self.saved_table_row = *selected_row;
                }
            }
        }
        self.display_items.clear();
        self.widgets.clear();
        self.focused = 0;
        self.scroll_offset = 0;

        if self.status != Status::Running {
            return;
        }

        let mut action_buttons = Vec::new();
        if let Some(ui_spec) = self.ui_sheet.as_ref().and_then(|s| {
            s.resolve_for_active_states(&self.ir, &self.active_states, &self.context)
        }) && let (Some(root_id), Some(elements)) = (
                ui_spec.get("root").and_then(Value::as_str),
                ui_spec.get("elements").and_then(Value::as_object),
            )
        {
            collect_spec_nodes(
                root_id,
                elements,
                &self.draft_state,
                &mut self.display_items,
                &mut self.widgets,
                &mut action_buttons,
            );
        }
        if !action_buttons.is_empty() {
            self.widgets.push(FocusWidget::ActionBar {
                buttons: action_buttons,
                cursor: 0,
            });
        }

        if !self.has_active_invoke() && self.widgets.is_empty() {
            let allowed = self.ir.allowed_events_for_active_states(&self.active_states);
            if !allowed.is_empty() {
                self.widgets.push(FocusWidget::FallbackPicker {
                    events: allowed,
                    cursor: 0,
                });
            }
        }

        if !self.widgets.is_empty() {
            self.focused = self.saved_focused.min(self.widgets.len() - 1);
            for w in &mut self.widgets {
                if let FocusWidget::Table {
                    rows, selected_row, ..
                } = w
                    && !rows.is_empty()
                {
                    *selected_row = self.saved_table_row.min(rows.len() - 1);
                }
            }
        }
    }

    fn push_trace_record(&mut self, rec: &TraceRecord) {
        let line = match &rec.event {
            TraceEvent::Started { states, .. } => format!("#{} started {:?}", rec.seq, states),
            TraceEvent::Transition {
                event, from, to, ..
            } => format!(
                "#{} {} : {} -> {}",
                rec.seq,
                event.event_type,
                from.join(","),
                to.join(",")
            ),
            TraceEvent::ServiceStarted { invoke_id, src, .. } => {
                format!("#{} invoke start {invoke_id} ({src})", rec.seq)
            }
            TraceEvent::ServiceProgress { stream, chunk, .. } => {
                let s = format!("[{stream:?}] {chunk}");
                self.live_progress.push((*stream, chunk.clone()));
                if self.live_progress.len() > 200 {
                    self.live_progress.remove(0);
                }
                format!("#{} {s}", rec.seq)
            }
            TraceEvent::ServiceFinished {
                invoke_id,
                ok,
                duration_ms,
                ..
            } => format!(
                "#{} invoke {} ({}ms) ok={ok}",
                rec.seq, invoke_id, duration_ms
            ),
            TraceEvent::Error { message } => format!("#{} ERROR: {message}", rec.seq),
            TraceEvent::Stopped { states, .. } => format!("#{} stopped {:?}", rec.seq, states),
            TraceEvent::EventDropped { event_type, reason } => {
                format!("#{} dropped {event_type}: {reason}", rec.seq)
            }
        };
        self.traces.push(line);
        if self.traces.len() > 300 {
            self.traces.remove(0);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }

        // Handle active modal input first
        if let Some(modal) = self.modal.take() {
            return self.handle_modal_key(modal, key);
        }

        // Terminal states: q or Enter exits cleanly
        if self.status != Status::Running {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Enter | KeyCode::Esc => AppAction::Quit,
                KeyCode::Char('t') => {
                    self.toggle_drawer(DrawerMode::Trace);
                    AppAction::None
                }
                KeyCode::Char('g') => {
                    self.toggle_drawer(DrawerMode::Diagram);
                    AppAction::None
                }
                KeyCode::Char('x') => {
                    self.toggle_drawer(DrawerMode::Context);
                    AppAction::None
                }
                _ => AppAction::None,
            };
        }

        let typing_prompt = matches!(
            self.widgets.get(self.focused),
            Some(FocusWidget::PromptInput { .. })
        );

        // Global shortcuts (when not inside a modal or actively typing in PromptInput)
        match key.code {
            KeyCode::Char('q') if !typing_prompt => return AppAction::Quit,
            KeyCode::Char('t') if !typing_prompt => {
                self.toggle_drawer(DrawerMode::Trace);
                return AppAction::None;
            }
            KeyCode::Char('g') if !typing_prompt => {
                self.toggle_drawer(DrawerMode::Diagram);
                return AppAction::None;
            }
            KeyCode::Char('x') if !typing_prompt => {
                self.toggle_drawer(DrawerMode::Context);
                return AppAction::None;
            }
            KeyCode::Tab => {
                if !self.widgets.is_empty() {
                    self.focused = (self.focused + 1) % self.widgets.len();
                }
                return AppAction::None;
            }
            KeyCode::BackTab => {
                if !self.widgets.is_empty() {
                    self.focused = if self.focused == 0 {
                        self.widgets.len() - 1
                    } else {
                        self.focused - 1
                    };
                }
                return AppAction::None;
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(6);
                return AppAction::None;
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(6);
                return AppAction::None;
            }
            _ => {}
        }

        if self.widgets.is_empty() {
            return AppAction::None;
        }

        self.handle_focused_widget_key(key)
    }

    fn toggle_drawer(&mut self, mode: DrawerMode) {
        self.drawer = if self.drawer == mode {
            DrawerMode::None
        } else {
            mode
        };
    }

    fn commit_modal(&mut self, modal: InputModal) -> AppAction {
        self.modal_focus = ModalFocus::Input;
        match modal {
            InputModal::MarkdownComment {
                comments_path,
                line,
                heading,
                quote,
                edit_index,
                buffer,
            } => {
                let text = buffer.trim().to_owned();
                let mut comments = self
                    .draft_state
                    .pointer(&comments_path)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                match (edit_index, text.is_empty()) {
                    (Some(idx), true) if idx < comments.len() => {
                        comments.remove(idx);
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Deleted comment on L{line}"));
                    }
                    (Some(idx), false) if idx < comments.len() => {
                        comments[idx] = json!({
                            "line": line,
                            "heading": heading,
                            "quote": quote,
                            "comment": text,
                        });
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Updated comment on L{line}"));
                    }
                    (None, false) => {
                        comments.push(json!({
                            "line": line,
                            "heading": heading,
                            "quote": quote,
                            "comment": text,
                        }));
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Comment attached to L{line}"));
                    }
                    _ => {}
                }
            }
            InputModal::DiffComment {
                comments_path,
                file,
                line,
                quote,
                edit_index,
                buffer,
            } => {
                let text = buffer.trim().to_owned();
                let mut comments = self
                    .draft_state
                    .pointer(&comments_path)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                match (edit_index, text.is_empty()) {
                    (Some(idx), true) if idx < comments.len() => {
                        comments.remove(idx);
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Deleted comment on {file}:{line}"));
                    }
                    (Some(idx), false) if idx < comments.len() => {
                        comments[idx] = json!({
                            "file": file,
                            "line": line,
                            "quote": quote,
                            "comment": text,
                        });
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Updated comment on {file}:{line}"));
                    }
                    (None, false) => {
                        comments.push(json!({
                            "file": file,
                            "line": line,
                            "quote": quote,
                            "comment": text,
                        }));
                        set_json_pointer(
                            &mut self.draft_state,
                            &comments_path,
                            Value::Array(comments),
                        );
                        self.status_flash = Some(format!("Comment attached to {file}:{line}"));
                    }
                    _ => {}
                }
            }
            InputModal::TextField { bind, buffer, .. } => {
                set_json_pointer(&mut self.draft_state, &bind, Value::String(buffer));
                self.status_flash = Some("Field updated".to_owned());
            }
            InputModal::FallbackPayload { event, buffer } => {
                let trimmed = buffer.trim();
                let payload = if trimmed.is_empty() || trimmed == "{}" {
                    Value::Null
                } else {
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(v) => v,
                        Err(err) => {
                            self.status_flash = Some(format!("Invalid JSON: {err}"));
                            self.modal = Some(InputModal::FallbackPayload { event, buffer });
                            return AppAction::None;
                        }
                    }
                };
                return AppAction::Dispatch {
                    event_type: event,
                    payload,
                };
            }
        }
        AppAction::None
    }

    fn handle_modal_key(&mut self, mut modal: InputModal, key: KeyEvent) -> AppAction {
        if key.code == KeyCode::Esc {
            self.modal_focus = ModalFocus::Input;
            self.status_flash = Some("Cancelled".to_owned());
            return AppAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.commit_modal(modal);
        }
        match key.code {
            KeyCode::Tab => {
                self.modal_focus = match self.modal_focus {
                    ModalFocus::Input => ModalFocus::Save,
                    ModalFocus::Save => ModalFocus::Cancel,
                    ModalFocus::Cancel => ModalFocus::Input,
                };
                self.modal = Some(modal);
                return AppAction::None;
            }
            KeyCode::BackTab => {
                self.modal_focus = match self.modal_focus {
                    ModalFocus::Input => ModalFocus::Cancel,
                    ModalFocus::Save => ModalFocus::Input,
                    ModalFocus::Cancel => ModalFocus::Save,
                };
                self.modal = Some(modal);
                return AppAction::None;
            }
            _ => {}
        }

        match self.modal_focus {
            ModalFocus::Save => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => self.commit_modal(modal),
                KeyCode::Right | KeyCode::Char('l') => {
                    self.modal_focus = ModalFocus::Cancel;
                    self.modal = Some(modal);
                    AppAction::None
                }
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h') => {
                    self.modal_focus = ModalFocus::Input;
                    self.modal = Some(modal);
                    AppAction::None
                }
                _ => {
                    self.modal = Some(modal);
                    AppAction::None
                }
            },
            ModalFocus::Cancel => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.modal_focus = ModalFocus::Input;
                    self.status_flash = Some("Cancelled".to_owned());
                    AppAction::None
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    self.modal_focus = ModalFocus::Save;
                    self.modal = Some(modal);
                    AppAction::None
                }
                KeyCode::Up => {
                    self.modal_focus = ModalFocus::Input;
                    self.modal = Some(modal);
                    AppAction::None
                }
                _ => {
                    self.modal = Some(modal);
                    AppAction::None
                }
            },
            ModalFocus::Input => match key.code {
                KeyCode::Enter => {
                    match &mut modal {
                        InputModal::MarkdownComment { buffer, .. }
                        | InputModal::DiffComment { buffer, .. }
                        | InputModal::TextField { buffer, .. }
                        | InputModal::FallbackPayload { buffer, .. } => {
                            buffer.push('\n');
                        }
                    }
                    self.modal = Some(modal);
                    AppAction::None
                }
                KeyCode::Backspace => {
                    match &mut modal {
                        InputModal::MarkdownComment { buffer, .. }
                        | InputModal::DiffComment { buffer, .. }
                        | InputModal::TextField { buffer, .. }
                        | InputModal::FallbackPayload { buffer, .. } => {
                            buffer.pop();
                        }
                    }
                    self.modal = Some(modal);
                    AppAction::None
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match &mut modal {
                        InputModal::MarkdownComment { buffer, .. }
                        | InputModal::DiffComment { buffer, .. }
                        | InputModal::TextField { buffer, .. }
                        | InputModal::FallbackPayload { buffer, .. } => {
                            buffer.push(c);
                        }
                    }
                    self.modal = Some(modal);
                    AppAction::None
                }
                _ => {
                    self.modal = Some(modal);
                    AppAction::None
                }
            },
        }
    }

    fn handle_focused_widget_key(&mut self, key: KeyEvent) -> AppAction {
        let idx = self.focused.min(self.widgets.len() - 1);
        match &mut self.widgets[idx] {
            FocusWidget::MarkdownReview {
                props,
                selected_line,
            } => {
                let md = resolve_str(props.get("markdown"), &self.draft_state);
                let md_lines = parse_md_lines(&md);
                let comments_path = props
                    .get("comments_path")
                    .and_then(Value::as_str)
                    .unwrap_or("/draft_comments")
                    .to_owned();

                if !md_lines.is_empty() && *selected_line >= md_lines.len() {
                    *selected_line = md_lines.len() - 1;
                }

                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !md_lines.is_empty() && *selected_line + 1 < md_lines.len() {
                            *selected_line += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected_line = selected_line.saturating_sub(1);
                    }
                    KeyCode::Char('c') => {
                        if let Some(ml) = md_lines.get(*selected_line) {
                            let existing = self
                                .draft_state
                                .pointer(&comments_path)
                                .and_then(Value::as_array)
                                .and_then(|arr| {
                                    arr.iter().rposition(|c| comment_matches_md_line(c, ml)).map(
                                        |pos| {
                                            let txt = arr[pos]
                                                .get("comment")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_owned();
                                            (pos, txt)
                                        },
                                    )
                                });
                            let (edit_index, buffer) = match existing {
                                Some((idx, txt)) => (Some(idx), txt),
                                None => (None, String::new()),
                            };
                            self.modal_focus = ModalFocus::Input;
                            self.modal = Some(InputModal::MarkdownComment {
                                comments_path,
                                line: ml.line_num,
                                heading: ml.heading.clone(),
                                quote: ml.raw.trim().to_owned(),
                                edit_index,
                                buffer,
                            });
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(ml) = md_lines.get(*selected_line) {
                            let mut comments = self
                                .draft_state
                                .pointer(&comments_path)
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if let Some(pos) = comments.iter().rposition(|c| {
                                comment_matches_md_line(c, ml)
                            }) {
                                comments.remove(pos);
                                set_json_pointer(
                                    &mut self.draft_state,
                                    &comments_path,
                                    Value::Array(comments),
                                );
                                self.status_flash = Some(format!("Deleted comment on L{}", ml.line_num));
                            }
                        }
                    }
                    _ => {}
                }
            }
            FocusWidget::DiffReview {
                props,
                selected_line,
            } => {
                let diff_txt = resolve_str(props.get("diff"), &self.draft_state);
                let lines = parse_diff_lines(&diff_txt);
                let comments_path = props
                    .get("comments_path")
                    .and_then(Value::as_str)
                    .unwrap_or("/draft_comments")
                    .to_owned();

                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !lines.is_empty() && *selected_line + 1 < lines.len() {
                            *selected_line += 1;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *selected_line = selected_line.saturating_sub(1);
                    }
                    KeyCode::Char('c') => {
                        if let Some(dl) = lines.get(*selected_line) {
                            let existing = self
                                .draft_state
                                .pointer(&comments_path)
                                .and_then(Value::as_array)
                                .and_then(|arr| {
                                    arr.iter()
                                        .rposition(|c| {
                                            c.get("file").and_then(Value::as_str)
                                                == Some(dl.file.as_str())
                                                && c.get("line").and_then(Value::as_u64)
                                                    == Some(dl.line_num as u64)
                                        })
                                        .map(|pos| {
                                            let txt = arr[pos]
                                                .get("comment")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_owned();
                                            (pos, txt)
                                        })
                                });
                            let (edit_index, buffer) = match existing {
                                Some((idx, txt)) => (Some(idx), txt),
                                None => (None, String::new()),
                            };
                            self.modal_focus = ModalFocus::Input;
                            self.modal = Some(InputModal::DiffComment {
                                comments_path,
                                file: dl.file.clone(),
                                line: dl.line_num,
                                quote: dl.text.clone(),
                                edit_index,
                                buffer,
                            });
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(dl) = lines.get(*selected_line) {
                            let mut comments = self
                                .draft_state
                                .pointer(&comments_path)
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            if let Some(pos) = comments.iter().rposition(|c| {
                                c.get("file").and_then(Value::as_str) == Some(dl.file.as_str())
                                    && c.get("line").and_then(Value::as_u64)
                                        == Some(dl.line_num as u64)
                            }) {
                                comments.remove(pos);
                                set_json_pointer(
                                    &mut self.draft_state,
                                    &comments_path,
                                    Value::Array(comments),
                                );
                                self.status_flash = Some("Removed diff comment".to_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
            FocusWidget::Table {
                rows,
                selected_row,
                ..
            } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if !rows.is_empty() && *selected_row + 1 < rows.len() {
                        *selected_row += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected_row = selected_row.saturating_sub(1);
                }
                KeyCode::Char('o') => {
                    if let Some(row) = rows.get(*selected_row)
                        && let Some(url) = extract_row_urls(row).into_iter().next()
                    {
                        if open_url_in_browser(&url) {
                            self.status_flash = Some(format!("Opened {url}"));
                        } else {
                            self.status_flash = Some(format!("Link: {url}"));
                        }
                    }
                }
                _ => {}
            },
            FocusWidget::Conversation { scroll, .. } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                }
                _ => {}
            },
            FocusWidget::PromptInput {
                submit_event,
                field_name,
                interrupt_event,
                buffer,
                ..
            } => match key.code {
                KeyCode::Enter => {
                    let trimmed = buffer.trim().to_owned();
                    if !trimmed.is_empty() {
                        let ev = submit_event.clone();
                        let field = field_name.clone();
                        buffer.clear();
                        let mut map = Map::new();
                        map.insert(field, Value::String(trimmed));
                        return AppAction::Dispatch {
                            event_type: ev,
                            payload: Value::Object(map),
                        };
                    }
                }
                KeyCode::Esc => {
                    if let Some(ev) = interrupt_event.clone() {
                        return AppAction::Dispatch {
                            event_type: ev,
                            payload: json!({}),
                        };
                    }
                    buffer.clear();
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    buffer.push(c);
                }
                _ => {}
            },
            FocusWidget::ToolCall { expanded, .. } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    *expanded = !*expanded;
                }
            }
            FocusWidget::Reasoning { expanded, .. } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    *expanded = !*expanded;
                }
            }
            FocusWidget::Terminal { scroll, .. } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                }
                _ => {}
            },
            FocusWidget::TextInput { label, bind } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('e')) {
                    let current = self
                        .draft_state
                        .pointer(bind)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    self.modal = Some(InputModal::TextField {
                        label: label.clone(),
                        bind: bind.clone(),
                        buffer: current,
                    });
                }
            }
            FocusWidget::Select {
                bind,
                options,
                cursor,
                ..
            } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if !options.is_empty() && *cursor + 1 < options.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(opt) = options.get(*cursor) {
                        set_json_pointer(
                            &mut self.draft_state,
                            bind,
                            Value::String(opt.clone()),
                        );
                        self.status_flash = Some(format!("Selected `{opt}`"));
                    }
                }
                _ => {}
            },
            FocusWidget::MultiSelect {
                bind,
                options,
                cursor,
                ..
            } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if !options.is_empty() && *cursor + 1 < options.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    if let Some((opt_id, _, _)) = options.get(*cursor) {
                        let mut current: Vec<String> = self
                            .draft_state
                            .pointer(bind)
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if let Some(pos) = current.iter().position(|x| x == opt_id) {
                            current.remove(pos);
                        } else {
                            current.push(opt_id.clone());
                        }
                        set_json_pointer(
                            &mut self.draft_state,
                            bind,
                            Value::Array(current.into_iter().map(Value::String).collect()),
                        );
                    }
                }
                _ => {}
            },
            FocusWidget::ActionBar { buttons, cursor } => match key.code {
                KeyCode::Right | KeyCode::Char('l') => {
                    if !buttons.is_empty() && *cursor + 1 < buttons.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(btn) = buttons.get(*cursor) {
                        let payload = if let Some(path) = &btn.comments_path {
                            let comments = self
                                .draft_state
                                .pointer(path)
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            json!({ "comments": comments })
                        } else {
                            diff_draft_payload(&btn.payload, &self.context, &self.draft_state)
                        };
                        return AppAction::Dispatch {
                            event_type: btn.event.clone(),
                            payload,
                        };
                    }
                }
                _ => {}
            },
            FocusWidget::FallbackPicker { events, cursor } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if !events.is_empty() && *cursor + 1 < events.len() {
                        *cursor += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Enter => {
                    if let Some(ev) = events.get(*cursor) {
                        return AppAction::Dispatch {
                            event_type: ev.clone(),
                            payload: Value::Null,
                        };
                    }
                }
                KeyCode::Char('p') => {
                    if let Some(ev) = events.get(*cursor) {
                        self.modal = Some(InputModal::FallbackPayload {
                            event: ev.clone(),
                            buffer: "{}".to_owned(),
                        });
                    }
                }
                _ => {}
            },
        }
        AppAction::None
    }

    fn footer_hint_pairs(&self) -> Vec<(&'static str, &'static str)> {
        if self.modal.is_some() {
            return match self.modal_focus {
                ModalFocus::Input => vec![
                    ("Enter", "newline"),
                    ("Tab", "Save/Cancel"),
                    ("Ctrl+S", "save"),
                    ("Esc", "cancel"),
                ],
                ModalFocus::Save | ModalFocus::Cancel => vec![
                    ("Enter/Space", "select"),
                    ("←/→", "switch"),
                    ("Tab", "next"),
                    ("Esc", "cancel"),
                ],
            };
        }
        if self.status != Status::Running {
            return vec![("Enter/q", "exit")];
        }
        if self.is_autonomous() && self.widgets.is_empty() {
            return vec![("Running…", "waiting for state transition")];
        }
        let mut pairs: Vec<(&'static str, &'static str)> = match self.widgets.get(self.focused) {
            Some(FocusWidget::MarkdownReview {
                props,
                selected_line,
            }) => {
                let md = resolve_str(props.get("markdown"), &self.draft_state);
                let md_lines = parse_md_lines(&md);
                let comments_path = props
                    .get("comments_path")
                    .and_then(Value::as_str)
                    .unwrap_or("/draft_comments");
                let has_comment = md_lines
                    .get(*selected_line)
                    .and_then(|ml| {
                        self.draft_state
                            .pointer(comments_path)
                            .and_then(Value::as_array)
                            .map(|arr| arr.iter().any(|c| comment_matches_md_line(c, ml)))
                    })
                    .unwrap_or(false);
                let mut v = vec![
                    ("↑/↓", "line"),
                    ("c", if has_comment { "edit comment" } else { "comment" }),
                ];
                if has_comment {
                    v.push(("d", "del comment"));
                }
                v
            }
            Some(FocusWidget::DiffReview {
                props,
                selected_line,
            }) => {
                let diff_txt = resolve_str(props.get("diff"), &self.draft_state);
                let diff_lines = parse_diff_lines(&diff_txt);
                let comments_path = props
                    .get("comments_path")
                    .and_then(Value::as_str)
                    .unwrap_or("/draft_comments");
                let has_comment = diff_lines
                    .get(*selected_line)
                    .and_then(|dl| {
                        self.draft_state
                            .pointer(comments_path)
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter().any(|c| {
                                    c.get("file").and_then(Value::as_str) == Some(dl.file.as_str())
                                        && c.get("line").and_then(Value::as_u64)
                                            == Some(dl.line_num as u64)
                                })
                            })
                    })
                    .unwrap_or(false);
                let mut v = vec![
                    ("↑/↓", "line"),
                    ("c", if has_comment { "edit comment" } else { "comment" }),
                ];
                if has_comment {
                    v.push(("d", "del comment"));
                }
                v
            }
            Some(FocusWidget::Table {
                rows, selected_row, ..
            }) => {
                let has_url = rows
                    .get(*selected_row)
                    .is_some_and(|r| !extract_row_urls(r).is_empty());
                if has_url {
                    vec![("↑/↓", "row"), ("o", "open link")]
                } else {
                    vec![("↑/↓", "row")]
                }
            }
            Some(FocusWidget::Conversation { .. }) => vec![("↑/↓", "scroll")],
            Some(FocusWidget::PromptInput {
                interrupt_event, ..
            }) => {
                let mut v = vec![("Type", "prompt"), ("Enter", "send")];
                if interrupt_event.is_some() {
                    v.push(("Esc", "stop"));
                }
                v
            }
            Some(FocusWidget::ToolCall { expanded, .. }) => {
                vec![("Enter", if *expanded { "collapse" } else { "expand" })]
            }
            Some(FocusWidget::Reasoning { expanded, .. }) => {
                vec![("Enter", if *expanded { "collapse" } else { "expand" })]
            }
            Some(FocusWidget::Terminal { .. }) => vec![("↑/↓", "scroll")],
            Some(FocusWidget::ActionBar { .. }) => {
                vec![("←/→", "select action"), ("Enter", "trigger")]
            }
            Some(FocusWidget::TextInput { .. }) => vec![("Enter", "edit")],
            Some(FocusWidget::Select { .. }) => vec![("↑/↓", "move"), ("Enter", "select")],
            Some(FocusWidget::MultiSelect { .. }) => vec![("↑/↓", "move"), ("Space", "toggle")],
            Some(FocusWidget::FallbackPicker { .. }) => {
                vec![("↑/↓", "pick"), ("Enter", "send"), ("p", "+payload")]
            }
            None => Vec::new(),
        };
        if self.widgets.len() > 1 {
            let next_label = match self.widgets.get(self.focused) {
                Some(FocusWidget::ActionBar { .. }) => "back to content",
                _ if self.widgets.len() > 2 => "next panel",
                _ => "actions",
            };
            pairs.push(("Tab", next_label));
        }
        pairs
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some((rect, url)) = self.link_hotspot.borrow().clone()
            && mouse.column >= rect.x
            && mouse.column < rect.x.saturating_add(rect.width)
            && mouse.row >= rect.y
            && mouse.row < rect.y.saturating_add(rect.height)
        {
            if open_url_in_browser(&url) {
                self.status_flash = Some(format!("Opened {url}"));
            } else {
                self.status_flash = Some(format!("Link: {url}"));
            }
        }
    }
}

/// Runs the fullscreen Ratatui TUI bound to a live [`StarnessActor`].
pub async fn run_ratatui_app(
    compiled: &CompiledChart,
    ui_sheet: Option<UiViewSheet>,
    actor: &StarnessActor,
) -> anyhow::Result<std::process::ExitCode> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let run_res = run_ratatui_loop(&mut terminal, compiled, ui_sheet, actor).await;

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();

    let exit_code = run_res?;
    let final_ctx = actor.context().await.unwrap_or(Value::Null);
    println!(
        "[done] machine=`{}` active={:?} done={} output={}",
        compiled.ir.id,
        actor.active_states(),
        actor.is_done(),
        actor.output()
    );
    let _ = final_ctx;
    Ok(exit_code)
}

async fn run_ratatui_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    compiled: &CompiledChart,
    ui_sheet: Option<UiViewSheet>,
    actor: &StarnessActor,
) -> anyhow::Result<std::process::ExitCode>
where
    B::Error: Send + Sync + 'static,
{
    let init_ctx = actor.context().await.unwrap_or(Value::Null);
    let mut model = AppModel::new(compiled.ir.clone(), ui_sheet, actor.active_states(), init_ctx);
    let mut state_rx = actor.subscribe();
    let mut trace_rx = actor.subscribe_traces();
    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));

    loop {
        for rec in trace_rx.drain() {
            model.push_trace_record(&rec);
        }
        terminal.draw(|f| render_app(f, &model))?;

        tokio::select! {
            _ = tick.tick() => {
                model.spinner_tick = (model.spinner_tick + 1) % SPINNER_FRAMES.len();
            }
            changed = state_rx.changed() => {
                if changed.is_ok() {
                    let view = state_rx.borrow_and_update().clone();
                    let ctx = actor.context().await.unwrap_or(Value::Null);
                    model.sync_machine_state(
                        actor.active_states(),
                        ctx,
                        view.status,
                        actor.output(),
                        view.error,
                    );
                }
            }
            maybe_trace = trace_rx.recv() => {
                if let Some(rec) = maybe_trace {
                    model.push_trace_record(&rec);
                }
            }
            maybe_ev = term_events.next() => {
                match maybe_ev {
                    Some(Ok(TermEvent::Mouse(mouse))) => {
                        model.handle_mouse(mouse);
                    }
                    Some(Ok(TermEvent::Key(key))) => {
                        model.status_flash = None;
                        match model.handle_key(key) {
                            AppAction::None => {}
                            AppAction::Quit => break,
                            AppAction::Dispatch { event_type, payload } => {
                                let res = if payload.is_null() {
                                    actor.send(&event_type).await
                                } else {
                                    actor.send_with_payload(&event_type, payload).await
                                };
                                if let Err(err) = res {
                                    model.status_flash = Some(format!("Dispatch error: {err}"));
                                } else {
                                    let view = state_rx.borrow_and_update().clone();
                                    let ctx = actor.context().await.unwrap_or(Value::Null);
                                    model.sync_machine_state(
                                        actor.active_states(),
                                        ctx,
                                        view.status,
                                        actor.output(),
                                        view.error,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if model.error.is_some() {
        Ok(std::process::ExitCode::from(1))
    } else {
        Ok(std::process::ExitCode::SUCCESS)
    }
}

fn render_app(f: &mut Frame<'_>, model: &AppModel) {
    *model.link_hotspot.borrow_mut() = None;
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(f, outer[0], model);

    if model.drawer != DrawerMode::None && area.width >= 90 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(outer[1]);
        render_main_pane(f, split[0], model);
        render_drawer(f, split[1], model);
    } else if model.drawer != DrawerMode::None {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(outer[1]);
        render_main_pane(f, split[0], model);
        render_drawer(f, split[1], model);
    } else {
        render_main_pane(f, outer[1], model);
    }

    render_footer(f, outer[2], model);

    if let Some(modal) = &model.modal {
        render_modal(f, area, modal, model.modal_focus);
    }
}

fn render_header(f: &mut Frame<'_>, area: Rect, model: &AppModel) {
    let (badge_text, badge_color) = if model.status != Status::Running {
        if model.error.is_some() {
            ("✖ failed".to_owned(), DANGER)
        } else {
            ("✓ completed".to_owned(), SUCCESS)
        }
    } else if model.is_autonomous() {
        (
            format!("{} running", SPINNER_FRAMES[model.spinner_tick]),
            ACCENT,
        )
    } else {
        ("● waiting".to_owned(), WARNING)
    };

    let left_line = Line::from(vec![
        Span::styled(
            " ◆ starness ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(BORDER_MUTED)),
        Span::styled(
            model.ir.id.as_str(),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(BORDER_MUTED)),
        Span::styled(
            format!("[{}]", model.active_states.join(", ")),
            Style::default().fg(TEXT_SECONDARY),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            badge_text,
            Style::default()
                .fg(badge_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let view_pill = |key: &str, label: &str, active: bool| -> Vec<Span<'static>> {
        if active {
            vec![Span::styled(
                format!(" [{key}] {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )]
        } else {
            vec![
                Span::styled(
                    format!("[{key}]"),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {label}"), Style::default().fg(TEXT_SECONDARY)),
            ]
        }
    };

    let mut right_spans = Vec::new();
    right_spans.extend(view_pill("t", "Trace", model.drawer == DrawerMode::Trace));
    right_spans.push(Span::raw("  "));
    right_spans.extend(view_pill(
        "g",
        "Diagram",
        model.drawer == DrawerMode::Diagram,
    ));
    right_spans.push(Span::raw("  "));
    right_spans.extend(view_pill("x", "Ctx", model.drawer == DrawerMode::Context));
    right_spans.push(Span::styled("  │  ", Style::default().fg(BORDER_MUTED)));
    right_spans.push(Span::styled(
        "[q]",
        Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
    ));
    right_spans.push(Span::styled(" Exit ", Style::default().fg(TEXT_MUTED)));
    let left_w = left_line.width() as u16;
    let right_line = Line::from(right_spans);
    let right_w = right_line.width() as u16;

    let compact_right = Line::from(vec![
        Span::styled("[t]", Style::default().fg(if model.drawer == DrawerMode::Trace { WARNING } else { ACCENT }).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("[g]", Style::default().fg(if model.drawer == DrawerMode::Diagram { WARNING } else { ACCENT }).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled("[x]", Style::default().fg(if model.drawer == DrawerMode::Context { WARNING } else { ACCENT }).add_modifier(Modifier::BOLD)),
        Span::styled(" │ ", Style::default().fg(BORDER_MUTED)),
        Span::styled("[q] ", Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD)),
    ]);
    let compact_w = compact_right.width() as u16;

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_MUTED));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width >= left_w.saturating_add(right_w).saturating_add(2) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(left_w), Constraint::Length(right_w)])
            .split(inner);
        f.render_widget(Paragraph::new(left_line), cols[0]);
        f.render_widget(Paragraph::new(right_line), cols[1]);
    } else if inner.width >= left_w.saturating_add(compact_w).saturating_add(1) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(left_w), Constraint::Length(compact_w)])
            .split(inner);
        f.render_widget(Paragraph::new(left_line), cols[0]);
        f.render_widget(Paragraph::new(compact_right), cols[1]);
    } else {
        f.render_widget(Paragraph::new(left_line), inner);
    }
}

fn render_main_pane(f: &mut Frame<'_>, area: Rect, model: &AppModel) {
    if model.status != Status::Running {
        let (title, color, body) = if let Some(err) = &model.error {
            (" Workflow Failed ", DANGER, err.clone())
        } else {
            (
                " Workflow Completed ",
                SUCCESS,
                serde_json::to_string_pretty(&model.output).unwrap_or_default(),
            )
        };
        let p = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(title)
                    .border_style(Style::default().fg(color)),
            );
        f.render_widget(p, area);
        return;
    }

    if model.is_autonomous() && model.display_items.is_empty() && model.widgets.is_empty() {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("{} ", SPINNER_FRAMES[model.spinner_tick]),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("Running in [{}]…", model.active_states.join(", ")),
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ])];
        lines.push(Line::from(""));
        for (kind, chunk) in model.live_progress.iter().rev().take(24).rev() {
            let (prefix, color) = match kind {
                ProgressKind::AgentThought => ("◆ ", NOTE_ACCENT),
                ProgressKind::ToolCall => ("⚙ ", WARNING),
                ProgressKind::Plan => ("▸ ", SUCCESS),
                ProgressKind::AgentText => ("  ", TEXT_PRIMARY),
                ProgressKind::Stdout => ("$ ", CODE_FG),
                ProgressKind::Stderr => ("! ", DANGER),
                ProgressKind::Artifact => ("★ ", ACCENT),
                ProgressKind::Http => ("↗ ", TEXT_MUTED),
                ProgressKind::Log => ("· ", TEXT_SECONDARY),
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(color)),
                Span::styled(chunk.as_str(), Style::default().fg(color)),
            ]));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Running ")
            .border_style(Style::default().fg(BORDER_FOCUS));
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(block),
            area,
        );
        return;
    }

    // Split top display items (hugging content height) from interactive widgets below.
    let display_height = estimate_display_height(&model.display_items).min(area.height / 2);
    let (display_area, widgets_area) = if display_height > 0 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(display_height), Constraint::Min(6)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };

    if let Some(d_area) = display_area {
        render_display_items(f, d_area, &model.display_items);
    }

    render_interactive_widgets(f, widgets_area, model);
}

fn action_bar_height(buttons: &[ActionBarItem], area_width: u16) -> u16 {
    let inner_w = area_width.saturating_sub(2).max(20) as usize;
    let total_w: usize = buttons
        .iter()
        .map(|b| b.label.chars().count() + 10)
        .sum();
    let rows = total_w.div_ceil(inner_w).clamp(1, 3) as u16;
    rows + 2
}

fn estimate_display_height(items: &[DisplayItem]) -> u16 {
    if items.is_empty() {
        return 0;
    }
    let mut rows: u16 = 2; // outer border
    let skip_first_heading = matches!(items.first().map(|i| &i.kind), Some(DisplayKind::Heading(_)));
    let mut has_prev = false;
    for (idx, item) in items.iter().enumerate() {
        if idx == 0 && skip_first_heading {
            continue;
        }
        rows = rows.saturating_add(match &item.kind {
            DisplayKind::Heading(_) | DisplayKind::Badge(_, _) => 1,
            DisplayKind::Alert { .. } => 1 + u16::from(has_prev),
            DisplayKind::KeyValue { entries, title } => {
                if is_compact_kv(entries) {
                    1 + u16::from(title.is_some())
                } else {
                    entries.len() as u16 + u16::from(title.is_some())
                }
            }
            DisplayKind::Markdown { title, content } => {
                (content.lines().count() as u16).min(6)
                    + u16::from(title.is_some())
                    + u16::from(has_prev)
            }
            DisplayKind::TaskList { title, tasks } => {
                (tasks.len() as u16).clamp(1, 6)
                    + u16::from(title.is_some())
                    + u16::from(has_prev)
            }
        });
        has_prev = true;
    }
    rows.clamp(3, 14)
}

fn is_compact_kv(entries: &[(String, String)]) -> bool {
    !entries.is_empty()
        && entries.len() <= 4
        && entries.iter().all(|(k, v)| k.len() + v.len() <= 32)
}

fn render_display_items(f: &mut Frame<'_>, area: Rect, items: &[DisplayItem]) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let (block_title, start_idx) = match items.first().map(|i| &i.kind) {
        Some(DisplayKind::Heading(title)) => (format!(" {title} "), 1),
        _ => (" Overview ".to_owned(), 0),
    };
    for item in &items[start_idx..] {
        match &item.kind {
            DisplayKind::Heading(title) => {
                lines.push(Line::styled(
                    title.clone(),
                    Style::default()
                        .fg(TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            DisplayKind::Badge(label, variant) => {
                let color = variant_color(variant);
                lines.push(Line::from(vec![
                    Span::styled(" ● ", Style::default().fg(color)),
                    Span::styled(
                        label.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            DisplayKind::Alert {
                title,
                message,
                variant,
            } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                let color = variant_color(variant);
                lines.push(Line::from(vec![
                    Span::styled(" ▎ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{title}: "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(message.clone(), Style::default().fg(TEXT_SECONDARY)),
                ]));
            }
            DisplayKind::KeyValue { title, entries } => {
                if let Some(t) = title {
                    lines.push(Line::styled(format!(" {t}"), Style::default().fg(TEXT_MUTED)));
                }
                if is_compact_kv(entries) {
                    let mut spans = vec![Span::raw(" ")];
                    for (i, (k, v)) in entries.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled("  ·  ", Style::default().fg(TEXT_MUTED)));
                        }
                        spans.push(Span::styled(format!("{k}: "), Style::default().fg(TEXT_MUTED)));
                        spans.push(Span::styled(v.clone(), Style::default().fg(TEXT_PRIMARY)));
                    }
                    lines.push(Line::from(spans));
                } else {
                    let max_k = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
                    for (k, v) in entries {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {:width$}  ", k, width = max_k),
                                Style::default().fg(TEXT_MUTED),
                            ),
                            Span::styled(v.clone(), Style::default().fg(TEXT_PRIMARY)),
                        ]));
                    }
                }
            }
            DisplayKind::Markdown { title, content } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                if let Some(t) = title {
                    lines.push(Line::styled(
                        format!(" ── {t} ──"),
                        Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                    ));
                }
                for ml in parse_md_lines(content) {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(render_md_line_spans(&ml.raw, ml.in_code_fence));
                    lines.push(Line::from(spans));
                }
            }
            DisplayKind::TaskList { title, tasks } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                let done_n = tasks.iter().filter(|t| t.status == "done").count();
                let header = match title {
                    Some(t) => format!(" ── {t} ({done_n}/{}) ──", tasks.len()),
                    None => format!(" ── Plan ({done_n}/{}) ──", tasks.len()),
                };
                lines.push(Line::styled(
                    header,
                    Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                ));
                for task in tasks {
                    let (icon, color, text_style) = match task.status.as_str() {
                        "done" => (" ✓ ", SUCCESS, Style::default().fg(TEXT_MUTED)),
                        "running" => (
                            " ⠋ ",
                            ACCENT,
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        "failed" => (
                            " ✖ ",
                            DANGER,
                            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
                        ),
                        _ => (" ○ ", TEXT_MUTED, Style::default().fg(TEXT_PRIMARY)),
                    };
                    let mut spans = vec![
                        Span::styled(icon, Style::default().fg(color)),
                        Span::styled(task.title.clone(), text_style),
                    ];
                    if let Some(det) = &task.detail
                        && !det.is_empty()
                    {
                        spans.push(Span::styled(
                            format!("  — {det}"),
                            Style::default().fg(TEXT_MUTED),
                        ));
                    }
                    lines.push(Line::from(spans));
                }
            }
        }
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(block_title)
        .border_style(Style::default().fg(BORDER_MUTED));
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block),
        area,
    );
}

fn render_interactive_widgets(f: &mut Frame<'_>, area: Rect, model: &AppModel) {
    if model.widgets.is_empty() {
        return;
    }

    // Give primary review widgets (MarkdownReview / DiffReview) the bulk of the height,
    // and compact rows for buttons/inputs below.
    let mut constraints: Vec<Constraint> = Vec::with_capacity(model.widgets.len());
    for w in &model.widgets {
        constraints.push(match w {
            FocusWidget::MarkdownReview { .. }
            | FocusWidget::DiffReview { .. }
            | FocusWidget::Table { .. }
            | FocusWidget::Conversation { .. } => Constraint::Min(8),
            FocusWidget::Terminal { stdout, stderr, .. } => {
                let line_count = stdout.lines().count() + stderr.lines().count();
                Constraint::Length((line_count as u16).clamp(2, 8) + 2)
            }
            FocusWidget::ToolCall {
                expanded,
                arguments,
                output,
                error,
                ..
            } => {
                if *expanded {
                    let body_lines = arguments.as_deref().map_or(0, |s| s.lines().count().min(4) + 1)
                        + output.as_deref().map_or(0, |s| s.lines().count().min(4) + 1)
                        + error.as_deref().map_or(0, |s| s.lines().count().min(3) + 1);
                    Constraint::Length((body_lines as u16).clamp(2, 10) + 3)
                } else {
                    Constraint::Length(3)
                }
            }
            FocusWidget::Reasoning {
                expanded, content, ..
            } => {
                if *expanded {
                    Constraint::Length((content.lines().count() as u16).clamp(1, 6) + 3)
                } else {
                    Constraint::Length(3)
                }
            }
            FocusWidget::PromptInput { .. } => Constraint::Length(3),
            FocusWidget::FallbackPicker { events, .. } => {
                Constraint::Min((events.len() as u16).saturating_add(3))
            }
            FocusWidget::Select { options, .. } => {
                Constraint::Length((options.len() as u16).min(6) + 2)
            }
            FocusWidget::MultiSelect { options, .. } => {
                Constraint::Length((options.len() as u16).min(6) + 2)
            }
            FocusWidget::TextInput { .. } => Constraint::Length(3),
            FocusWidget::ActionBar { buttons, .. } => {
                Constraint::Length(action_bar_height(buttons, area.width))
            }
        });
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (idx, widget) in model.widgets.iter().enumerate() {
        let is_focused = idx == model.focused;
        let border_style = if is_focused {
            Style::default().fg(BORDER_FOCUS)
        } else {
            Style::default().fg(BORDER_MUTED)
        };
        let rect = chunks[idx];

        match widget {
            FocusWidget::MarkdownReview {
                props,
                selected_line,
            } => {
                render_markdown_review(
                    f,
                    rect,
                    props,
                    *selected_line,
                    &model.draft_state,
                    border_style,
                    model.scroll_offset,
                );
            }
            FocusWidget::DiffReview {
                props,
                selected_line,
            } => {
                render_diff_review(
                    f,
                    rect,
                    props,
                    *selected_line,
                    &model.draft_state,
                    border_style,
                );
            }
            FocusWidget::Table {
                title,
                columns,
                rows,
                selected_row,
            } => {
                render_table_widget(
                    f,
                    rect,
                    title.as_deref(),
                    columns,
                    rows,
                    *selected_row,
                    is_focused,
                    border_style,
                    &model.link_hotspot,
                );
            }
            FocusWidget::Conversation {
                title,
                messages,
                empty_text,
                scroll,
            } => {
                render_conversation_widget(
                    f,
                    rect,
                    title.as_deref(),
                    messages,
                    empty_text,
                    *scroll,
                    border_style,
                );
            }
            FocusWidget::PromptInput {
                label,
                placeholder,
                submit_label,
                interrupt_event,
                interrupt_label,
                buffer,
                ..
            } => {
                render_prompt_input_widget(
                    f,
                    rect,
                    label.as_deref(),
                    placeholder,
                    submit_label,
                    interrupt_event.is_some(),
                    interrupt_label,
                    buffer,
                    is_focused,
                    border_style,
                );
            }
            FocusWidget::ToolCall {
                name,
                status,
                arguments,
                output,
                error,
                expanded,
            } => {
                render_tool_call_widget(
                    f,
                    rect,
                    name,
                    status,
                    arguments.as_deref(),
                    output.as_deref(),
                    error.as_deref(),
                    *expanded,
                    border_style,
                );
            }
            FocusWidget::Reasoning {
                title,
                content,
                duration_label,
                expanded,
            } => {
                render_reasoning_widget(
                    f,
                    rect,
                    title,
                    content,
                    duration_label.as_deref(),
                    *expanded,
                    border_style,
                );
            }
            FocusWidget::Terminal {
                title,
                command,
                stdout,
                stderr,
                exit_code,
                scroll,
            } => {
                render_terminal_widget(
                    f,
                    rect,
                    title.as_deref(),
                    command.as_deref(),
                    stdout,
                    stderr,
                    *exit_code,
                    *scroll,
                    border_style,
                );
            }
            FocusWidget::TextInput { label, bind } => {
                let val = model
                    .draft_state
                    .pointer(bind)
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let p = Paragraph::new(val).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(format!(" {label} "))
                        .border_style(border_style),
                );
                f.render_widget(p, rect);
            }
            FocusWidget::Select {
                label,
                bind,
                options,
                cursor,
            } => {
                let current = model
                    .draft_state
                    .pointer(bind)
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let items: Vec<ListItem<'_>> = options
                    .iter()
                    .enumerate()
                    .map(|(i, opt)| {
                        let mark = if opt == current { "●" } else { "○" };
                        let style = if is_focused && i == *cursor {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        ListItem::new(Line::styled(format!(" {mark} {opt}"), style))
                    })
                    .collect();
                f.render_widget(
                    List::new(items).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .title(format!(" {label} "))
                            .border_style(border_style),
                    ),
                    rect,
                );
            }
            FocusWidget::MultiSelect {
                label,
                bind,
                options,
                cursor,
            } => {
                let selected: Vec<String> = model
                    .draft_state
                    .pointer(bind)
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let items: Vec<ListItem<'_>> = options
                    .iter()
                    .enumerate()
                    .map(|(i, (opt_id, opt_label, desc))| {
                        let mark = if selected.iter().any(|s| s == opt_id) {
                            "[x]"
                        } else {
                            "[ ]"
                        };
                        let style = if is_focused && i == *cursor {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        let text = match desc {
                            Some(d) => format!(" {mark} {opt_label} — {d}"),
                            None => format!(" {mark} {opt_label}"),
                        };
                        ListItem::new(Line::styled(text, style))
                    })
                    .collect();
                f.render_widget(
                    List::new(items).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .title(format!(" {label} "))
                            .border_style(border_style),
                    ),
                    rect,
                );
            }
            FocusWidget::ActionBar { buttons, cursor } => {
                let inner_w = rect.width.saturating_sub(2).max(16) as usize;
                let mut rows_out: Vec<Line<'static>> = Vec::new();
                let mut cur_spans: Vec<Span<'static>> = vec![Span::raw(" ")];
                let mut cur_w: usize = 1;

                for (bi, btn) in buttons.iter().enumerate() {
                    let count_suffix = if btn.show_comment_count
                        && let Some(path) = &btn.comments_path
                    {
                        let n = model
                            .draft_state
                            .pointer(path)
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        if n > 0 {
                            format!(" ({n})")
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    let color = match btn.variant.as_str() {
                        "success" => SUCCESS,
                        "primary" => ACCENT,
                        "danger" => DANGER,
                        "warning" => WARNING,
                        _ => TEXT_SECONDARY,
                    };
                    let text = format!("{}{count_suffix}", btn.label);
                    let chip_text = if is_focused && bi == *cursor {
                        format!("  {text}  ")
                    } else {
                        format!("[ {text} ]")
                    };
                    let chip_w = chip_text.chars().count();
                    let sep_w = if cur_spans.len() > 1 { 2 } else { 0 };
                    if cur_spans.len() > 1 && cur_w + sep_w + chip_w > inner_w {
                        rows_out.push(Line::from(std::mem::take(&mut cur_spans)));
                        cur_spans.push(Span::raw(" "));
                        cur_w = 1;
                    } else if cur_spans.len() > 1 {
                        cur_spans.push(Span::raw("  "));
                        cur_w += 2;
                    }
                    let style = if is_focused && bi == *cursor {
                        Style::default()
                            .fg(Color::Black)
                            .bg(color)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(color)
                    };
                    cur_spans.push(Span::styled(chip_text, style));
                    cur_w += chip_w;
                }
                if cur_spans.len() > 1 {
                    rows_out.push(Line::from(cur_spans));
                }
                f.render_widget(
                    Paragraph::new(rows_out).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .title(" Actions ")
                            .border_style(border_style),
                    ),
                    rect,
                );
            }
            FocusWidget::FallbackPicker { events, cursor } => {
                let items: Vec<ListItem<'_>> = events
                    .iter()
                    .enumerate()
                    .map(|(i, ev)| {
                        let (prefix, style) = if i == *cursor {
                            (
                                "▸ ",
                                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                            )
                        } else {
                            ("  ", Style::default().fg(TEXT_PRIMARY))
                        };
                        ListItem::new(Line::styled(format!("{prefix}{ev}"), style))
                    })
                    .collect();
                f.render_widget(
                    List::new(items).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .title(" Choose Event ")
                            .border_style(border_style),
                    ),
                    rect,
                );
            }
        }
    }
}

fn render_markdown_review(
    f: &mut Frame<'_>,
    area: Rect,
    props: &Map<String, Value>,
    selected_line: usize,
    draft_state: &Value,
    border_style: Style,
    manual_scroll: u16,
) {
    let title = props
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Markdown Review");
    let md = resolve_str(props.get("markdown"), draft_state);
    let notes = resolve_val(props.get("agent_notes"), draft_state);
    let comments_path = props
        .get("comments_path")
        .and_then(Value::as_str)
        .unwrap_or("/draft_comments");
    let comments = draft_state
        .pointer(comments_path)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let md_lines = parse_md_lines(&md);
    let notes_arr = notes.as_array().cloned().unwrap_or_default();
    let active_idx = selected_line.min(md_lines.len().saturating_sub(1));

    // Collect annotations for the currently selected line so we can show them in a
    // clean bottom inspector bar without cluttering the document flow.
    let mut inspector_lines: Vec<Line<'static>> = Vec::new();
    if let Some(sel_ml) = md_lines.get(active_idx) {
        for note_text in notes_for_md_line(&notes_arr, sel_ml) {
            inspector_lines.push(Line::from(vec![
                Span::styled(
                    " ℹ Agent note  ",
                    Style::default()
                        .fg(NOTE_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(note_text, Style::default().fg(TEXT_SECONDARY)),
            ]));
        }
        for c in comments.iter().filter(|c| comment_matches_md_line(c, sel_ml)) {
            let txt = c.get("comment").and_then(Value::as_str).unwrap_or("");
            let prefix = format!(" ◆ Comment L{}  ", sel_ml.line_num);
            let cont_pad = " ".repeat(prefix.chars().count());
            for (li, line_part) in txt.lines().enumerate() {
                if li == 0 {
                    inspector_lines.push(Line::from(vec![
                        Span::styled(
                            prefix.clone(),
                            Style::default()
                                .fg(COMMENT_ACCENT)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(line_part.to_owned(), Style::default().fg(TEXT_PRIMARY)),
                    ]));
                } else {
                    inspector_lines.push(Line::from(vec![
                        Span::raw(cont_pad.clone()),
                        Span::styled(line_part.to_owned(), Style::default().fg(TEXT_PRIMARY)),
                    ]));
                }
            }
        }
    }

    let total_lines = md_lines.len().max(1);
    let cur_line = (active_idx + 1).min(total_lines);
    let header_title = if comments.is_empty() {
        format!(" {title} · L{cur_line}/{total_lines} ")
    } else {
        format!(
            " {title} · L{cur_line}/{total_lines} · {} comment(s) ",
            comments.len()
        )
    };
    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(header_title)
        .border_style(border_style);
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    let inspector_height = if inspector_lines.is_empty() {
        0
    } else {
        (inspector_lines.len() as u16 + 1).min(inner.height / 3).max(2)
    };
    let (doc_area, insp_area) = if inspector_height > 0 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(inspector_height)])
            .split(inner);
        (split[0], Some(split[1]))
    } else {
        (inner, None)
    };

    let mut rendered: Vec<Line<'static>> = Vec::with_capacity(md_lines.len());
    for (idx, ml) in md_lines.iter().enumerate() {
        let is_sel = idx == active_idx;
        let has_comment = comments.iter().any(|c| comment_matches_md_line(c, ml));
        let has_note = !notes_for_md_line(&notes_arr, ml).is_empty();

        let row_bg = if is_sel {
            Style::default().bg(CURSOR_BG)
        } else {
            Style::default()
        };

        let line_no_style = if is_sel {
            row_bg.fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            row_bg.fg(TEXT_MUTED)
        };

        let gutter_icon = match (has_comment, has_note) {
            (true, true) => Span::styled("◆ℹ", row_bg.fg(COMMENT_ACCENT)),
            (true, false) => Span::styled("◆ ", row_bg.fg(COMMENT_ACCENT)),
            (false, true) => Span::styled("ℹ ", row_bg.fg(NOTE_ACCENT)),
            (false, false) => Span::styled("  ", row_bg),
        };

        let sep = if is_sel {
            Span::styled("┃ ", row_bg.fg(ACCENT))
        } else {
            Span::styled("│ ", row_bg.fg(BORDER_MUTED))
        };

        let cursor_mark = if is_sel {
            Span::styled("▸", row_bg.fg(ACCENT).add_modifier(Modifier::BOLD))
        } else {
            Span::styled(" ", row_bg)
        };

        let mut spans = vec![
            cursor_mark,
            Span::styled(format!("{:>3} ", ml.line_num), line_no_style),
            gutter_icon,
            sep,
        ];
        for mut sp in render_md_line_spans(&ml.raw, ml.in_code_fence) {
            if is_sel && sp.style.bg.is_none() {
                sp.style = sp.style.bg(CURSOR_BG);
            }
            spans.push(sp);
        }
        rendered.push(Line::from(spans).style(row_bg));
    }

    let doc_width = doc_area.width.max(1) as usize;
    let mut selected_visual_row: u16 = 0;
    let mut running_rows: u16 = 0;
    for (idx, line) in rendered.iter().enumerate() {
        if idx == active_idx {
            selected_visual_row = running_rows;
        }
        let w = line.width().max(1);
        let rows = w.div_ceil(doc_width) as u16;
        running_rows = running_rows.saturating_add(rows);
    }

    let doc_height = doc_area.height.max(1);
    let auto_scroll = selected_visual_row.saturating_sub(doc_height / 2);
    let scroll = auto_scroll.saturating_add(manual_scroll);

    f.render_widget(
        Paragraph::new(Text::from(rendered))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        doc_area,
    );

    if let Some(ia) = insp_area {
        let sep_rule = "─".repeat(ia.width as usize);
        let mut panel_lines = vec![Line::styled(sep_rule, Style::default().fg(BORDER_MUTED))];
        panel_lines.extend(inspector_lines);
        f.render_widget(
            Paragraph::new(panel_lines).wrap(Wrap { trim: false }),
            ia,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_conversation_widget(
    f: &mut Frame<'_>,
    area: Rect,
    title: Option<&str>,
    messages: &[TuiChatMessage],
    empty_text: &str,
    scroll: usize,
    border_style: Style,
) {
    let header = match title {
        Some(t) => format!(" {t} ({}) ", messages.len()),
        None => format!(" Conversation ({}) ", messages.len()),
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if messages.is_empty() {
        lines.push(Line::styled(
            format!(" {empty_text}"),
            Style::default().fg(TEXT_MUTED),
        ));
    } else {
        for (idx, msg) in messages.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::from(""));
            }
            match msg.role.as_str() {
                "user" => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            " You ",
                            Style::default()
                                .fg(ACCENT)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                "tool" => {
                    let name = msg.tool_name.as_deref().unwrap_or("tool");
                    lines.push(Line::from(vec![
                        Span::styled(" ⚙ ", Style::default().fg(WARNING)),
                        Span::styled(
                            format!("Tool: {name}"),
                            Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                "system" => {
                    lines.push(Line::styled(
                        " System",
                        Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                    ));
                }
                _ => {
                    lines.push(Line::styled(
                        " Assistant",
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            if let Some(reasoning) = &msg.reasoning
                && !reasoning.trim().is_empty()
            {
                let preview = reasoning
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                lines.push(Line::from(vec![
                    Span::styled("   ◆ Thinking: ", Style::default().fg(NOTE_ACCENT)),
                    Span::styled(preview, Style::default().fg(TEXT_MUTED)),
                ]));
            }
            if !msg.text.trim().is_empty() {
                let text_color = if msg.role == "tool" {
                    CODE_FG
                } else {
                    TEXT_PRIMARY
                };
                for l in msg.text.lines().take(12) {
                    lines.push(Line::styled(
                        format!("   {l}"),
                        Style::default().fg(text_color),
                    ));
                }
            }
            for tc in &msg.tool_calls {
                let args_preview = tc
                    .arguments
                    .as_deref()
                    .map(|a| {
                        let compact = a.replace('\n', " ");
                        let clipped: String = compact.chars().take(60).collect();
                        format!("({clipped})")
                    })
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled("   ⚙ ", Style::default().fg(WARNING)),
                    Span::styled(
                        tc.name.clone(),
                        Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(args_preview, Style::default().fg(TEXT_MUTED)),
                    Span::styled(
                        format!(" [{}]", tc.status),
                        Style::default().fg(if tc.status == "error" { DANGER } else { SUCCESS }),
                    ),
                ]));
                if let Some(out_text) = &tc.output
                    && !out_text.trim().is_empty()
                {
                    let compact_out: String = out_text
                        .replace('\n', " ")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(90)
                        .collect();
                    lines.push(Line::styled(
                        format!("     ↳ {compact_out}"),
                        Style::default().fg(CODE_FG),
                    ));
                }
            }
        }
    }

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(inner_h);
    let effective_scroll = if scroll == 0 {
        max_scroll
    } else {
        scroll.min(max_scroll)
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(header)
        .border_style(border_style);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((effective_scroll as u16, 0))
            .block(block),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_prompt_input_widget(
    f: &mut Frame<'_>,
    area: Rect,
    label: Option<&str>,
    placeholder: &str,
    submit_label: &str,
    has_interrupt: bool,
    interrupt_label: &str,
    buffer: &str,
    is_focused: bool,
    border_style: Style,
) {
    let title = match label {
        Some(l) if !l.is_empty() => format!(" {l} "),
        _ => " Prompt ".to_owned(),
    };
    let mut spans = vec![Span::styled(" › ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))];
    if buffer.is_empty() {
        spans.push(Span::styled(
            placeholder.to_owned(),
            Style::default().fg(TEXT_MUTED),
        ));
    } else {
        spans.push(Span::styled(
            buffer.to_owned(),
            Style::default().fg(TEXT_PRIMARY),
        ));
    }
    if is_focused {
        spans.push(Span::styled("█", Style::default().fg(ACCENT)));
    }
    spans.push(Span::styled(
        format!("   [Enter: {submit_label}]"),
        Style::default().fg(TEXT_MUTED),
    ));
    if has_interrupt {
        spans.push(Span::styled(
            format!("  [Esc: {interrupt_label}]"),
            Style::default().fg(DANGER),
        ));
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(border_style);
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

#[allow(clippy::too_many_arguments)]
fn render_tool_call_widget(
    f: &mut Frame<'_>,
    area: Rect,
    name: &str,
    status: &str,
    arguments: Option<&str>,
    output: Option<&str>,
    error: Option<&str>,
    expanded: bool,
    border_style: Style,
) {
    let arrow = if expanded { "▾" } else { "▸" };
    let status_color = match status {
        "running" => ACCENT,
        "error" | "failed" => DANGER,
        "pending" => TEXT_MUTED,
        _ => SUCCESS,
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {arrow} ⚙ "), Style::default().fg(WARNING)),
        Span::styled(
            name.to_owned(),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{status}]"),
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])];

    if expanded {
        if let Some(args) = arguments
            && !args.trim().is_empty()
        {
            lines.push(Line::styled(
                "   Arguments:",
                Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
            ));
            for l in args.lines().take(4) {
                lines.push(Line::styled(
                    format!("     {l}"),
                    Style::default().fg(TEXT_SECONDARY),
                ));
            }
        }
        if let Some(out) = output
            && !out.trim().is_empty()
        {
            lines.push(Line::styled(
                "   Output:",
                Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
            ));
            for l in out.lines().take(4) {
                lines.push(Line::styled(
                    format!("     {l}"),
                    Style::default().fg(CODE_FG),
                ));
            }
        }
        if let Some(err) = error
            && !err.trim().is_empty()
        {
            lines.push(Line::styled(
                "   Error:",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ));
            for l in err.lines().take(3) {
                lines.push(Line::styled(
                    format!("     {l}"),
                    Style::default().fg(DANGER),
                ));
            }
        }
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Tool Call ")
        .border_style(border_style);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block),
        area,
    );
}

fn render_reasoning_widget(
    f: &mut Frame<'_>,
    area: Rect,
    title: &str,
    content: &str,
    duration_label: Option<&str>,
    expanded: bool,
    border_style: Style,
) {
    let arrow = if expanded { "▾" } else { "▸" };
    let mut header_spans = vec![
        Span::styled(format!(" {arrow} ◆ "), Style::default().fg(NOTE_ACCENT)),
        Span::styled(
            title.to_owned(),
            Style::default()
                .fg(NOTE_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(dur) = duration_label {
        header_spans.push(Span::styled(
            format!("  ({dur})"),
            Style::default().fg(TEXT_MUTED),
        ));
    }
    let mut lines = vec![Line::from(header_spans)];
    if expanded {
        for l in content.lines().take(6) {
            lines.push(Line::styled(
                format!("   {l}"),
                Style::default().fg(TEXT_SECONDARY),
            ));
        }
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Reasoning ")
        .border_style(border_style);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_terminal_widget(
    f: &mut Frame<'_>,
    area: Rect,
    title: Option<&str>,
    command: Option<&str>,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i64>,
    scroll: usize,
    border_style: Style,
) {
    let header_cmd = match (command, title) {
        (Some(cmd), _) if !cmd.is_empty() => format!(" $ {cmd} "),
        (_, Some(t)) if !t.is_empty() => format!(" {t} "),
        _ => " Terminal ".to_owned(),
    };
    let title_str = match exit_code {
        Some(code) => format!("{header_cmd}[exit {code}] "),
        None => header_cmd,
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for l in stdout.lines() {
        lines.push(Line::styled(format!(" {l}"), Style::default().fg(CODE_FG)));
    }
    for l in stderr.lines() {
        lines.push(Line::styled(format!(" {l}"), Style::default().fg(DANGER)));
    }
    if lines.is_empty() {
        lines.push(Line::styled(" (no output)", Style::default().fg(TEXT_MUTED)));
    }

    let inner_h = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(inner_h);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title_str)
        .border_style(border_style);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(max_scroll) as u16, 0))
            .block(block),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_table_widget(
    f: &mut Frame<'_>,
    area: Rect,
    title: Option<&str>,
    columns: &[String],
    rows: &[Vec<String>],
    selected_row: usize,
    is_focused: bool,
    border_style: Style,
    link_hotspot: &RefCell<Option<(Rect, String)>>,
) {
    let title_str = match (title, rows.is_empty()) {
        (Some(t), false) => format!(" {t} ({}/{}) ", (selected_row + 1).min(rows.len()), rows.len()),
        (Some(t), true) => format!(" {t} (0) "),
        (None, false) => format!(" Table ({}/{}) ", (selected_row + 1).min(rows.len()), rows.len()),
        (None, true) => " Table (0) ".to_owned(),
    };

    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title_str)
        .border_style(border_style);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let cleaned_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| format_table_cell(c)).collect())
        .collect();

    // Build full-text + URL detail panel at the bottom for the selected row so long posts
    // and clickable post links are always visible without spilling off-screen.
    let sel_urls = rows
        .get(selected_row)
        .map(|r| extract_row_urls(r))
        .unwrap_or_default();
    let detail_text = cleaned_rows
        .get(selected_row)
        .and_then(|r| r.last())
        .filter(|s| s.chars().count() > 48 || !sel_urls.is_empty())
        .cloned();
    let has_detail = (detail_text.is_some() || !sel_urls.is_empty()) && inner.height >= 7;
    let detail_height = if has_detail {
        let w = inner.width.saturating_sub(2).max(20) as usize;
        let text_rows = detail_text
            .as_deref()
            .map_or(0, |s| s.chars().count().div_ceil(w).max(1));
        let url_rows = usize::from(!sel_urls.is_empty());
        ((1 + url_rows + text_rows) as u16)
            .clamp(3, (inner.height / 2).max(3))
    } else {
        0
    };
    let (table_area, detail_area) = if detail_height > 0 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(detail_height)])
            .split(inner);
        (split[0], Some(split[1]))
    } else {
        (inner, None)
    };

    let col_count = columns.len().max(1);
    let mut widths: Vec<Constraint> = Vec::with_capacity(col_count);
    for col_idx in 0..col_count {
        if col_idx + 1 == col_count {
            widths.push(Constraint::Fill(1));
        } else {
            let header_len = columns.get(col_idx).map_or(4, |s| s.chars().count());
            let max_cell = cleaned_rows
                .iter()
                .filter_map(|r| r.get(col_idx))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0);
            let w = header_len.max(max_cell).clamp(4, 28) as u16 + 2;
            widths.push(Constraint::Length(w));
        }
    }

    let header = Row::new(
        columns
            .iter()
            .map(|c| Cell::from(c.as_str()).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
    )
    .bottom_margin(1);

    let body_rows: Vec<Row<'_>> = cleaned_rows
        .iter()
        .enumerate()
        .map(|(i, cells)| {
            let is_sel = i == selected_row;
            let style = if is_sel && is_focused {
                Style::default().fg(TEXT_PRIMARY).bg(CURSOR_BG).add_modifier(Modifier::BOLD)
            } else if is_sel {
                Style::default().fg(TEXT_PRIMARY).bg(CURSOR_BG)
            } else {
                Style::default().fg(TEXT_SECONDARY)
            };
            Row::new(cells.iter().map(|c| Cell::from(c.as_str()))).style(style)
        })
        .collect();

    let mut table_state = TableState::default();
    if !cleaned_rows.is_empty() {
        table_state.select(Some(selected_row.min(cleaned_rows.len() - 1)));
    }
    let table = RatatuiTable::new(body_rows, widths)
        .header(header)
        .column_spacing(2);
    f.render_stateful_widget(table, table_area, &mut table_state);

    if let Some(da) = detail_area {
        let sep = "─".repeat(da.width as usize);
        let mut lines = vec![Line::styled(sep, Style::default().fg(BORDER_MUTED))];
        if let Some(url) = sel_urls.first() {
            let link_rect = Rect::new(da.x, da.y.saturating_add(1), da.width, 1);
            *link_hotspot.borrow_mut() = Some((link_rect, url.clone()));
            lines.push(Line::from(vec![
                Span::styled(" ↗ ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(
                    url.clone(),
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));
        }
        if let Some(detail) = detail_text {
            lines.push(Line::styled(
                format!(" {detail}"),
                Style::default().fg(TEXT_PRIMARY),
            ));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), da);
    }
}

fn render_diff_review(
    f: &mut Frame<'_>,
    area: Rect,
    props: &Map<String, Value>,
    selected_line: usize,
    draft_state: &Value,
    border_style: Style,
) {
    let title = props
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Diff Review");
    let diff_txt = resolve_str(props.get("diff"), draft_state);
    let comments_path = props
        .get("comments_path")
        .and_then(Value::as_str)
        .unwrap_or("/draft_comments");
    let comments = draft_state
        .pointer(comments_path)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let diff_lines = parse_diff_lines(&diff_txt);
    let mut rows = Vec::new();
    for (idx, dl) in diff_lines.iter().enumerate() {
        let fg = match dl.kind {
            '+' => SUCCESS,
            '-' => DANGER,
            '@' => ACCENT,
            _ => TEXT_SECONDARY,
        };
        let mut style = Style::default().fg(fg);
        if idx == selected_line {
            style = style.bg(CURSOR_BG).add_modifier(Modifier::BOLD);
        }
        let has_comment = comments.iter().any(|c| {
            c.get("file").and_then(Value::as_str) == Some(dl.file.as_str())
                && c.get("line").and_then(Value::as_u64) == Some(dl.line_num as u64)
        });
        let gutter = if has_comment {
            format!("{:>4} ◆", dl.line_num)
        } else {
            format!("{:>4}  ", dl.line_num)
        };
        rows.push(Row::new(vec![gutter, dl.text.clone()]).style(style));
    }

    let table = RatatuiTable::new(rows, [Constraint::Length(7), Constraint::Min(10)]).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {title} ({} comment(s)) ", comments.len()))
            .border_style(border_style),
    );
    f.render_widget(table, area);
}

fn render_drawer(f: &mut Frame<'_>, area: Rect, model: &AppModel) {
    match model.drawer {
        DrawerMode::None => {}
        DrawerMode::Trace => {
            let start = model
                .traces
                .len()
                .saturating_sub(area.height.saturating_sub(2) as usize);
            let lines: Vec<Line<'_>> = model.traces[start..]
                .iter()
                .map(|s| Line::styled(s.as_str(), Style::default().fg(TEXT_SECONDARY)))
                .collect();
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" Live Trace [t] ")
                        .border_style(Style::default().fg(BORDER_FOCUS)),
                ),
                area,
            );
        }
        DrawerMode::Diagram => {
            let mut lines = Vec::new();
            render_ir_tree(&model.ir.states, &model.active_states, 0, &mut lines);
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" Statechart [g] ")
                        .border_style(Style::default().fg(BORDER_FOCUS)),
                ),
                area,
            );
        }
        DrawerMode::Context => {
            let pretty = serde_json::to_string_pretty(&model.context).unwrap_or_default();
            f.render_widget(
                Paragraph::new(pretty).wrap(Wrap { trim: false }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" Context [x] ")
                        .border_style(Style::default().fg(BORDER_FOCUS)),
                ),
                area,
            );
        }
    }
}

fn render_ir_tree(
    states: &indexmap::IndexMap<StateId, StateNodeDef>,
    active_states: &[String],
    depth: usize,
    out: &mut Vec<Line<'static>>,
) {
    let pad = "  ".repeat(depth);
    for (sid, node) in states {
        let is_active = active_states
            .iter()
            .any(|a| a == sid.as_str() || a.rsplit('.').next() == Some(sid.as_str()));
        let kind_tag = match &node.kind {
            StateKind::Compound { .. } => "▸",
            StateKind::Parallel { .. } => "∥",
            StateKind::Final => "◉",
            StateKind::History { .. } => "H",
            StateKind::Atomic => "•",
        };
        let style = if is_active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_PRIMARY)
        };
        let marker = if is_active { " ◀ active" } else { "" };
        out.push(Line::styled(
            format!("{pad}{kind_tag} {}{marker}", sid.as_str()),
            style,
        ));
        for tr in &node.on {
            if !tr.forbidden {
                let target = tr
                    .target
                    .as_ref()
                    .map(|t| format!(" -> {}", t.as_str()))
                    .unwrap_or_else(|| " (internal)".to_owned());
                out.push(Line::styled(
                    format!("{pad}    on {}{target}", tr.event),
                    Style::default().fg(TEXT_MUTED),
                ));
            }
        }
        match &node.kind {
            StateKind::Compound { states: kids, .. } | StateKind::Parallel { states: kids } => {
                render_ir_tree(kids, active_states, depth + 1, out);
            }
            StateKind::Atomic | StateKind::Final | StateKind::History { .. } => {}
        }
    }
}

fn render_footer(f: &mut Frame<'_>, area: Rect, model: &AppModel) {
    let mut lines = Vec::new();
    if let Some(flash) = &model.status_flash {
        lines.push(Line::styled(
            format!(" {flash}"),
            Style::default().fg(COMMENT_ACCENT),
        ));
    } else {
        lines.push(Line::from(""));
    }
    let pairs = model.footer_hint_pairs();
    let mut hint_spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            hint_spans.push(Span::styled("  ·  ", Style::default().fg(TEXT_MUTED)));
        }
        hint_spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        hint_spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(TEXT_SECONDARY),
        ));
    }
    lines.push(Line::from(hint_spans));
    f.render_widget(Paragraph::new(lines), area);
}

fn render_modal(f: &mut Frame<'_>, area: Rect, modal: &InputModal, focus: ModalFocus) {
    let popup = centered_rect(72, 38, area);
    f.render_widget(Clear, popup);
    let (title, quote_opt, buf) = match modal {
        InputModal::MarkdownComment {
            line,
            heading,
            quote,
            edit_index,
            buffer,
            ..
        } => (
            format!(
                " {} on L{line} ({heading}) ",
                if edit_index.is_some() {
                    "Edit comment"
                } else {
                    "Comment"
                }
            ),
            if quote.is_empty() {
                None
            } else {
                Some(quote.as_str())
            },
            buffer.as_str(),
        ),
        InputModal::DiffComment {
            file,
            line,
            quote,
            edit_index,
            buffer,
            ..
        } => (
            format!(
                " {} on {file}:{line} ",
                if edit_index.is_some() {
                    "Edit comment"
                } else {
                    "Comment"
                }
            ),
            if quote.is_empty() {
                None
            } else {
                Some(quote.as_str())
            },
            buffer.as_str(),
        ),
        InputModal::TextField { label, buffer, .. } => {
            (format!(" Edit {label} "), None, buffer.as_str())
        }
        InputModal::FallbackPayload { event, buffer } => (
            format!(" JSON payload for {event} "),
            None,
            buffer.as_str(),
        ),
    };

    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(BORDER_FOCUS));
    let inner = outer.inner(popup);
    f.render_widget(outer, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);

    let mut editor_lines: Vec<Line<'static>> = Vec::new();
    if let Some(q) = quote_opt {
        editor_lines.push(Line::styled(
            format!(" > {q}"),
            Style::default().fg(TEXT_MUTED),
        ));
        editor_lines.push(Line::from(""));
    }
    let cursor_str = if focus == ModalFocus::Input { "█" } else { "" };
    let display_buf = format!("{buf}{cursor_str}");
    for line_str in display_buf.split('\n') {
        editor_lines.push(Line::styled(
            format!(" {line_str}"),
            Style::default().fg(TEXT_PRIMARY),
        ));
    }
    f.render_widget(
        Paragraph::new(editor_lines).wrap(Wrap { trim: false }),
        chunks[0],
    );

    let save_style = if focus == ModalFocus::Save {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(ACCENT)
    };
    let cancel_style = if focus == ModalFocus::Cancel {
        Style::default()
            .fg(Color::Black)
            .bg(DANGER)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_SECONDARY)
    };
    let sep_rule = "─".repeat(chunks[1].width as usize);
    let btn_lines = vec![
        Line::styled(sep_rule, Style::default().fg(BORDER_MUTED)),
        Line::from(vec![
            Span::raw(" "),
            Span::styled("  Save  ", save_style),
            Span::raw("   "),
            Span::styled("  Cancel  ", cancel_style),
            Span::styled(
                "    (Tab to switch · Ctrl+S to save)",
                Style::default().fg(TEXT_MUTED),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(btn_lines), chunks[1]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn collect_spec_nodes(
    elem_id: &str,
    elements: &Map<String, Value>,
    draft_state: &Value,
    display: &mut Vec<DisplayItem>,
    widgets: &mut Vec<FocusWidget>,
    action_buttons: &mut Vec<ActionBarItem>,
) {
    let Some(elem) = elements.get(elem_id) else {
        return;
    };
    let kind = elem.get("type").and_then(Value::as_str).unwrap_or("");
    let props = elem
        .get("props")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    match kind {
        "Card" => {
            if let Some(title) = props.get("title").and_then(Value::as_str)
                && !title.is_empty()
            {
                display.push(DisplayItem {
                    kind: DisplayKind::Heading(title.to_owned()),
                });
            }
        }
        "Badge" => {
            display.push(DisplayItem {
                kind: DisplayKind::Badge(
                    resolve_str(props.get("label"), draft_state),
                    props
                        .get("variant")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_owned(),
                ),
            });
        }
        "Alert" => {
            display.push(DisplayItem {
                kind: DisplayKind::Alert {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Alert")
                        .to_owned(),
                    message: resolve_str(props.get("message"), draft_state),
                    variant: props
                        .get("variant")
                        .and_then(Value::as_str)
                        .unwrap_or("info")
                        .to_owned(),
                },
            });
        }
        "KeyValue" => {
            let items = resolve_val(props.get("items"), draft_state);
            let mut entries = Vec::new();
            if let Some(map) = items.as_object() {
                for (k, v) in map {
                    entries.push((
                        k.clone(),
                        v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()),
                    ));
                }
            }
            display.push(DisplayItem {
                kind: DisplayKind::KeyValue {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    entries,
                },
            });
        }
        "Table" => {
            let columns = props
                .get("columns")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let raw_rows = resolve_val(props.get("rows"), draft_state);
            let rows = raw_rows
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r.as_array())
                        .map(|cells| {
                            cells
                                .iter()
                                .map(|c| {
                                    c.as_str().map(str::to_owned).unwrap_or_else(|| c.to_string())
                                })
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            widgets.push(FocusWidget::Table {
                title: props
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                columns,
                rows,
                selected_row: 0,
            });
        }
        "Markdown" => {
            display.push(DisplayItem {
                kind: DisplayKind::Markdown {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    content: resolve_str(props.get("content"), draft_state),
                },
            });
        }
        "TaskList" => {
            let raw_tasks = resolve_val(props.get("tasks"), draft_state);
            let tasks = parse_tui_tasks(&raw_tasks);
            display.push(DisplayItem {
                kind: DisplayKind::TaskList {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    tasks,
                },
            });
        }
        "Conversation" => {
            let raw_msgs = resolve_val(props.get("messages"), draft_state);
            let messages = parse_tui_messages(&raw_msgs);
            widgets.push(FocusWidget::Conversation {
                title: props
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                messages,
                empty_text: props
                    .get("empty_text")
                    .and_then(Value::as_str)
                    .unwrap_or("No messages yet. Type a prompt below to begin.")
                    .to_owned(),
                scroll: 0,
            });
        }
        "PromptInput" => {
            if let Some(submit_event) = props.get("submit_event").and_then(Value::as_str) {
                widgets.push(FocusWidget::PromptInput {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    placeholder: props
                        .get("placeholder")
                        .and_then(Value::as_str)
                        .unwrap_or("Type a prompt…")
                        .to_owned(),
                    submit_event: submit_event.to_owned(),
                    submit_label: props
                        .get("submit_label")
                        .and_then(Value::as_str)
                        .unwrap_or("Send")
                        .to_owned(),
                    field_name: props
                        .get("field_name")
                        .and_then(Value::as_str)
                        .unwrap_or("text")
                        .to_owned(),
                    interrupt_event: props
                        .get("interrupt_event")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    interrupt_label: props
                        .get("interrupt_label")
                        .and_then(Value::as_str)
                        .unwrap_or("Stop")
                        .to_owned(),
                    buffer: String::new(),
                });
            }
        }
        "ToolCall" => {
            let name = resolve_str(props.get("name"), draft_state);
            let error_val = props.get("error").map(|v| resolve_val(Some(v), draft_state));
            let error = error_val.and_then(|v| {
                if v.is_null() {
                    None
                } else {
                    Some(format_json_compact(&v))
                }
            });
            let status_raw = props
                .get("status")
                .map(|v| resolve_str(Some(v), draft_state))
                .unwrap_or_else(|| if error.is_some() { "error".to_owned() } else { "ok".to_owned() });
            let arguments = props
                .get("arguments")
                .map(|v| resolve_val(Some(v), draft_state))
                .and_then(|v| if v.is_null() { None } else { Some(format_json_compact(&v)) });
            let output = props
                .get("output")
                .map(|v| resolve_val(Some(v), draft_state))
                .and_then(|v| if v.is_null() { None } else { Some(format_json_compact(&v)) });
            let expanded = props
                .get("expanded")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| error.is_some());
            widgets.push(FocusWidget::ToolCall {
                name: if name.is_empty() { "tool".to_owned() } else { name },
                status: status_raw,
                arguments,
                output,
                error,
                expanded,
            });
        }
        "Reasoning" => {
            let title = props
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Reasoning")
                .to_owned();
            let content_val = resolve_val(props.get("content"), draft_state);
            let content = if let Some(arr) = content_val.as_array() {
                arr.iter()
                    .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                format_json_compact(&content_val)
            };
            let duration_label = props
                .get("duration_ms")
                .map(|v| resolve_val(Some(v), draft_state))
                .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .map(|ms| {
                    if ms >= 1000.0 {
                        format!("{:.1}s", ms / 1000.0)
                    } else {
                        format!("{}ms", ms.round() as i64)
                    }
                });
            let expanded = props
                .get("expanded")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            widgets.push(FocusWidget::Reasoning {
                title,
                content,
                duration_label,
                expanded,
            });
        }
        "Terminal" => {
            let title = props
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let command = props
                .get("command")
                .map(|v| resolve_str(Some(v), draft_state))
                .filter(|s| !s.is_empty());
            let stdout = props
                .get("stdout")
                .map(|v| resolve_str(Some(v), draft_state))
                .unwrap_or_default();
            let stderr = props
                .get("stderr")
                .map(|v| resolve_str(Some(v), draft_state))
                .unwrap_or_default();
            let exit_code = props
                .get("exit_code")
                .map(|v| resolve_val(Some(v), draft_state))
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
            widgets.push(FocusWidget::Terminal {
                title,
                command,
                stdout,
                stderr,
                exit_code,
                scroll: 0,
            });
        }
        "TextInput" | "Textarea" => {
            if let Some(bind) = props.get("bind").and_then(Value::as_str) {
                widgets.push(FocusWidget::TextInput {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("Input")
                        .to_owned(),
                    bind: bind.to_owned(),
                });
            }
        }
        "Select" => {
            if let Some(bind) = props.get("bind").and_then(Value::as_str) {
                let options = props
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                widgets.push(FocusWidget::Select {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("Select")
                        .to_owned(),
                    bind: bind.to_owned(),
                    options,
                    cursor: 0,
                });
            }
        }
        "MultiSelect" => {
            if let Some(bind) = props.get("bind").and_then(Value::as_str) {
                let options = props
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| {
                                let o = v.as_object()?;
                                let id = o.get("id")?.as_str()?.to_owned();
                                let lbl = o
                                    .get("label")
                                    .and_then(Value::as_str)
                                    .unwrap_or(&id)
                                    .to_owned();
                                let desc = o
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                Some((id, lbl, desc))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                widgets.push(FocusWidget::MultiSelect {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("Checklist")
                        .to_owned(),
                    bind: bind.to_owned(),
                    options,
                    cursor: 0,
                });
            }
        }
        "Button" => {
            if let Some(ev) = props.get("event").and_then(Value::as_str) {
                action_buttons.push(ActionBarItem {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(ev)
                        .to_owned(),
                    event: ev.to_owned(),
                    variant: props
                        .get("variant")
                        .and_then(Value::as_str)
                        .unwrap_or("default")
                        .to_owned(),
                    payload: props
                        .get("payload")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default(),
                    comments_path: None,
                    show_comment_count: false,
                });
            }
        }
        "MarkdownReview" => {
            let comments_path = props
                .get("comments_path")
                .and_then(Value::as_str)
                .unwrap_or("/draft_comments")
                .to_owned();
            let approve_event = props
                .get("approve_event")
                .and_then(Value::as_str)
                .unwrap_or("APPROVE")
                .to_owned();
            let revise_event = props
                .get("revise_event")
                .and_then(Value::as_str)
                .unwrap_or("REQUEST_CHANGES")
                .to_owned();
            action_buttons.push(ActionBarItem {
                label: "✓ Approve".to_owned(),
                event: approve_event,
                variant: "success".to_owned(),
                payload: Map::new(),
                comments_path: Some(comments_path.clone()),
                show_comment_count: false,
            });
            action_buttons.push(ActionBarItem {
                label: "↻ Request Changes".to_owned(),
                event: revise_event,
                variant: "primary".to_owned(),
                payload: Map::new(),
                comments_path: Some(comments_path),
                show_comment_count: true,
            });
            widgets.push(FocusWidget::MarkdownReview {
                props,
                selected_line: 0,
            });
        }
        "DiffReview" => {
            let comments_path = props
                .get("comments_path")
                .and_then(Value::as_str)
                .unwrap_or("/draft_comments")
                .to_owned();
            let approve_event = props
                .get("approve_event")
                .and_then(Value::as_str)
                .unwrap_or("APPROVE")
                .to_owned();
            let revise_event = props
                .get("revise_event")
                .and_then(Value::as_str)
                .unwrap_or("REQUEST_CHANGES")
                .to_owned();
            action_buttons.push(ActionBarItem {
                label: "✓ Approve".to_owned(),
                event: approve_event,
                variant: "success".to_owned(),
                payload: Map::new(),
                comments_path: Some(comments_path.clone()),
                show_comment_count: false,
            });
            action_buttons.push(ActionBarItem {
                label: "↻ Request Changes".to_owned(),
                event: revise_event,
                variant: "primary".to_owned(),
                payload: Map::new(),
                comments_path: Some(comments_path),
                show_comment_count: true,
            });
            widgets.push(FocusWidget::DiffReview {
                props,
                selected_line: 0,
            });
        }
        _ => {}
    }

    if let Some(children) = elem.get("children").and_then(Value::as_array) {
        for child in children {
            if let Some(cid) = child.as_str() {
                collect_spec_nodes(
                    cid,
                    elements,
                    draft_state,
                    display,
                    widgets,
                    action_buttons,
                );
            }
        }
    }
}

fn variant_color(variant: &str) -> Color {
    match variant {
        "success" => SUCCESS,
        "warning" => WARNING,
        "danger" => DANGER,
        "info" => ACCENT,
        _ => TEXT_SECONDARY,
    }
}

fn parse_md_lines(md: &str) -> Vec<MdLine> {
    let mut out = Vec::new();
    let mut current_heading = "Overview".to_owned();
    let mut in_fence = false;
    for (i, line) in md.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(MdLine {
                line_num: i + 1,
                heading: current_heading.clone(),
                is_heading_line: false,
                raw: line.to_owned(),
                in_code_fence: true,
            });
            continue;
        }
        let heading_opt = if !in_fence {
            trimmed
                .strip_prefix("#### ")
                .or_else(|| trimmed.strip_prefix("### "))
                .or_else(|| trimmed.strip_prefix("## "))
                .or_else(|| trimmed.strip_prefix("# "))
                .map(|s| s.trim().to_owned())
        } else {
            None
        };
        let is_heading_line = heading_opt.is_some();
        if let Some(h) = heading_opt {
            current_heading = h;
        }
        out.push(MdLine {
            line_num: i + 1,
            heading: current_heading.clone(),
            is_heading_line,
            raw: line.to_owned(),
            in_code_fence: in_fence,
        });
    }
    out
}

fn comment_matches_md_line(comment: &Value, ml: &MdLine) -> bool {
    if let Some(line) = comment.get("line").and_then(Value::as_u64) {
        return line == ml.line_num as u64;
    }
    ml.is_heading_line
        && comment.get("heading").and_then(Value::as_str) == Some(ml.heading.as_str())
}

fn notes_for_md_line(notes: &[Value], ml: &MdLine) -> Vec<String> {
    let mut out = Vec::new();
    for note in notes {
        let matches = if let Some(ln) = note.get("line").and_then(Value::as_u64) {
            ln == ml.line_num as u64
        } else {
            let nh = note.get("heading").and_then(Value::as_str);
            match nh {
                None | Some("*") => ml.line_num == 1,
                Some(h) => ml.is_heading_line && h.eq_ignore_ascii_case(&ml.heading),
            }
        };
        if matches {
            let text = note
                .get("note")
                .or_else(|| note.get("text"))
                .and_then(Value::as_str)
                .or_else(|| note.as_str())
                .unwrap_or("");
            if !text.is_empty() {
                out.push(text.to_owned());
            }
        }
    }
    out
}

/// Renders one Markdown line with visible headings, bullets, bold, italic, and inline code pills
/// while preserving 1-to-1 line correspondence for per-line gutter commenting.
fn render_md_line_spans(raw: &str, in_code_fence: bool) -> Vec<Span<'static>> {
    if in_code_fence {
        return vec![Span::styled(
            raw.to_owned(),
            Style::default().fg(CODE_FG).bg(CODE_BG),
        )];
    }
    let trimmed = raw.trim_start();
    let indent_len = raw.len().saturating_sub(trimmed.len());
    let indent = &raw[..indent_len];

    if let Some(h) = trimmed.strip_prefix("# ") {
        return vec![Span::styled(
            h.to_owned(),
            Style::default()
                .fg(TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )];
    }
    if let Some(h) = trimmed.strip_prefix("## ") {
        return vec![Span::styled(
            h.to_owned(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(h) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("#### "))
    {
        return vec![Span::styled(
            h.to_owned(),
            Style::default()
                .fg(TEXT_SECONDARY)
                .add_modifier(Modifier::BOLD),
        )];
    }

    let mut spans = Vec::new();
    let rest = if let Some(item) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        spans.push(Span::styled(
            format!("{indent}• "),
            Style::default().fg(TEXT_MUTED),
        ));
        item
    } else {
        if !indent.is_empty() {
            spans.push(Span::raw(indent.to_owned()));
        }
        trimmed
    };

    spans.extend(render_md_inline_spans(rest));
    spans
}

fn render_md_inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(buf),
                Style::default().fg(TEXT_PRIMARY),
            ));
        }
    };

    while i < chars.len() {
        // Inline `code`
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            flush(&mut buf, &mut spans);
            let code: String = chars[i + 1..i + 1 + end].iter().collect();
            spans.push(Span::styled(
                format!(" {code} "),
                Style::default().fg(CODE_FG).bg(CODE_BG),
            ));
            i = i + 2 + end;
            continue;
        }
        // **bold**
        if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
            let mut found = None;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '*' && chars[j + 1] == '*' {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                flush(&mut buf, &mut spans);
                let bold: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(
                    bold,
                    Style::default()
                        .fg(TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn parse_diff_lines(raw: &str) -> Vec<DiffLineItem> {
    let mut out = Vec::new();
    let mut file = "patch".to_owned();
    let mut line_num: usize = 0;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let trimmed = rest.trim().trim_start_matches("b/");
            if !trimmed.is_empty() && trimmed != "/dev/null" {
                file = trimmed.to_owned();
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff --git ") || line.starts_with("index ") {
            continue;
        }
        if line.starts_with("@@") {
            if let Some(plus_part) = line.split('+').nth(1) {
                let digits: String = plus_part
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                line_num = digits.parse().unwrap_or(1);
            }
            out.push(DiffLineItem {
                file: file.clone(),
                line_num,
                kind: '@',
                text: line.to_owned(),
            });
            continue;
        }
        let kind = if line.starts_with('+') {
            '+'
        } else if line.starts_with('-') {
            '-'
        } else {
            ' '
        };
        out.push(DiffLineItem {
            file: file.clone(),
            line_num,
            kind,
            text: line.to_owned(),
        });
        if kind != '-' && line_num > 0 {
            line_num += 1;
        }
    }
    out
}

fn resolve_val(prop: Option<&Value>, state: &Value) -> Value {
    let Some(v) = prop else {
        return Value::Null;
    };
    if let Some(ptr) = v.get("$state").and_then(Value::as_str) {
        return state.pointer(ptr).cloned().unwrap_or(Value::Null);
    }
    v.clone()
}

fn resolve_str(prop: Option<&Value>, state: &Value) -> String {
    match resolve_val(prop, state) {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn format_json_compact(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn parse_tui_tasks(raw: &Value) -> Vec<TuiTaskItem> {
    let Some(arr) = raw.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(s) = item.as_str() {
            if !s.is_empty() {
                out.push(TuiTaskItem {
                    title: s.to_owned(),
                    status: "pending".to_owned(),
                    detail: None,
                });
            }
            continue;
        }
        let Some(obj) = item.as_object() else {
            continue;
        };
        let title = obj
            .get("title")
            .or_else(|| obj.get("label"))
            .or_else(|| obj.get("text"))
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if title.is_empty() {
            continue;
        }
        let status_raw = obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                if obj.get("done").and_then(Value::as_bool).unwrap_or(false) {
                    "done"
                } else {
                    "pending"
                }
            })
            .to_ascii_lowercase();
        let status = match status_raw.as_str() {
            "done" | "completed" | "ok" | "passed" => "done",
            "running" | "in_progress" | "active" => "running",
            "failed" | "error" => "failed",
            _ => "pending",
        }
        .to_owned();
        let detail = obj
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.push(TuiTaskItem {
            title,
            status,
            detail,
        });
    }
    out
}

fn attach_tui_tool_result(
    msgs: &mut [TuiChatMessage],
    call_id: Option<&str>,
    tool_name: &str,
    content: &Value,
) -> bool {
    let is_err = content.as_object().is_some_and(|o| o.contains_key("error"));
    let formatted = format_json_compact(content);
    for msg in msgs.iter_mut().rev() {
        if msg.role != "assistant" {
            continue;
        }
        for tc in &mut msg.tool_calls {
            let id_match = call_id.is_some() && tc.id.as_deref() == call_id;
            let name_match = call_id.is_none() && tc.name == tool_name && tc.output.is_none();
            if id_match || name_match {
                tc.output = Some(formatted);
                tc.status = if is_err { "error" } else { "ok" }.to_owned();
                return true;
            }
        }
    }
    false
}

fn parse_tui_messages(raw: &Value) -> Vec<TuiChatMessage> {
    let Some(arr) = raw.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        // Unwrap @starness/llm lossless `$starness_llm_msg` envelope
        if let Some(inner) = obj.get("$starness_llm_msg").and_then(Value::as_object) {
            let role = inner
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant")
                .to_ascii_lowercase();
            let mut text_parts = Vec::new();
            let mut reasoning_parts = Vec::new();
            let mut tool_calls = Vec::new();
            if let Some(content) = inner.get("content") {
                if let Some(s) = content.as_str() {
                    if !s.is_empty() {
                        text_parts.push(s.to_owned());
                    }
                } else if let Some(parts) = content.as_array() {
                    for part in parts {
                        if let Some(s) = part.as_str() {
                            text_parts.push(s.to_owned());
                            continue;
                        }
                        let Some(po) = part.as_object() else {
                            continue;
                        };
                        if let Some(t) = po.get("Text").and_then(Value::as_str) {
                            text_parts.push(t.to_owned());
                        } else if let Some(r) = po
                            .get("Thought")
                            .or_else(|| po.get("Reasoning"))
                            .or_else(|| po.get("ReasoningContent"))
                            .and_then(Value::as_str)
                        {
                            reasoning_parts.push(r.to_owned());
                        } else if let Some(tc) = po.get("ToolCall").and_then(Value::as_object) {
                            tool_calls.push(TuiToolCall {
                                id: tc
                                    .get("call_id")
                                    .or_else(|| tc.get("id"))
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                name: tc
                                    .get("fn_name")
                                    .or_else(|| tc.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_owned(),
                                status: "ok".to_owned(),
                                arguments: tc
                                    .get("fn_arguments")
                                    .or_else(|| tc.get("arguments"))
                                    .or_else(|| tc.get("args"))
                                    .map(format_json_compact),
                                output: None,
                            });
                        }
                    }
                }
            }
            out.push(TuiChatMessage {
                role,
                text: text_parts.join("\n"),
                reasoning: if reasoning_parts.is_empty() {
                    None
                } else {
                    Some(reasoning_parts.join("\n"))
                },
                tool_calls,
                tool_name: None,
            });
            continue;
        }

        let role = obj
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_ascii_lowercase();

        if role == "tool"
            && let Some(results) = obj.get("results").and_then(Value::as_array)
        {
            for res in results {
                let Some(ro) = res.as_object() else {
                    continue;
                };
                let cid = ro.get("call_id").and_then(Value::as_str);
                let tname = ro
                    .get("fn_name")
                    .or_else(|| ro.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let content = ro.get("content").unwrap_or(&Value::Null);
                if !attach_tui_tool_result(&mut out, cid, tname, content) {
                    out.push(TuiChatMessage {
                        role: "tool".to_owned(),
                        text: format_json_compact(content),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        tool_name: Some(tname.to_owned()),
                    });
                }
            }
            continue;
        }

        if role == "tool" {
            let cid = obj.get("call_id").and_then(Value::as_str);
            let tname = obj
                .get("fn_name")
                .or_else(|| obj.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let content = obj
                .get("content")
                .or_else(|| obj.get("text"))
                .unwrap_or(&Value::Null);
            if !attach_tui_tool_result(&mut out, cid, tname, content) {
                out.push(TuiChatMessage {
                    role: "tool".to_owned(),
                    text: format_json_compact(content),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_name: Some(tname.to_owned()),
                });
            }
            continue;
        }

        let text = obj
            .get("text")
            .or_else(|| obj.get("content"))
            .map(format_json_compact)
            .unwrap_or_default();
        let reasoning = obj
            .get("reasoning")
            .or_else(|| obj.get("thought"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut tool_calls = Vec::new();
        if let Some(raw_tcs) = obj.get("tool_calls").and_then(Value::as_array) {
            for raw_tc in raw_tcs {
                let Some(tc) = raw_tc.as_object() else {
                    continue;
                };
                tool_calls.push(TuiToolCall {
                    id: tc
                        .get("id")
                        .or_else(|| tc.get("call_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: tc
                        .get("name")
                        .or_else(|| tc.get("fn_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                    status: tc
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("ok")
                        .to_owned(),
                    arguments: tc
                        .get("arguments")
                        .or_else(|| tc.get("args"))
                        .or_else(|| tc.get("fn_arguments"))
                        .map(format_json_compact),
                    output: tc
                        .get("output")
                        .or_else(|| tc.get("result"))
                        .map(format_json_compact),
                });
            }
        }
        out.push(TuiChatMessage {
            role,
            text,
            reasoning,
            tool_calls,
            tool_name: None,
        });
    }
    out
}

fn set_json_pointer(root: &mut Value, pointer: &str, new_val: Value) {
    let Some(stripped) = pointer.strip_prefix('/') else {
        return;
    };
    let segments: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return;
    }
    let mut cursor = root;
    for seg in &segments[..segments.len() - 1] {
        if !cursor.is_object() {
            *cursor = json!({});
        }
        cursor = cursor
            .as_object_mut()
            .unwrap()
            .entry((*seg).to_owned())
            .or_insert_with(|| json!({}));
    }
    if !cursor.is_object() {
        *cursor = json!({});
    }
    cursor
        .as_object_mut()
        .unwrap()
        .insert(segments[segments.len() - 1].to_owned(), new_val);
}

fn diff_draft_payload(
    base: &Map<String, Value>,
    original_ctx: &Value,
    draft_state: &Value,
) -> Value {
    let mut out = base.clone();
    if let Some(draft_obj) = draft_state.as_object() {
        let orig_obj = original_ctx.as_object();
        for (k, v) in draft_obj {
            if orig_obj.and_then(|o| o.get(k)) != Some(v) && !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Object(out)
    }
}

fn active_states_have_invoke(ir: &MachineDef, active_paths: &[String]) -> bool {
    for raw_path in active_paths {
        let trimmed = raw_path
            .strip_prefix(&format!("{}.", ir.id))
            .unwrap_or(raw_path.as_str());
        let mut current_map = &ir.states;
        for seg in trimmed.split('.') {
            let Some(node) = current_map.get(&StateId::from(seg)) else {
                break;
            };
            if !node.invoke.is_empty() {
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

/// Cleans a table cell for terminal display: replaces Markdown links `[label](https://...)` with
/// `label` and collapses embedded newlines into single spaces so every row fits cleanly.
fn format_table_cell(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open_b) = rest.find('[') {
        out.push_str(&rest[..open_b]);
        let after_open = &rest[open_b + 1..];
        if let Some(close_b) = after_open.find("](http") {
            let label = &after_open[..close_b];
            let after_mid = &after_open[close_b + 2..];
            if let Some(close_p) = after_mid.find(')') {
                out.push_str(label);
                rest = &after_mid[close_p + 1..];
                continue;
            }
        }
        out.push('[');
        rest = after_open;
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extracts `http://` / `https://` URLs from a raw table row (both Markdown `[label](url)` links
/// and bare URLs), deduplicated in order of appearance.
fn extract_row_urls(row: &[String]) -> Vec<String> {
    let mut urls = Vec::new();
    for cell in row {
        let mut rest = cell.as_str();
        while let Some(pos) = rest.find("http://").or_else(|| rest.find("https://")) {
            let candidate = &rest[pos..];
            let end = candidate
                .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\''))
                .unwrap_or(candidate.len());
            let url = candidate[..end]
                .trim_end_matches(['.', ',', ';', ':', '!', '?'])
                .to_owned();
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
            rest = &candidate[end..];
        }
    }
    urls
}

fn open_url_in_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use starness_starlark::compile_starlark_str;

    use super::*;

    #[test]
    fn ratatui_renders_markdown_review_modal_and_dispatches_anchored_comment() {
        let src = r##"
Ctx = record(
    plan = field(str, "# Goal\nShip Ratatui\n\n## Steps\n- Build widgets"),
    notes = field(list[dict], [{"heading": "Steps", "note": "Verify TestBackend"}]),
    draft_comments = field(list[dict], []),
)
S = state_space("reviewing", "done")

chart = machine(
    id = "ratatui_test",
    initial = S.reviewing,
    context = Ctx(),
    states = {
        S.reviewing: state(
            on = [
                transition(event = "APPROVE", target = S.done),
                internal_transition(event = "REQUEST_CHANGES"),
            ],
        ),
        S.done: final_state(),
    },
)
"##;
        let ui_sheet = UiViewSheet::from_json_str(
            r#"{
                "machine": "ratatui_test",
                "views": {
                    "reviewing": {
                        "root": "root",
                        "elements": {
                            "root": { "type": "Stack", "props": {}, "children": ["alert", "review"] },
                            "alert": { "type": "Alert", "props": { "title": "Notice", "message": "Check sections", "variant": "info" } },
                            "review": {
                                "type": "MarkdownReview",
                                "props": {
                                    "title": "Plan Review",
                                    "markdown": {"$state": "/plan"},
                                    "agent_notes": {"$state": "/notes"},
                                    "comments_path": "/draft_comments",
                                    "approve_event": "APPROVE",
                                    "revise_event": "REQUEST_CHANGES"
                                }
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let compiled = compile_starlark_str("ratatui_test.star", src, None).unwrap();
        assert_eq!(ui_sheet.validate_against(&compiled.ir), Ok(()));
        let mut model = AppModel::new(
            compiled.ir.clone(),
            Some(ui_sheet),
            vec!["reviewing".to_owned()],
            compiled.ir.context.clone(),
        );

        // Render initial frame to TestBackend and verify header + gutter icons appear
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_app(f, &model)).unwrap();
        let buf_str: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_str.contains("ratatui_test"));
        assert!(buf_str.contains("Plan Review"));
        assert!(buf_str.contains("ℹ")); // gutter icon on line 4 ("## Steps")
        // Agent note is NOT shown inline until we move cursor onto line 4 ("## Steps")
        assert!(!buf_str.contains("Verify TestBackend"));

        // Move down 3 lines: L1(# Goal) -> L2(Ship Ratatui) -> L3(blank) -> L4(## Steps)
        model.handle_key(KeyEvent::from(KeyCode::Down));
        model.handle_key(KeyEvent::from(KeyCode::Down));
        model.handle_key(KeyEvent::from(KeyCode::Down));
        term.draw(|f| render_app(f, &model)).unwrap();
        let buf_str2: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_str2.contains("Verify TestBackend")); // shown in bottom inspector!

        // Move down to L5 ("- Build widgets"), open comment modal, write a 2-line comment, Tab -> Save -> Enter
        model.handle_key(KeyEvent::from(KeyCode::Down));
        model.handle_key(KeyEvent::from(KeyCode::Char('c')));
        for ch in "Add tests".chars() {
            model.handle_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        model.handle_key(KeyEvent::from(KeyCode::Enter)); // newline!
        for ch in "Include edge cases".chars() {
            model.handle_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        model.handle_key(KeyEvent::from(KeyCode::Tab)); // focus Save
        model.handle_key(KeyEvent::from(KeyCode::Enter)); // click Save

        // Pressing 'c' again on L5 edits the existing comment in place; Ctrl+S saves
        model.handle_key(KeyEvent::from(KeyCode::Char('c')));
        for ch in "!".chars() {
            model.handle_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        model.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        // Tab to ActionBar, Right to "Request Changes (1)", Enter to dispatch
        model.handle_key(KeyEvent::from(KeyCode::Tab));
        model.handle_key(KeyEvent::from(KeyCode::Right));
        let action = model.handle_key(KeyEvent::from(KeyCode::Enter));

        match action {
            AppAction::Dispatch {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "REQUEST_CHANGES");
                assert_eq!(payload["comments"].as_array().unwrap().len(), 1);
                assert_eq!(payload["comments"][0]["line"], 5);
                assert_eq!(payload["comments"][0]["heading"], "Steps");
                assert_eq!(payload["comments"][0]["quote"], "- Build widgets");
                assert_eq!(
                    payload["comments"][0]["comment"],
                    "Add tests\nInclude edge cases!"
                );
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn ratatui_renders_bluesky_radar_with_after_timer_and_clean_table_links() {
        let star_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/bluesky_radar.star");
        let ui_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/bluesky_radar.ui.json");
        let compiled =
            starness_starlark::compile_starlark_file(&star_path, Some("chart")).unwrap();
        let sheet =
            UiViewSheet::from_json_str(&std::fs::read_to_string(&ui_path).unwrap()).unwrap();
        assert_eq!(sheet.validate_against(&compiled.ir), Ok(()));

        let ctx = json!({
            "query": "ai harness",
            "ticks": 1,
            "status_label": "Tick #1 (+2 new) · polling every 10s",
            "summary_kv": {"query": "ai harness", "ticks": 1, "posts": 2},
            "posts_rows": [
                ["[@alice.bsky.social](https://bsky.app/profile/alice.bsky.social/post/1)", "12", "Line one\nLine two with [link](https://example.com)"],
                ["[@bob.bsky.social](https://bsky.app/profile/bob.bsky.social/post/2)", "5", "Second post"]
            ]
        });

        let mut model = AppModel::new(
            compiled.ir.clone(),
            Some(sheet),
            vec!["waiting".to_owned()],
            ctx,
        );
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_app(f, &model)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        // Must render the waiting UI even though `waiting` has an `after` timer!
        assert!(screen.contains("Bluesky Live Feed"));
        assert!(screen.contains("@alice.bsky.social"));
        assert!(screen.contains("@bob.bsky.social"));
        assert!(!screen.contains("](https://")); // Markdown links stripped cleanly in table cells
        assert!(screen.contains("https://bsky.app/profile/alice.bsky.social/post/1")); // Post link shown in detail strip!
        assert!(screen.contains("Search Query"));
        assert!(screen.contains("Update Query"));
        assert!(screen.contains("Pause"));
        assert!(screen.contains("Stop"));

        // Tab to Table, move Down to row 2, verify focus + row survive a `fetching` -> `waiting` poll tick!
        model.handle_key(KeyEvent::from(KeyCode::Tab));
        model.handle_key(KeyEvent::from(KeyCode::Down));
        model.sync_machine_state(
            vec!["fetching".to_owned()],
            model.context.clone(),
            Status::Running,
            Value::Null,
            None,
        );
        model.sync_machine_state(
            vec!["waiting".to_owned()],
            model.context.clone(),
            Status::Running,
            Value::Null,
            None,
        );
        term.draw(|f| render_app(f, &model)).unwrap();
        let screen_after_poll: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen_after_poll.contains("https://bsky.app/profile/bob.bsky.social/post/2"));
        assert!(screen_after_poll.contains("open link"));

        // Shift+Tab back to Search Query, edit via modal, save with Ctrl+S, Tab twice to Actions -> Enter triggers UPDATE_QUERY
        model.handle_key(KeyEvent::from(KeyCode::BackTab));
        model.handle_key(KeyEvent::from(KeyCode::Enter));
        for _ in 0.."ai harness".len() {
            model.handle_key(KeyEvent::from(KeyCode::Backspace));
        }
        for ch in "cats".chars() {
            model.handle_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        model.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        model.handle_key(KeyEvent::from(KeyCode::Tab)); // -> Table
        model.handle_key(KeyEvent::from(KeyCode::Tab)); // -> ActionBar
        let action = model.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            match action {
                AppAction::Dispatch {
                    event_type,
                    payload,
                } => (event_type, payload["query"].as_str().map(str::to_owned)),
                other => panic!("expected Dispatch, got {other:?}"),
            },
            ("UPDATE_QUERY".to_owned(), Some("cats".to_owned()))
        );
    }

    #[test]
    fn ratatui_after_timer_state_without_ui_shows_fallback_picker() {
        let star_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/bluesky_radar.star");
        let compiled =
            starness_starlark::compile_starlark_file(&star_path, Some("chart")).unwrap();

        // In `fetching` (has `invoke`), shows Running spinner
        let model_fetch = AppModel::new(
            compiled.ir.clone(),
            None,
            vec!["fetching".to_owned()],
            compiled.ir.context.clone(),
        );
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_app(f, &model_fetch)).unwrap();
        let fetch_screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(fetch_screen.contains("Running in [fetching]"));
        assert!(fetch_screen.contains("[fetching]")); // not chopped on 80-col header!

        // In `waiting` (no `invoke`, only `after` timer + `on` events), shows FallbackPicker!
        let mut model_wait = AppModel::new(
            compiled.ir.clone(),
            None,
            vec!["waiting".to_owned()],
            compiled.ir.context.clone(),
        );
        term.draw(|f| render_app(f, &model_wait)).unwrap();
        let wait_screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(wait_screen.contains("Choose Event"));
        assert!(wait_screen.contains("REFRESH"));
        assert!(wait_screen.contains("PAUSE"));

        // Move down 2x to PAUSE and press Enter
        model_wait.handle_key(KeyEvent::from(KeyCode::Down));
        model_wait.handle_key(KeyEvent::from(KeyCode::Down));
        let action = model_wait.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(
            action,
            AppAction::Dispatch { event_type, .. } if event_type == "PAUSE"
        ));
    }

    fn xterm256_to_hex(idx: u8) -> String {
        match idx {
            0 => "#18181b".to_owned(),
            1 => "#f43f5e".to_owned(),
            2 => "#22c55e".to_owned(),
            3 => "#eab308".to_owned(),
            4 => "#3b82f6".to_owned(),
            5 => "#a855f7".to_owned(),
            6 => "#06b6d4".to_owned(),
            7 => "#d4d4d8".to_owned(),
            8 => "#71717a".to_owned(),
            9 => "#fb7185".to_owned(),
            10 => "#4ade80".to_owned(),
            11 => "#fde047".to_owned(),
            12 => "#60a5fa".to_owned(),
            13 => "#c084fc".to_owned(),
            14 => "#22d3ee".to_owned(),
            15 => "#fafafa".to_owned(),
            16..=231 => {
                let n = idx - 16;
                let r = n / 36;
                let g = (n % 36) / 6;
                let b = n % 6;
                let comp = |c: u8| if c == 0 { 0u8 } else { 55 + c * 40 };
                format!("#{:02x}{:02x}{:02x}", comp(r), comp(g), comp(b))
            }
            232..=255 => {
                let v = 8 + (idx - 232) * 10;
                format!("#{v:02x}{v:02x}{v:02x}")
            }
        }
    }

    fn color_to_hex(color: Color, default_hex: &str) -> String {
        match color {
            Color::Reset => default_hex.to_owned(),
            Color::Black => "#09090b".to_owned(),
            Color::Red => "#f43f5e".to_owned(),
            Color::Green => "#4ade80".to_owned(),
            Color::Yellow => "#facc15".to_owned(),
            Color::Blue => "#60a5fa".to_owned(),
            Color::Magenta => "#c084fc".to_owned(),
            Color::Cyan => "#22d3ee".to_owned(),
            Color::Gray => "#a1a1aa".to_owned(),
            Color::DarkGray => "#52525b".to_owned(),
            Color::LightRed => "#fb7185".to_owned(),
            Color::LightGreen => "#86efac".to_owned(),
            Color::LightYellow => "#fef08a".to_owned(),
            Color::LightBlue => "#93c5fd".to_owned(),
            Color::LightMagenta => "#d8b4fe".to_owned(),
            Color::LightCyan => "#67e8f9".to_owned(),
            Color::White => "#f4f4f5".to_owned(),
            Color::Indexed(i) => xterm256_to_hex(i),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    fn escape_xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// Renders a Ratatui `TestBackend` buffer into a crisp standalone dark-mode SVG screenshot
    /// with zero external dependencies (~2ms per frame).
    fn buffer_to_svg(buf: &ratatui::buffer::Buffer) -> String {
        let cell_w = 8.4_f32;
        let cell_h = 18.0_f32;
        let pad = 14.0_f32;
        let width = (buf.area.width as f32 * cell_w + pad * 2.0).ceil() as u32;
        let height = (buf.area.height as f32 * cell_h + pad * 2.0).ceil() as u32;
        let bg_default = "#09090b";
        let fg_default = "#e4e4e7";

        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n\
             <rect width=\"100%\" height=\"100%\" rx=\"10\" fill=\"{bg_default}\"/>\n\
             <g font-family=\"ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace\" font-size=\"13.5\">\n"
        );

        for y in 0..buf.area.height {
            let py = pad + (y as f32) * cell_h;
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                let bg = color_to_hex(cell.bg, bg_default);
                if bg != bg_default {
                    let px = pad + (x as f32) * cell_w;
                    svg.push_str(&format!(
                        "<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{:.1}\" height=\"{cell_h:.1}\" fill=\"{bg}\"/>\n",
                        cell_w + 0.4
                    ));
                }
                let sym = cell.symbol();
                if sym.is_empty() || sym == " " {
                    continue;
                }
                let px = pad + (x as f32) * cell_w;
                let fg = color_to_hex(cell.fg, fg_default);
                let bold = if cell.modifier.contains(Modifier::BOLD) {
                    " font-weight=\"700\""
                } else {
                    ""
                };
                let underline = if cell.modifier.contains(Modifier::UNDERLINED) {
                    " text-decoration=\"underline\""
                } else {
                    ""
                };
                svg.push_str(&format!(
                    "<text x=\"{px:.1}\" y=\"{:.1}\" fill=\"{fg}\"{bold}{underline}>{}</text>\n",
                    py + 13.5,
                    escape_xml(sym)
                ));
            }
        }
        svg.push_str("</g>\n</svg>\n");
        svg
    }

    #[test]
    fn ratatui_renders_all_20_showcase_widgets_and_exports_svg_snapshots() {
        let star_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/ui_showcase.star");
        let ui_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/ui_showcase.ui.json");
        let compiled =
            starness_starlark::compile_starlark_file(&star_path, Some("chart")).unwrap();
        let sheet =
            UiViewSheet::from_json_str(&std::fs::read_to_string(&ui_path).unwrap()).unwrap();
        assert_eq!(sheet.validate_against(&compiled.ir), Ok(()));

        let snap_dir = std::env::var("STARNESS_TUI_SNAPSHOT_DIR").ok();
        let screens = [
            ("display", "showcase_1_display.svg", "Workspace Crates", 34),
            ("inputs", "showcase_2_inputs.svg", "Pre-flight Checks", 34),
            ("markdown_review", "showcase_3_markdown.svg", "Release Checklist", 34),
            ("diff_review", "showcase_4_diff.svg", "route_event", 34),
            ("ai_elements", "showcase_5_ai_elements.svg", "Model Reasoning", 42),
        ];

        for (state_name, filename, expected_text, height) in screens {
            let model = AppModel::new(
                compiled.ir.clone(),
                Some(sheet.clone()),
                vec![state_name.to_owned()],
                compiled.ir.context.clone(),
            );
            let backend = TestBackend::new(118, height);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| render_app(f, &model)).unwrap();
            let text: String = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                text.contains(expected_text),
                "screen `{state_name}` missing `{expected_text}`"
            );
            let svg = buffer_to_svg(term.backend().buffer());
            assert!(svg.starts_with("<svg ") && svg.contains("</svg>"));
            if let Some(dir) = &snap_dir {
                let out_path = std::path::Path::new(dir).join(filename);
                std::fs::write(out_path, svg).unwrap();
            }
        }
    }

    #[test]
    fn ratatui_prompt_input_types_and_dispatches_without_global_hotkey_conflict() {
        let star_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/coding_session_llm.star");
        let ui_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/coding_session_llm.ui.json");
        let compiled =
            starness_starlark::compile_starlark_file(&star_path, Some("chart")).unwrap();
        let sheet =
            UiViewSheet::from_json_str(&std::fs::read_to_string(&ui_path).unwrap()).unwrap();
        assert_eq!(sheet.validate_against(&compiled.ir), Ok(()));

        let mut model = AppModel::new(
            compiled.ir.clone(),
            Some(sheet),
            vec!["gather_input".to_owned()],
            compiled.ir.context.clone(),
        );
        // Focus PromptInput widget
        let prompt_idx = model
            .widgets
            .iter()
            .position(|w| matches!(w, FocusWidget::PromptInput { .. }))
            .expect("PromptInput widget should exist");
        model.focused = prompt_idx;

        // Type "quit test" — 'q' and 't' must NOT quit or toggle trace drawer!
        for ch in "quit test".chars() {
            let act = model.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            assert!(matches!(act, AppAction::None));
        }
        assert_eq!(model.drawer, DrawerMode::None);

        // Press Enter -> dispatches PROMPT with {"text": "quit test"}
        let act = model.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match act {
            AppAction::Dispatch {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "PROMPT");
                assert_eq!(payload["text"], "quit test");
            }
            other => panic!("expected PROMPT dispatch, got {other:?}"),
        }
    }
}

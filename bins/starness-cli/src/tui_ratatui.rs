//! Fullscreen Ratatui TUI (`starness run -i`) with Bubble Tea–style Model/Update/View loop.
//!
//! Renders all 15 `@starness/ui` `json-render` widgets (`Stack`, `Card`, `Markdown`, `Alert`,
//! `KeyValue`, `Table`, `Badge`, `TextInput`, `Textarea`, `Select`, `MultiSelect`, `Button`,
//! `Confirm`, `MarkdownReview`, `DiffReview`) plus a zero-config `allowed_events` picker,
//! live streaming trace drawer (`t`), live IR statechart diagram drawer (`g`), and context JSON
//! drawer (`x`).

use std::io;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
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
    Block, BorderType, Clear, List, ListItem, Paragraph, Row, Table as RatatuiTable, Wrap,
};
use ratatui::{Frame, Terminal};
use serde_json::{Map, Value, json};
use starness_ir::{MachineDef, StateId, StateKind, StateNodeDef, UiViewSheet};
use starness_runtime::{
    StarnessActor, TraceEvent, TraceRecord, active_states_have_autonomous_work,
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
enum FocusWidget {
    MarkdownReview {
        props: Map<String, Value>,
        selected_line: usize,
    },
    DiffReview {
        props: Map<String, Value>,
        selected_line: usize,
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
    Confirm {
        title: String,
        confirm_event: String,
        cancel_event: Option<String>,
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
    Table {
        title: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Markdown {
        title: Option<String>,
        content: String,
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
    live_progress: Vec<String>,
    display_items: Vec<DisplayItem>,
    widgets: Vec<FocusWidget>,
    focused: usize,
    scroll_offset: u16,
    modal: Option<InputModal>,
    modal_focus: ModalFocus,
    status_flash: Option<String>,
    last_state_key: String,
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
            scroll_offset: 0,
            modal: None,
            modal_focus: ModalFocus::Input,
            status_flash: None,
            last_state_key: String::new(),
        };
        model.sync_machine_state(active_states, context, Status::Running, Value::Null, None);
        model
    }

    fn is_autonomous(&self) -> bool {
        self.status == Status::Running
            && active_states_have_autonomous_work(&self.ir, &self.active_states)
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
        self.display_items.clear();
        self.widgets.clear();
        self.focused = 0;
        self.scroll_offset = 0;

        if self.status != Status::Running || self.is_autonomous() {
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

        if self.widgets.is_empty() {
            let allowed = self.ir.allowed_events_for_active_states(&self.active_states);
            if !allowed.is_empty() {
                self.widgets.push(FocusWidget::FallbackPicker {
                    events: allowed,
                    cursor: 0,
                });
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
                self.live_progress.push(s.clone());
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

        // Global shortcuts (when not inside a modal)
        match key.code {
            KeyCode::Char('q') => return AppAction::Quit,
            KeyCode::Char('t') => {
                self.toggle_drawer(DrawerMode::Trace);
                return AppAction::None;
            }
            KeyCode::Char('g') => {
                self.toggle_drawer(DrawerMode::Diagram);
                return AppAction::None;
            }
            KeyCode::Char('x') => {
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

        if self.is_autonomous() || self.widgets.is_empty() {
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
            FocusWidget::Confirm {
                confirm_event,
                cancel_event,
                ..
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let payload = diff_draft_payload(&Map::new(), &self.context, &self.draft_state);
                    return AppAction::Dispatch {
                        event_type: confirm_event.clone(),
                        payload,
                    };
                }
                KeyCode::Char('n') => {
                    if let Some(ev) = cancel_event {
                        let payload =
                            diff_draft_payload(&Map::new(), &self.context, &self.draft_state);
                        return AppAction::Dispatch {
                            event_type: ev.clone(),
                            payload,
                        };
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
        if self.is_autonomous() {
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
            Some(FocusWidget::ActionBar { .. }) => {
                vec![("←/→", "select action"), ("Enter", "trigger")]
            }
            Some(FocusWidget::TextInput { .. }) => vec![("Enter", "edit")],
            Some(FocusWidget::Select { .. }) => vec![("↑/↓", "move"), ("Enter", "select")],
            Some(FocusWidget::MultiSelect { .. }) => vec![("↑/↓", "move"), ("Space", "toggle")],
            Some(FocusWidget::Confirm { .. }) => vec![("y/Enter", "confirm"), ("n", "cancel")],
            Some(FocusWidget::FallbackPicker { .. }) => {
                vec![("↑/↓", "pick"), ("Enter", "send"), ("p", "+payload")]
            }
            None => Vec::new(),
        };
        if self.widgets.len() > 1 {
            let next_label = match self.widgets.get(self.focused) {
                Some(FocusWidget::ActionBar { .. }) => "back to content",
                _ => "actions",
            };
            pairs.push(("Tab", next_label));
        }
        pairs
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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let run_res = run_ratatui_loop(&mut terminal, compiled, ui_sheet, actor).await;

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
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
                if let Some(Ok(TermEvent::Key(key))) = maybe_ev {
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
    let right_line = Line::from(right_spans);
    let right_w = right_line.width() as u16;

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER_MUTED));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.width > right_w + 18 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(right_w)])
            .split(inner);
        f.render_widget(Paragraph::new(left_line), cols[0]);
        f.render_widget(Paragraph::new(right_line), cols[1]);
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

    if model.is_autonomous() {
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
        for p in model.live_progress.iter().rev().take(20).rev() {
            lines.push(Line::styled(p.as_str(), Style::default().fg(TEXT_SECONDARY)));
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

    // Split top display items from interactive widgets.
    // When there is no full-height review/picker widget, let display items (e.g. a live Table)
    // take the flexible remaining height while compact inputs/ActionBar sit neatly below.
    let has_primary_widget = model.widgets.iter().any(|w| {
        matches!(
            w,
            FocusWidget::MarkdownReview { .. }
                | FocusWidget::DiffReview { .. }
                | FocusWidget::FallbackPicker { .. }
        )
    });
    let (display_area, widgets_area) = if model.display_items.is_empty() {
        (None, area)
    } else if has_primary_widget {
        let display_height = estimate_display_height(&model.display_items).min(area.height / 3);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(display_height), Constraint::Min(6)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        let widgets_height = estimate_compact_widgets_height(&model.widgets, area.width)
            .min(area.height.saturating_sub(4));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(widgets_height)])
            .split(area);
        (Some(chunks[0]), chunks[1])
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

fn estimate_compact_widgets_height(widgets: &[FocusWidget], area_width: u16) -> u16 {
    let mut total: u16 = 0;
    for w in widgets {
        total = total.saturating_add(match w {
            FocusWidget::Select { options, .. } => (options.len() as u16).min(6) + 2,
            FocusWidget::MultiSelect { options, .. } => (options.len() as u16).min(6) + 2,
            FocusWidget::Confirm { .. } => 4,
            FocusWidget::TextInput { .. } => 3,
            FocusWidget::ActionBar { buttons, .. } => action_bar_height(buttons, area_width),
            _ => 6,
        });
    }
    total
}

fn estimate_display_height(items: &[DisplayItem]) -> u16 {
    if items.is_empty() {
        return 0;
    }
    let mut rows: u16 = 2; // outer border
    let skip_first_heading = matches!(items.first().map(|i| &i.kind), Some(DisplayKind::Heading(_)));
    for (idx, item) in items.iter().enumerate() {
        if idx == 0 && skip_first_heading {
            continue;
        }
        rows = rows.saturating_add(match &item.kind {
            DisplayKind::Heading(_) | DisplayKind::Badge(_, _) => 1,
            DisplayKind::Alert { .. } => 2,
            DisplayKind::KeyValue { entries, title } => {
                if is_compact_kv(entries) {
                    1 + u16::from(title.is_some())
                } else {
                    entries.len() as u16 + u16::from(title.is_some())
                }
            }
            DisplayKind::Table { rows: r, .. } => (r.len() as u16).min(5) + 2,
            DisplayKind::Markdown { content, .. } => (content.lines().count() as u16).min(6),
        });
    }
    rows.clamp(3, 12)
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
                let color = variant_color(variant);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" ● {title}: "),
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
            DisplayKind::Table {
                title,
                columns,
                rows,
            } => {
                lines.push(Line::from(""));
                if let Some(t) = title {
                    lines.push(Line::styled(
                        format!(" {t}"),
                        Style::default().fg(TEXT_MUTED).add_modifier(Modifier::BOLD),
                    ));
                }
                lines.push(Line::styled(
                    format!("  {}", columns.join(" │ ")),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
                for r in rows.iter().take(25) {
                    lines.push(Line::styled(
                        format!("  {}", r.join(" │ ")),
                        Style::default().fg(TEXT_SECONDARY),
                    ));
                }
            }
            DisplayKind::Markdown { title, content } => {
                if let Some(t) = title {
                    lines.push(Line::styled(t.clone(), Style::default().fg(ACCENT)));
                }
                for ml in parse_md_lines(content) {
                    lines.push(Line::from(render_md_line_spans(&ml.raw, ml.in_code_fence)));
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
            FocusWidget::MarkdownReview { .. } | FocusWidget::DiffReview { .. } => {
                Constraint::Min(10)
            }
            FocusWidget::FallbackPicker { events, .. } => {
                Constraint::Min((events.len() as u16).saturating_add(3))
            }
            FocusWidget::Select { options, .. } => {
                Constraint::Length((options.len() as u16).min(6) + 2)
            }
            FocusWidget::MultiSelect { options, .. } => {
                Constraint::Length((options.len() as u16).min(6) + 2)
            }
            FocusWidget::Confirm { .. } => Constraint::Length(4),
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
            FocusWidget::Confirm {
                title,
                confirm_event,
                cancel_event,
            } => {
                let mut spans = vec![
                    Span::styled(
                        format!(" [y] {confirm_event} "),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                ];
                if let Some(cev) = cancel_event {
                    spans.push(Span::styled(
                        format!(" [n] {cev} "),
                        Style::default().fg(Color::White).bg(Color::DarkGray),
                    ));
                }
                f.render_widget(
                    Paragraph::new(Line::from(spans)).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .title(format!(" {title} "))
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
            display.push(DisplayItem {
                kind: DisplayKind::Table {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    columns,
                    rows,
                },
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
        "Confirm" => {
            if let Some(ev) = props.get("confirm_event").and_then(Value::as_str) {
                widgets.push(FocusWidget::Confirm {
                    title: props
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Confirm")
                        .to_owned(),
                    confirm_event: ev.to_owned(),
                    cancel_event: props
                        .get("cancel_event")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
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
}

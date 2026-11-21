//! Interactive terminal UI (`starness run -i`) for human-in-the-loop (HITL) statecharts.
//!
//! Supports two modes whenever a machine parks in a quiescent waiting state:
//! 1. **`json-render` TUI interpreter** when the active state exposes a `ui` spec (`meta["ui"]` or
//!    dynamic `$state` binding), including block-by-block `MarkdownReview` section commenting.
//! 2. **Zero-config event picker** derived automatically from `allowed_events` when no `ui` spec
//!    is declared on the active state.

use std::io::{BufRead, Write};

use serde_json::{Map, Value, json};
use starness_ir::{MachineDef, UiViewSheet};

/// Outcome of one interactive terminal prompt cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractiveDecision {
    Dispatch { event_type: String, payload: Value },
    Quit,
}

#[derive(Debug, Clone)]
struct ActionChoice {
    label: String,
    event: String,
    payload: Map<String, Value>,
}

#[derive(Debug, Clone)]
struct MarkdownSection {
    heading: String,
    lines: Vec<String>,
}

/// Prompts the user for the next statechart event using either the active state's `json-render`
/// UI spec or an auto-generated fallback picker of `allowed_events`.
pub fn prompt_interactive_event<R: BufRead, W: Write>(
    ir: &MachineDef,
    ui_sheet: Option<&UiViewSheet>,
    active_states: &[String],
    context: &Value,
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<InteractiveDecision> {
    let allowed = ir.allowed_events_for_active_states(active_states);
    if allowed.is_empty() {
        return Ok(InteractiveDecision::Quit);
    }

    if let Some(ui_spec) =
        ui_sheet.and_then(|s| s.resolve_for_active_states(ir, active_states, context))
    {
        return run_ui_spec_prompt(&ui_spec, active_states, context, &allowed, reader, writer);
    }

    run_fallback_event_picker(active_states, &allowed, reader, writer)
}

fn run_fallback_event_picker<R: BufRead, W: Write>(
    active_states: &[String],
    allowed: &[String],
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<InteractiveDecision> {
    writeln!(
        writer,
        "\n=== Waiting in [{}] ===",
        active_states.join(", ")
    )?;
    for (idx, ev) in allowed.iter().enumerate() {
        writeln!(writer, "  [{}] {ev}", idx + 1)?;
    }
    writeln!(writer, "  [q] Quit")?;

    loop {
        write!(
            writer,
            "Select event (number or name, optional JSON payload after space): "
        )?;
        writer.flush()?;

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(InteractiveDecision::Quit);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("quit") {
            return Ok(InteractiveDecision::Quit);
        }

        if trimmed.starts_with('{') {
            if let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(trimmed)
                && let Some(ev) = obj
                    .remove("type")
                    .and_then(|v| v.as_str().map(str::to_owned))
            {
                let payload = if obj.is_empty() {
                    Value::Null
                } else {
                    Value::Object(obj)
                };
                return Ok(InteractiveDecision::Dispatch {
                    event_type: ev,
                    payload,
                });
            }
            writeln!(writer, "Invalid JSON event object; expected {{\"type\": \"...\"}}.")?;
            continue;
        }

        let (head, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((h, r)) => (h.trim(), r.trim()),
            None => (trimmed, ""),
        };

        let chosen = if let Ok(num) = head.parse::<usize>()
            && num >= 1
            && num <= allowed.len()
        {
            allowed[num - 1].clone()
        } else if allowed.iter().any(|e| e == head) {
            head.to_owned()
        } else {
            writeln!(writer, "Unknown selection `{head}`. Pick 1..={} or `q`.", allowed.len())?;
            continue;
        };

        let payload = if rest.is_empty() {
            Value::Null
        } else {
            match serde_json::from_str::<Value>(rest) {
                Ok(v) => v,
                Err(err) => {
                    writeln!(writer, "Invalid JSON payload `{rest}`: {err}")?;
                    continue;
                }
            }
        };

        return Ok(InteractiveDecision::Dispatch {
            event_type: chosen,
            payload,
        });
    }
}

fn run_ui_spec_prompt<R: BufRead, W: Write>(
    ui_spec: &Value,
    active_states: &[String],
    context: &Value,
    allowed: &[String],
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<InteractiveDecision> {
    let Some(root_id) = ui_spec.get("root").and_then(Value::as_str) else {
        return run_fallback_event_picker(active_states, allowed, reader, writer);
    };
    let Some(elements) = ui_spec.get("elements").and_then(Value::as_object) else {
        return run_fallback_event_picker(active_states, allowed, reader, writer);
    };

    let mut draft_state = context.clone();
    if !draft_state.is_object() {
        draft_state = json!({});
    }
    let mut actions: Vec<ActionChoice> = Vec::new();
    let mut review_elem: Option<Value> = None;

    writeln!(
        writer,
        "\n=== Interactive UI [{}] ===",
        active_states.join(", ")
    )?;
    render_element_tree(
        root_id,
        elements,
        &mut draft_state,
        &mut actions,
        &mut review_elem,
        0,
        reader,
        writer,
    )?;

    if let Some(review_props) = review_elem {
        return run_markdown_review_loop(
            &review_props,
            &mut draft_state,
            &actions,
            reader,
            writer,
        );
    }

    if actions.is_empty() {
        return run_fallback_event_picker(active_states, allowed, reader, writer);
    }

    writeln!(writer, "\nActions:")?;
    for (idx, act) in actions.iter().enumerate() {
        writeln!(writer, "  [{}] {} ({})", idx + 1, act.label, act.event)?;
    }
    writeln!(writer, "  [q] Quit")?;

    loop {
        write!(writer, "Choose action [1-{} or q]: ", actions.len())?;
        writer.flush()?;
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(InteractiveDecision::Quit);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("quit") {
            return Ok(InteractiveDecision::Quit);
        }
        if let Ok(num) = trimmed.parse::<usize>()
            && num >= 1
            && num <= actions.len()
        {
            let chosen = &actions[num - 1];
            let payload = merge_draft_into_payload(&chosen.payload, context, &draft_state);
            return Ok(InteractiveDecision::Dispatch {
                event_type: chosen.event.clone(),
                payload,
            });
        }
        if let Some(chosen) = actions.iter().find(|a| a.event.eq_ignore_ascii_case(trimmed)) {
            let payload = merge_draft_into_payload(&chosen.payload, context, &draft_state);
            return Ok(InteractiveDecision::Dispatch {
                event_type: chosen.event.clone(),
                payload,
            });
        }
        writeln!(writer, "Invalid choice `{trimmed}`.")?;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_element_tree<R: BufRead, W: Write>(
    elem_id: &str,
    elements: &Map<String, Value>,
    draft_state: &mut Value,
    actions: &mut Vec<ActionChoice>,
    review_elem: &mut Option<Value>,
    depth: usize,
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<()> {
    let Some(elem) = elements.get(elem_id) else {
        return Ok(());
    };
    let kind = elem.get("type").and_then(Value::as_str).unwrap_or("");
    let props = elem
        .get("props")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let pad = "  ".repeat(depth);

    match kind {
        "Stack" => {}
        "Card" => {
            if let Some(title) = props.get("title").and_then(Value::as_str)
                && !title.is_empty()
            {
                writeln!(writer, "{pad}-- {title} --")?;
            }
            if let Some(desc) = props.get("description").and_then(Value::as_str)
                && !desc.is_empty()
            {
                writeln!(writer, "{pad}{desc}")?;
            }
        }
        "Badge" => {
            let label = resolve_prop_str(props.get("label"), draft_state);
            writeln!(writer, "{pad}[{label}]")?;
        }
        "Markdown" => {
            if let Some(title) = props.get("title").and_then(Value::as_str) {
                writeln!(writer, "{pad}{title}:")?;
            }
            let content = resolve_prop_str(props.get("content"), draft_state);
            for line in content.lines() {
                writeln!(writer, "{pad}{line}")?;
            }
        }
        "KeyValue" => {
            if let Some(title) = props.get("title").and_then(Value::as_str) {
                writeln!(writer, "{pad}{title}:")?;
            }
            let items = resolve_prop_value(props.get("items"), draft_state);
            if let Some(map) = items.as_object() {
                for (k, v) in map {
                    let rendered = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
                    writeln!(writer, "{pad}  {k}: {rendered}")?;
                }
            }
        }
        "Table" => {
            if let Some(title) = props.get("title").and_then(Value::as_str) {
                writeln!(writer, "{pad}{title}:")?;
            }
            if let Some(cols) = props.get("columns").and_then(Value::as_array) {
                let header: Vec<String> = cols
                    .iter()
                    .filter_map(|c| c.as_str().map(str::to_owned))
                    .collect();
                writeln!(writer, "{pad}  {}", header.join(" | "))?;
            }
            let rows = resolve_prop_value(props.get("rows"), draft_state);
            if let Some(arr) = rows.as_array() {
                for row in arr {
                    if let Some(cells) = row.as_array() {
                        let line: Vec<String> = cells
                            .iter()
                            .map(|c| c.as_str().map(str::to_owned).unwrap_or_else(|| c.to_string()))
                            .collect();
                        writeln!(writer, "{pad}  {}", line.join(" | "))?;
                    }
                }
            }
        }
        "TextInput" | "Textarea" => {
            let label = props.get("label").and_then(Value::as_str).unwrap_or("Input");
            let bind = props.get("bind").and_then(Value::as_str).unwrap_or("");
            let current = draft_state
                .pointer(bind)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            write!(writer, "{pad}{label} [{current}] (Enter to keep): ")?;
            writer.flush()?;
            let mut input = String::new();
            if reader.read_line(&mut input)? > 0 {
                let trimmed = input.trim();
                if !trimmed.is_empty() && !bind.is_empty() {
                    set_json_pointer(draft_state, bind, Value::String(trimmed.to_owned()));
                }
            }
        }
        "Select" => {
            let label = props.get("label").and_then(Value::as_str).unwrap_or("Select");
            let bind = props.get("bind").and_then(Value::as_str).unwrap_or("");
            let options: Vec<String> = props
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            writeln!(writer, "{pad}{label}: {}", options.join(" / "))?;
            write!(writer, "{pad}Choose value (Enter to keep current): ")?;
            writer.flush()?;
            let mut input = String::new();
            if reader.read_line(&mut input)? > 0 {
                let trimmed = input.trim();
                if !trimmed.is_empty() && options.iter().any(|o| o == trimmed) && !bind.is_empty() {
                    set_json_pointer(draft_state, bind, Value::String(trimmed.to_owned()));
                }
            }
        }
        "Confirm" => {
            let title = props.get("title").and_then(Value::as_str).unwrap_or("Confirm");
            let msg = props.get("message").and_then(Value::as_str).unwrap_or("");
            writeln!(writer, "{pad}{title}")?;
            if !msg.is_empty() {
                writeln!(writer, "{pad}{msg}")?;
            }
            if let Some(ev) = props.get("confirm_event").and_then(Value::as_str) {
                actions.push(ActionChoice {
                    label: props
                        .get("confirm_label")
                        .and_then(Value::as_str)
                        .unwrap_or("Confirm")
                        .to_owned(),
                    event: ev.to_owned(),
                    payload: Map::new(),
                });
            }
            if let Some(ev) = props.get("cancel_event").and_then(Value::as_str) {
                actions.push(ActionChoice {
                    label: props
                        .get("cancel_label")
                        .and_then(Value::as_str)
                        .unwrap_or("Cancel")
                        .to_owned(),
                    event: ev.to_owned(),
                    payload: Map::new(),
                });
            }
        }
        "Button" => {
            if let Some(ev) = props.get("event").and_then(Value::as_str) {
                actions.push(ActionChoice {
                    label: props
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(ev)
                        .to_owned(),
                    event: ev.to_owned(),
                    payload: props
                        .get("payload")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
        "MarkdownReview" => {
            *review_elem = Some(Value::Object(props));
        }
        _ => {}
    }

    if let Some(children) = elem.get("children").and_then(Value::as_array) {
        for child in children {
            if let Some(cid) = child.as_str() {
                render_element_tree(
                    cid,
                    elements,
                    draft_state,
                    actions,
                    review_elem,
                    depth + usize::from(kind == "Card"),
                    reader,
                    writer,
                )?;
            }
        }
    }

    Ok(())
}

fn run_markdown_review_loop<R: BufRead, W: Write>(
    props: &Value,
    draft_state: &mut Value,
    extra_actions: &[ActionChoice],
    reader: &mut R,
    writer: &mut W,
) -> anyhow::Result<InteractiveDecision> {
    let title = props
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Markdown Review");
    let markdown = resolve_prop_str(props.get("markdown"), draft_state);
    let agent_notes = resolve_prop_value(props.get("agent_notes"), draft_state);
    let comments_path = props
        .get("comments_path")
        .and_then(Value::as_str)
        .unwrap_or("/draft_comments");
    let approve_event = props
        .get("approve_event")
        .and_then(Value::as_str)
        .unwrap_or("APPROVE");
    let revise_event = props
        .get("revise_event")
        .and_then(Value::as_str)
        .unwrap_or("REQUEST_CHANGES");

    let sections = parse_markdown_sections(&markdown);
    let mut comments: Vec<Value> = draft_state
        .pointer(comments_path)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    print_markdown_review_sections(title, &sections, &agent_notes, &comments, writer)?;

    loop {
        writeln!(
            writer,
            "\nCommands: [c <section#>] add comment | [v] view | [a] {approve_event} | [r] {revise_event} ({} comment(s)) | [q] quit",
            comments.len()
        )?;
        if !extra_actions.is_empty() {
            let extras: Vec<String> = extra_actions
                .iter()
                .map(|a| format!("{} ({})", a.label, a.event))
                .collect();
            writeln!(writer, "Other actions: {}", extras.join(", "))?;
        }
        write!(writer, "review> ")?;
        writer.flush()?;

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(InteractiveDecision::Quit);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("quit") {
            return Ok(InteractiveDecision::Quit);
        }
        if trimmed.eq_ignore_ascii_case("v") || trimmed.eq_ignore_ascii_case("view") {
            print_markdown_review_sections(title, &sections, &agent_notes, &comments, writer)?;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("a") || trimmed.eq_ignore_ascii_case(approve_event) {
            return Ok(InteractiveDecision::Dispatch {
                event_type: approve_event.to_owned(),
                payload: json!({ "comments": comments }),
            });
        }
        if trimmed.eq_ignore_ascii_case("r") || trimmed.eq_ignore_ascii_case(revise_event) {
            return Ok(InteractiveDecision::Dispatch {
                event_type: revise_event.to_owned(),
                payload: json!({ "comments": comments }),
            });
        }
        if let Some(rest) = trimmed
            .strip_prefix("c ")
            .or_else(|| trimmed.strip_prefix("comment "))
        {
            let rest = rest.trim();
            let (sec_str, inline_comment) = match rest.split_once(char::is_whitespace) {
                Some((s, c)) => (s.trim(), Some(c.trim())),
                None => (rest, None),
            };
            let Ok(sec_num) = sec_str.parse::<usize>() else {
                writeln!(writer, "Usage: c <section#> [comment text]")?;
                continue;
            };
            if sec_num == 0 || sec_num > sections.len() {
                writeln!(writer, "Section number must be between 1 and {}.", sections.len())?;
                continue;
            }
            let heading = sections[sec_num - 1].heading.clone();
            let (quote, comment_text) = if let Some(inline) = inline_comment {
                (String::new(), inline.to_owned())
            } else {
                write!(writer, "Optional quote (Enter to skip): ")?;
                writer.flush()?;
                let mut q_line = String::new();
                let _ = reader.read_line(&mut q_line)?;
                write!(writer, "Comment for `{heading}`: ")?;
                writer.flush()?;
                let mut c_line = String::new();
                let _ = reader.read_line(&mut c_line)?;
                (q_line.trim().to_owned(), c_line.trim().to_owned())
            };
            if comment_text.is_empty() {
                writeln!(writer, "Ignored empty comment.")?;
                continue;
            }
            comments.push(json!({
                "heading": heading,
                "quote": quote,
                "comment": comment_text,
            }));
            set_json_pointer(draft_state, comments_path, Value::Array(comments.clone()));
            writeln!(writer, "Attached comment #{} to `{heading}`.", comments.len())?;
            continue;
        }
        if let Some(extra) = extra_actions
            .iter()
            .find(|a| a.event.eq_ignore_ascii_case(trimmed))
        {
            return Ok(InteractiveDecision::Dispatch {
                event_type: extra.event.clone(),
                payload: Value::Object(extra.payload.clone()),
            });
        }

        writeln!(writer, "Unknown command `{trimmed}`.")?;
    }
}

fn print_markdown_review_sections<W: Write>(
    title: &str,
    sections: &[MarkdownSection],
    agent_notes: &Value,
    comments: &[Value],
    writer: &mut W,
) -> anyhow::Result<()> {
    writeln!(writer, "\n## {title}")?;
    let notes_arr = agent_notes.as_array().cloned().unwrap_or_default();

    for (idx, sec) in sections.iter().enumerate() {
        writeln!(writer, "\n[{}] {}", idx + 1, sec.heading)?;
        for line in &sec.lines {
            if !line.trim().is_empty() {
                writeln!(writer, "    {line}")?;
            }
        }
        for note in &notes_arr {
            let note_heading = note.get("heading").and_then(Value::as_str);
            let matches = match note_heading {
                None => idx == 0,
                Some("*") => idx == 0,
                Some(h) => h.eq_ignore_ascii_case(&sec.heading),
            };
            if matches {
                let text = note
                    .get("note")
                    .or_else(|| note.get("text"))
                    .and_then(Value::as_str)
                    .or_else(|| note.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    writeln!(writer, "    * Agent note: {text}")?;
                }
            }
        }
        for c in comments {
            if c.get("heading").and_then(Value::as_str) == Some(sec.heading.as_str()) {
                let txt = c.get("comment").and_then(Value::as_str).unwrap_or("");
                writeln!(writer, "    > Your comment: {txt}")?;
            }
        }
    }
    Ok(())
}

fn parse_markdown_sections(md: &str) -> Vec<MarkdownSection> {
    let mut sections = Vec::new();
    let mut current = MarkdownSection {
        heading: "Overview".to_owned(),
        lines: Vec::new(),
    };
    for line in md.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("#### ")
            .or_else(|| trimmed.strip_prefix("### "))
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            if !current.lines.is_empty() || !sections.is_empty() {
                sections.push(current);
            }
            current = MarkdownSection {
                heading: rest.trim().to_owned(),
                lines: Vec::new(),
            };
        } else {
            current.lines.push(line.to_owned());
        }
    }
    sections.push(current);
    sections
}

fn resolve_prop_value(prop: Option<&Value>, state: &Value) -> Value {
    let Some(val) = prop else {
        return Value::Null;
    };
    if let Some(ptr) = val.get("$state").and_then(Value::as_str) {
        return state.pointer(ptr).cloned().unwrap_or(Value::Null);
    }
    val.clone()
}

fn resolve_prop_str(prop: Option<&Value>, state: &Value) -> String {
    let resolved = resolve_prop_value(prop, state);
    match resolved {
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

fn merge_draft_into_payload(
    base: &Map<String, Value>,
    original_ctx: &Value,
    draft_state: &Value,
) -> Value {
    let mut out = base.clone();
    if let Some(draft_obj) = draft_state.as_object() {
        let orig_obj = original_ctx.as_object();
        for (k, v) in draft_obj {
            let changed = orig_obj.and_then(|o| o.get(k)) != Some(v);
            if changed && !out.contains_key(k) {
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
    use std::io::Cursor;

    use starness_starlark::compile_starlark_str;

    use super::*;

    #[test]
    fn tui_markdown_review_attaches_anchored_comment_and_dispatches_revise() {
        let src = r##"
Ctx = record(
    plan = field(str, "# Goal\nShip UI\n\n## Steps\n- Add TUI"),
    notes = field(list[dict], [{"heading": "Steps", "note": "Check non-TTY tests"}]),
    draft_comments = field(list[dict], []),
)
S = state_space("reviewing", "done")

chart = machine(
    id = "tui_review_test",
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
                "machine": "tui_review_test",
                "views": {
                    "reviewing": {
                        "root": "review",
                        "elements": {
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
        let compiled = compile_starlark_str("tui_review_test.star", src, None).unwrap();
        assert_eq!(ui_sheet.validate_against(&compiled.ir), Ok(()));
        let mut input = Cursor::new("c 2 Please add retry logic\nr\n");
        let mut output = Vec::new();
        let decision = prompt_interactive_event(
            &compiled.ir,
            Some(&ui_sheet),
            &["reviewing".to_owned()],
            &compiled.ir.context,
            &mut input,
            &mut output,
        )
        .unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Agent note: Check non-TTY tests"));
        assert_eq!(
            decision,
            InteractiveDecision::Dispatch {
                event_type: "REQUEST_CHANGES".to_owned(),
                payload: json!({
                    "comments": [{
                        "heading": "Steps",
                        "quote": "",
                        "comment": "Please add retry logic",
                    }]
                }),
            }
        );
    }

    #[test]
    fn tui_fallback_picker_selects_allowed_event_by_number() {
        let src = r#"
S = state_space("waiting", "approved", "rejected")
chart = machine(
    id = "fallback_test",
    initial = S.waiting,
    states = {
        S.waiting: state(
            on = [
                transition(event = "APPROVE", target = S.approved),
                transition(event = "REJECT", target = S.rejected),
            ],
        ),
        S.approved: final_state(),
        S.rejected: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("fallback_test.star", src, None).unwrap();
        let mut input = Cursor::new("1 {\"reviewer\":\"donnie\"}\n");
        let mut output = Vec::new();
        let decision = prompt_interactive_event(
            &compiled.ir,
            None,
            &["waiting".to_owned()],
            &compiled.ir.context,
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(
            decision,
            InteractiveDecision::Dispatch {
                event_type: "APPROVE".to_owned(),
                payload: json!({ "reviewer": "donnie" }),
            }
        );
    }
}

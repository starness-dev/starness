# 20-Widget Catalog Showcase (pure headless `.star` wizard — zero UI coupling):
# Exercises every widget in the dual-target `json-render` catalog across 5 focused screens:
#   1. `display`         -> Stack, Card, Badge, Alert, KeyValue, Table, Markdown
#   2. `inputs`          -> TextInput, Textarea, Select, MultiSelect, Button
#   3. `markdown_review` -> MarkdownReview (per-line gutter, notes, multiline comments)
#   4. `diff_review`     -> DiffReview (unified diff hunks + line-anchored comments)
#   5. `ai_elements`     -> Conversation, PromptInput, ToolCall, Reasoning, TaskList, Terminal
#
# Run:
#   starness check examples/ui_showcase.star --ui examples/ui_showcase.ui.json
#   starness run -i examples/ui_showcase.star --ui examples/ui_showcase.ui.json
#   starness serve examples/ui_showcase.star --ui examples/ui_showcase.ui.json --bind 127.0.0.1:8080

ShowcaseEvent = enum("NEXT", "BACK", "APPROVE", "REVISE", "PROMPT", "FINISH")

SAMPLE_MD = """# Release Checklist
Review **Starness v0.2** before tagging. This paragraph intentionally wraps across multiple lines so you can verify line spacing, gutter icon vertical alignment against the first line of wrapped prose, and inline comment drawers right beneath any selected line.

## Highlights
- Decoupled `*.ui.json` view-sheets keep `.star` files 100% headless and allow attaching different layouts to the same statechart without modifying workflow logic.
- Fullscreen Ratatui TUI and Web Studio share the same 20-widget catalog (including official shadcn AI Elements).

## Verification
- Run `cargo clippy --workspace --all-targets -- -D warnings`.
- Run `cargo test --workspace`."""

SAMPLE_DIFF = """diff --git a/src/router.rs b/src/router.rs
--- a/src/router.rs
+++ b/src/router.rs
@@ -12,7 +12,9 @@ pub fn route_event(state: &State, ev: &Event) -> Result<State> {
     if state.is_terminal() {
-        return Ok(state.clone());
+        return Err(Error::AlreadyTerminal(state.id.clone()));
     }
+    metrics::increment_counter!("events_routed");
     state.apply(ev)
 }"""

ShowcaseCtx = record(
    step_badge = field(str, "Step 1 of 5 · Display Widgets"),
    metrics = field(dict, {"catalog": "20 widgets", "targets": "Web + TUI", "status": "ready"}),
    services_rows = field(
        list[list[str]],
        [
            ["starness-ir", "ok", "Pure statechart IR & UiViewSheet validator"],
            ["starness-starlark", "ok", "Starlark 0.14 DSL & static typechecker"],
            ["starness-runtime", "ok", "Async actor, SQLite serve daemon & drivers"],
            ["starness-cli", "ok", "CLI, LSP, ACP bridge & fullscreen Ratatui TUI"],
        ],
    ),
    overview_md = field(str, "Use **Tab** to focus `Actions` and click **Next** to inspect all 5 catalog screens."),
    reviewer = field(str, "donnie"),
    notes_text = field(str, "Looks clean overall."),
    environment = field(str, "staging"),
    checklist = field(list[str], ["clippy", "tests"]),
    review_md = field(str, SAMPLE_MD),
    review_notes = field(
        list[dict],
        [
            {"heading": "Highlights", "severity": "info", "note": "Both targets validate static events via `starness check --ui`."},
            {"heading": "Verification", "severity": "warning", "note": "Verify narrow 80-column terminals as well as desktop."},
        ],
    ),
    review_comments = field(list[dict], []),
    patch_diff = field(str, SAMPLE_DIFF),
    diff_comments = field(list[dict], []),
    ai_tasks = field(
        list[dict],
        [
            {"title": "Inspect workspace crates", "status": "done", "detail": "4 crates found"},
            {"title": "Run cargo test --workspace", "status": "running", "detail": "88 tests"},
            {"title": "Summarize findings", "status": "pending"},
        ],
    ),
    ai_messages = field(
        list[dict],
        [
            {"role": "user", "text": "Run the test suite and check for any warnings."},
            {
                "role": "assistant",
                "reasoning": "I should run cargo test --workspace via bash and inspect the exit code.",
                "text": "Running the workspace tests now.",
                "tool_calls": [
                    {"id": "call_1", "name": "bash", "status": "ok", "arguments": {"command": "cargo test --workspace"}},
                ],
            },
            {"role": "tool", "fn_name": "bash", "call_id": "call_1", "content": {"exit_code": 0, "passed": 88}},
        ],
    ),
    ai_reasoning = field(str, "All 88 workspace tests passed with exit code 0. Ready to report back to the user."),
    ai_tool_args = field(dict, {"command": "cargo test --workspace"}),
    ai_tool_out = field(str, "test result: ok. 88 passed; 0 failed"),
    ai_cmd_stdout = field(str, "running 88 tests\ntest result: ok. 88 passed; 0 failed; 0 ignored"),
)

def append_showcase_prompt(ctx: ShowcaseCtx, ev: dict) -> ShowcaseCtx:
    txt = str(ev.get("text", "")).strip()
    if txt == "":
        return ctx
    return update(
        ctx,
        ai_messages = ctx.ai_messages + [{"role": "user", "text": txt}],
    )

S = state_space("display", "inputs", "markdown_review", "diff_review", "ai_elements", "done")

chart: StateMachine = machine(
    id = "ui_showcase",
    description = "5-step catalog showcase exercising all 20 Web + TUI widgets (including AI Elements)",
    initial = S.display,
    context = ShowcaseCtx(),
    states = {
        S.display: state(
            on = [
                transition(event = ShowcaseEvent.NEXT, target = S.inputs),
                transition(event = ShowcaseEvent.FINISH, target = S.done),
            ],
        ),
        S.inputs: state(
            on = [
                transition(event = ShowcaseEvent.BACK, target = S.display),
                transition(event = ShowcaseEvent.NEXT, target = S.markdown_review),
                transition(event = ShowcaseEvent.FINISH, target = S.done),
            ],
        ),
        S.markdown_review: state(
            on = [
                transition(event = ShowcaseEvent.BACK, target = S.inputs),
                transition(event = ShowcaseEvent.APPROVE, target = S.diff_review),
                internal_transition(event = ShowcaseEvent.REVISE),
            ],
        ),
        S.diff_review: state(
            on = [
                transition(event = ShowcaseEvent.BACK, target = S.markdown_review),
                transition(event = ShowcaseEvent.APPROVE, target = S.ai_elements),
                internal_transition(event = ShowcaseEvent.REVISE),
            ],
        ),
        S.ai_elements: state(
            on = [
                transition(event = ShowcaseEvent.BACK, target = S.diff_review),
                internal_transition(event = ShowcaseEvent.PROMPT, assign = append_showcase_prompt),
                transition(event = ShowcaseEvent.FINISH, target = S.done),
            ],
        ),
        S.done: final_state(output = {"status": "showcase_complete"}),
    },
)

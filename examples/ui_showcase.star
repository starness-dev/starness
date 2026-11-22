# 14-Widget Catalog Showcase (pure headless `.star` wizard — zero UI coupling):
# Exercises every widget in the dual-target `json-render` catalog across 4 focused screens:
#   1. `display`         -> Stack, Card, Badge, Alert, KeyValue, Table, Markdown
#   2. `inputs`          -> TextInput, Textarea, Select, MultiSelect, Button
#   3. `markdown_review` -> MarkdownReview (per-line gutter, notes, multiline comments)
#   4. `diff_review`     -> DiffReview (unified diff hunks + line-anchored comments)
#
# Run:
#   starness check examples/ui_showcase.star --ui examples/ui_showcase.ui.json
#   starness run -i examples/ui_showcase.star --ui examples/ui_showcase.ui.json
#   starness serve examples/ui_showcase.star --ui examples/ui_showcase.ui.json --bind 127.0.0.1:8080

ShowcaseEvent = enum("NEXT", "BACK", "APPROVE", "REVISE", "FINISH")

SAMPLE_MD = """# Release Checklist
Review **Starness v0.2** before tagging.

## Highlights
- Decoupled `*.ui.json` view-sheets keep `.star` files 100% headless.
- Fullscreen Ratatui TUI and Web Studio share the same 15-widget catalog.

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
    step_badge = field(str, "Step 1 of 4 · Display Widgets"),
    metrics = field(dict, {"catalog": "14 widgets", "targets": "Web + TUI", "status": "ready"}),
    services_rows = field(
        list[list[str]],
        [
            ["starness-ir", "ok", "Pure statechart IR & UiViewSheet validator"],
            ["starness-starlark", "ok", "Starlark 0.14 DSL & static typechecker"],
            ["starness-runtime", "ok", "Async actor, SQLite serve daemon & drivers"],
            ["starness-cli", "ok", "CLI, LSP, ACP bridge & fullscreen Ratatui TUI"],
        ],
    ),
    overview_md = field(str, "Use **Tab** to focus `Actions` and click **Next** to inspect all 4 catalog screens."),
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
)

S = state_space("display", "inputs", "markdown_review", "diff_review", "done")

chart: StateMachine = machine(
    id = "ui_showcase",
    description = "4-step catalog showcase exercising all 15 Web + TUI widgets",
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
                transition(event = ShowcaseEvent.APPROVE, target = S.done),
                internal_transition(event = ShowcaseEvent.REVISE),
            ],
        ),
        S.done: final_state(output = {"status": "showcase_complete"}),
    },
)

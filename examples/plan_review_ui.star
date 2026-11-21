# Interactive human-in-the-loop Markdown plan review statechart using `@starness/ui`:
# - Renders a `json-render` UI (`ui_markdown_review`) in BOTH:
#     1. Web Studio: `starness serve examples/plan_review_ui.star --bind 127.0.0.1:8080` -> open http://127.0.0.1:8080/ui
#     2. Terminal TUI: `starness run -i examples/plan_review_ui.star`
# - Displays structured Markdown sections alongside inline agent review notes, lets a human attach
#   anchored section comments (`[{heading, quote, comment}]`), loops through `revising` on
#   `REQUEST_CHANGES`, and completes cleanly on `APPROVE`.

load(
    "@starness/ui",
    "ui_badge",
    "ui_bind",
    "ui_button",
    "ui_card",
    "ui_key_value",
    "ui_markdown_review",
    "ui_spec",
    "ui_stack",
)

ReviewEvent = enum("APPROVE", "REQUEST_CHANGES", "ABORT")

INITIAL_PLAN = """# Goal
Add declarative and generative UI support to Starness using `json-render`.

## Architecture
- Provide `@starness/ui` helpers compiling to `json-render` `{root, elements}` specs.
- Validate static UI button event names against outgoing state transitions in `starness check`.
- Expose active `ui` and `allowed_events` on `starness serve` instance snapshots.

## Verification
- Run `cargo clippy --workspace --all-targets -- -D warnings`.
- Run `cargo test --workspace` and `starness check examples/*.star`."""

INITIAL_NOTES = [
    {
        "heading": "Architecture",
        "severity": "info",
        "note": "Static UI event validation runs during `starness check` before any runtime execution.",
    },
    {
        "heading": "Verification",
        "severity": "warning",
        "note": "Confirm both non-TTY TUI tests and `starness serve` HTTP integration tests pass.",
    },
]

PlanCtx = record(
    revision = field(int, 1),
    status_label = field(str, "Awaiting Human Review (rev 1)"),
    meta_summary = field(dict, {"revision": 1, "owner": "starness-agent", "comments_addressed": 0}),
    plan = field(str, INITIAL_PLAN),
    notes = field(list[dict], INITIAL_NOTES),
    draft_comments = field(list[dict], []),
    feedback_history = field(list[dict], []),
)

S = state_space("human_review", "revising", "approved", "aborted")

def capture_comments(ctx: PlanCtx, ev: dict) -> PlanCtx:
    incoming = ev.get("comments", [])
    if type(incoming) != "list":
        incoming = []
    return update(
        ctx,
        draft_comments = incoming,
        feedback_history = ctx.feedback_history + incoming,
    )

def apply_revisions(ctx: PlanCtx, _ev: dict) -> PlanCtx:
    next_rev = ctx.revision + 1
    additions = []
    for c in ctx.draft_comments:
        heading = str(c.get("heading", "General"))
        comment = str(c.get("comment", ""))
        if comment != "":
            additions.append("- [" + heading + "] Addressed: " + comment)
    revision_block = "\n\n## Revision " + str(next_rev) + " Updates\n" + (
        "\n".join(additions) if len(additions) > 0 else "- General refinement applied."
    )
    return update(
        ctx,
        revision = next_rev,
        status_label = "Awaiting Human Review (rev " + str(next_rev) + ")",
        meta_summary = {
            "revision": next_rev,
            "owner": "starness-agent",
            "comments_addressed": len(ctx.feedback_history),
        },
        plan = ctx.plan + revision_block,
        draft_comments = [],
    )

chart: StateMachine = machine(
    id = "plan_review_ui",
    description = "Interactive Markdown plan review loop powered by @starness/ui (Web Studio & TUI)",
    initial = S.human_review,
    context = PlanCtx(),
    states = {
        S.human_review: state(
            ui = ui_spec(
                ui_stack([
                    ui_card(
                        "Plan Review Status",
                        [
                            ui_badge(ui_bind("/status_label"), variant = "warning"),
                            ui_key_value(ui_bind("/meta_summary")),
                        ],
                    ),
                    ui_markdown_review(
                        "Implementation Plan",
                        markdown = ui_bind("/plan"),
                        agent_notes = ui_bind("/notes"),
                        comments_path = "/draft_comments",
                        approve_event = ReviewEvent.APPROVE,
                        revise_event = ReviewEvent.REQUEST_CHANGES,
                    ),
                    ui_button("Abort Plan", event = ReviewEvent.ABORT, variant = "danger"),
                ]),
            ),
            on = [
                transition(event = ReviewEvent.APPROVE, target = S.approved),
                transition(
                    event = ReviewEvent.REQUEST_CHANGES,
                    target = S.revising,
                    assign = capture_comments,
                ),
                transition(event = ReviewEvent.ABORT, target = S.aborted),
            ],
        ),
        S.revising: state(
            always = outcome(target = S.human_review, assign = apply_revisions),
        ),
        S.approved: final_state(output = {"status": "approved"}),
        S.aborted: final_state(output = {"status": "aborted"}),
    },
)

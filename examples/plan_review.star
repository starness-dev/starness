# Pure headless Markdown plan review statechart (zero UI coupling in `.star`):
# - Run headlessly or with an auto-generated fallback picker:
#     starness run -i examples/plan_review.star
# - Attach the optional external view-sheet (`examples/plan_review.ui.json`) for rich TUI / Web Studio:
#     starness check examples/plan_review.star --ui examples/plan_review.ui.json
#     starness run -i examples/plan_review.star --ui examples/plan_review.ui.json
#     starness serve examples/plan_review.star --ui examples/plan_review.ui.json --bind 127.0.0.1:8080

ReviewEvent = enum("APPROVE", "REQUEST_CHANGES", "ABORT")

INITIAL_PLAN = """# Goal
Decouple workflow statecharts from UI presentation using external `*.ui.json` view-sheets.

## Architecture
- Keep `.star` workflows 100% pure headless statecharts with zero UI imports.
- Attach optional `*.ui.json` `json-render` view-sheets via `--ui` for both Web Studio and Ratatui TUI.
- Validate view-sheet state IDs and action events statically in `starness check --ui`.

## Verification
- Run `cargo clippy --workspace --all-targets -- -D warnings`.
- Run `cargo test --workspace` and `starness check examples/plan_review.star --ui examples/plan_review.ui.json`."""

INITIAL_NOTES = [
    {
        "heading": "Architecture",
        "severity": "info",
        "note": "View-sheet event validation runs during `starness check --ui` before any runtime execution.",
    },
    {
        "heading": "Verification",
        "severity": "warning",
        "note": "Confirm both interactive TUI tests (with and without `--ui`) and `starness serve` HTTP tests pass.",
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
        line_no = c.get("line", None)
        loc = heading + (" L" + str(line_no) if line_no != None else "")
        comment = str(c.get("comment", ""))
        if comment != "":
            additions.append("- [" + loc + "] Addressed: " + comment)
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
    id = "plan_review",
    description = "Pure headless Markdown plan review loop (pair with examples/plan_review.ui.json)",
    initial = S.human_review,
    context = PlanCtx(),
    states = {
        S.human_review: state(
            tags = ["review_gate"],
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
            # Brief simulated revision delay so state transitions and the live spinner are visible
            after = after(delay = 1200, target = S.human_review, assign = apply_revisions),
        ),
        S.approved: final_state(output = {"status": "approved"}),
        S.aborted: final_state(output = {"status": "aborted"}),
    },
)

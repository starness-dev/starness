# 04 — Multi-Harness Coding + Peer Review over ACP (`@starness/agents` + `DiffReview` UI)
#
# Caps off the progressive harness ladder by connecting TWO independent agent harnesses over ACP:
#   1. `gather_task`    — human submits a coding task.
#   2. `implementing`   — Harness #1 (`implementer`) implements the task and submits a typed
#                         `ImplementationReport` artifact via turn-scoped MCP.
#   3. `capturing_diff` — runs `git diff` deterministically via `@starness/exec`.
#   4. `reviewing`      — Harness #2 (`reviewer`, read-only) inspects the diff and submits typed
#                         `Finding`s + `ReviewVerdict`. Rejections automatically loop back to
#                         `implementing` (up to `max_attempts`).
#   5. `human_review`   — human inspects the diff + findings in `DiffReview` and chooses `APPROVE`,
#                         `REQUEST_CHANGES` (loops back with feedback), or `ABORT`.
#
# Swap `implementer` / `reviewer` freely between `agy_agent`, `claude_agent`, `codex_agent`, or
# `acp_agent` (e.g. another `starness acp` workflow) without changing any statechart logic.
#
# Try it:
#   starness run -i examples/04_coding_review_acp.star --ui examples/04_coding_review_acp.ui.json
#   starness serve examples/04_coding_review_acp.star --ui examples/04_coding_review_acp.ui.json --bind 127.0.0.1:8080

load(
    "@starness/agents",
    "ArtifactMode",
    "agy_agent",
    "artifact",
    "artifact_list",
    "artifact_tool",
    "claude_agent",
    "invoke_agent",
)
load("@starness/exec", "invoke_cmd")

ReviewEvent = enum("SUBMIT_TASK", "APPROVE", "REQUEST_CHANGES", "ABORT")

ImplementationReport = record(
    summary = str,
    files_changed = field(list[str], []),
)

Finding = record(
    severity = str,
    file = field(str, ""),
    message = str,
)

ReviewVerdict = record(
    approved = bool,
    summary = str,
)

ReviewCtx = record(
    task = field(str, ""),
    attempt = field(int, 0),
    max_attempts = field(int, 2),
    impl_summary = field(str, ""),
    files_changed = field(list[str], []),
    diff = field(str, ""),
    reviewer_approved = field(bool, False),
    reviewer_summary = field(str, ""),
    findings = field(list[Finding], []),
    finding_rows = field(list[list[str]], []),
    human_feedback = field(str, ""),
    status_label = field(str, "Awaiting Task"),
    error = field(str, ""),
)

S = state_space(
    "gather_task",
    "implementing",
    "capturing_diff",
    "reviewing",
    "human_review",
    "approved",
    "aborted",
    "failed",
)

# --- Harness bindings (swap any slot between agy_agent, claude_agent, codex_agent, acp_agent) ---

implementer = agy_agent(
    id = "implementer",
    args = ["acp", "--dangerously-skip-permissions"],
)

reviewer = claude_agent(
    id = "reviewer",
    permission_mode = "plan",
)

# --- Helpers ---

def has_task_text(_ctx: ReviewCtx, ev: dict) -> bool:
    return str(ev.get("text", "")).strip() != ""

def record_task(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    text = str(ev.get("text", "")).strip()
    return update(
        ctx,
        task = text,
        attempt = 1,
        status_label = "Implementing (attempt 1)",
    )

def impl_context(ctx: ReviewCtx) -> dict:
    return {
        "task": ctx.task,
        "attempt": ctx.attempt,
        "previous_findings": ctx.findings,
        "human_feedback": ctx.human_feedback,
    }

def build_impl_prompt(ctx: ReviewCtx) -> str:
    if ctx.attempt <= 1 and ctx.human_feedback == "":
        return "Implement the task in `<context>`, run relevant checks, and call `submit_implementation` with a concise summary and list of changed files."
    return "Address the review findings and human feedback in `<context>`, update the implementation, and call `submit_implementation`."

def record_implementation(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    rep = artifact(ev, "submit_implementation", ImplementationReport)
    return update(
        ctx,
        impl_summary = rep.summary,
        files_changed = rep.files_changed,
        status_label = "Capturing git diff",
    )

def record_diff(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    out = ev.get("output", {})
    raw = str(out.get("stdout", "")) if type(out) == "dict" else ""
    if raw.strip() == "":
        raw = "(no unstaged git diff)"
    return update(
        ctx,
        diff = raw,
        status_label = "Peer reviewing via ACP",
    )

def review_context(ctx: ReviewCtx) -> dict:
    return {
        "task": ctx.task,
        "attempt": ctx.attempt,
        "implementation_summary": ctx.impl_summary,
        "files_changed": ctx.files_changed,
        "diff": ctx.diff,
    }

def review_needs_revision(ctx: ReviewCtx, ev: dict) -> bool:
    verdict = artifact(ev, "submit_verdict", ReviewVerdict)
    return (not verdict.approved) and ctx.attempt < ctx.max_attempts

def findings_to_rows(items: list[Finding]) -> list[list[str]]:
    rows = []
    for f in items:
        rows.append([str(f.severity), str(f.file), str(f.message)])
    return rows

def record_review_and_retry(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    verdict = artifact(ev, "submit_verdict", ReviewVerdict)
    items = artifact_list(ev, "add_finding", Finding)
    next_attempt = ctx.attempt + 1
    return update(
        ctx,
        attempt = next_attempt,
        reviewer_approved = verdict.approved,
        reviewer_summary = verdict.summary,
        findings = items,
        finding_rows = findings_to_rows(items),
        status_label = "Revising after peer review (attempt " + str(next_attempt) + ")",
    )

def record_review_for_human(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    verdict = artifact(ev, "submit_verdict", ReviewVerdict)
    items = artifact_list(ev, "add_finding", Finding)
    label = "Peer review approved — awaiting human sign-off" if verdict.approved else "Max attempts reached — awaiting human decision"
    return update(
        ctx,
        reviewer_approved = verdict.approved,
        reviewer_summary = verdict.summary,
        findings = items,
        finding_rows = findings_to_rows(items),
        status_label = label,
    )

def record_human_feedback(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    comment = str(ev.get("comment", "")).strip()
    next_attempt = ctx.attempt + 1
    return update(
        ctx,
        attempt = next_attempt,
        human_feedback = comment,
        status_label = "Revising from human feedback (attempt " + str(next_attempt) + ")",
    )

def record_error(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    return update(
        ctx,
        error = str(ev.get("error", ev)),
        status_label = "Failed",
    )

chart: StateMachine = machine(
    id = "coding_review_acp",
    description = "Multi-harness coding and peer review over @starness/acp with DiffReview sign-off",
    initial = S.gather_task,
    context = ReviewCtx(),
    states = {
        S.gather_task: state(
            on = [
                transition(
                    event = ReviewEvent.SUBMIT_TASK,
                    guard = has_task_text,
                    target = S.implementing,
                    assign = record_task,
                ),
                transition(
                    event = ReviewEvent.ABORT,
                    target = S.aborted,
                ),
            ],
        ),
        S.implementing: state(
            invoke = invoke_agent(
                id = "implement_task",
                agent = implementer,
                prompt = build_impl_prompt,
                context = impl_context,
                tools = [
                    artifact_tool(
                        name = "submit_implementation",
                        description = "Submit a summary of the completed code changes.",
                        schema = ImplementationReport,
                        mode = ArtifactMode.replace,
                        required = True,
                    ),
                ],
                on_done = outcome(
                    target = S.capturing_diff,
                    assign = record_implementation,
                ),
                on_error = outcome(
                    target = S.failed,
                    assign = record_error,
                ),
            ),
        ),
        S.capturing_diff: state(
            invoke = invoke_cmd(
                id = "git_diff",
                cmd = ["git", "diff"],
                check = False,
                on_done = outcome(
                    target = S.reviewing,
                    assign = record_diff,
                ),
                on_error = outcome(
                    target = S.failed,
                    assign = record_error,
                ),
            ),
        ),
        S.reviewing: state(
            invoke = invoke_agent(
                id = "peer_review",
                agent = reviewer,
                prompt = "Review the diff in `<context>` for correctness, regressions, and edge cases (do not edit files). Call `add_finding` for each issue and `submit_verdict` when done.",
                context = review_context,
                tools = [
                    artifact_tool(
                        name = "add_finding",
                        description = "Record one code review finding.",
                        schema = Finding,
                        mode = ArtifactMode.append,
                        required = False,
                    ),
                    artifact_tool(
                        name = "submit_verdict",
                        description = "Submit your final approval verdict and summary.",
                        schema = ReviewVerdict,
                        mode = ArtifactMode.once,
                        required = True,
                    ),
                ],
                on_done = [
                    outcome(
                        guard = review_needs_revision,
                        target = S.implementing,
                        assign = record_review_and_retry,
                    ),
                    outcome(
                        target = S.human_review,
                        assign = record_review_for_human,
                    ),
                ],
                on_error = outcome(
                    target = S.failed,
                    assign = record_error,
                ),
            ),
        ),
        S.human_review: state(
            on = [
                transition(
                    event = ReviewEvent.APPROVE,
                    target = S.approved,
                ),
                transition(
                    event = ReviewEvent.REQUEST_CHANGES,
                    target = S.implementing,
                    assign = record_human_feedback,
                ),
                transition(
                    event = ReviewEvent.ABORT,
                    target = S.aborted,
                ),
            ],
        ),
        S.approved: final_state(),
        S.aborted: final_state(),
        S.failed: final_state(),
    },
)

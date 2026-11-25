# Multi-agent plan -> parallel review -> implementation -> test workflow.
#
# Harness bindings (`planner`, `agent1`, `agent2`, `implementer`) are declared once at the top
# so you can swap any slot between `agy_agent`, `claude_agent`, `codex_agent`, or `acp_agent`
# without renaming states, context fields, or reducers.

load(
    "@starness/agents",
    "ArtifactMode",
    "agy_agent",
    "artifact",
    "artifact_list",
    "artifact_tool",
    "claude_agent",
    "codex_agent",
    "invoke_agent",
)
load("@starness/exec", "invoke_cmd")

PlanArtifact = record(
    markdown = str,
    files_to_modify = field(list[str], []),
    reference_files = field(list[str], []),
)

ReviewVerdict = record(
    approved = bool,
    notes = str,
)

Finding = record(
    severity = str,
    file = field(str, ""),
    message = str,
)

ImplementationReport = record(
    summary = str,
    files_changed = list[str],
)

PipelineCtx = record(
    task = str,
    plan = field(PlanArtifact | None, None),
    agent1_review = field(ReviewVerdict | None, None),
    agent2_review = field(ReviewVerdict | None, None),
    agent2_findings = field(list[Finding], []),
    implementation = field(ImplementationReport | None, None),
    test_errors = field(str, ""),
    last_error = field(str, ""),
    plan_revisions = field(int, 0),
    max_plan_revisions = field(int, 2),
    impl_attempts = field(int, 0),
    max_impl_attempts = field(int, 2),
)

S = state_space(
    "planning",
    "reviewing",
    "agent1_branch",
    "agent1_review",
    "agent1_done",
    "agent2_branch",
    "agent2_review",
    "agent2_done",
    "implementing",
    "testing",
    "done",
    "escalated",
)

# --- Agent harness bindings (swap any slot here without touching the chart below) ---

planner = agy_agent(
    id = "planner",
)

agent1 = claude_agent(
    id = "agent1_reviewer",
    permission_mode = "plan",
)

agent2 = codex_agent(
    id = "agent2_reviewer",
    sandbox = "read-only",
)

implementer = agy_agent(
    id = "implementer",
    args = ["acp", "--dangerously-skip-permissions"],
)

# --- State context selectors (pushed directly into each agent's turn) ---

def planning_context(ctx: PipelineCtx) -> dict:
    return {
        "task": ctx.task,
        "plan_revision": ctx.plan_revisions,
        "previous_plan": ctx.plan,
        "agent1_review": ctx.agent1_review,
        "agent2_review": ctx.agent2_review,
        "agent2_findings": ctx.agent2_findings,
    }

def implementation_context(ctx: PipelineCtx) -> dict:
    return {
        "task": ctx.task,
        "approved_plan": ctx.plan,
        "agent1_review": ctx.agent1_review,
        "agent2_review": ctx.agent2_review,
        "agent2_findings": ctx.agent2_findings,
        "impl_attempt": ctx.impl_attempts,
        "test_errors": ctx.test_errors,
    }

# --- Prompts ---

def build_plan_prompt(ctx: PipelineCtx) -> str:
    if ctx.plan_revisions == 0:
        return "Write a Markdown implementation plan for the task in `<context>` (do not edit files) and call `submit_plan`."
    return "Your previous plan was rejected. Address `agent1_review`, `agent2_review`, and `agent2_findings` from `<context>` (do not edit files) and call `submit_plan`."

def build_implement_prompt(ctx: PipelineCtx) -> str:
    if ctx.impl_attempts == 0:
        return "Implement `approved_plan` from `<context>` in the workspace (do not run `cargo test` yourself; the `testing` state runs it next), then call `submit_implementation`."
    return "Tests failed on the previous attempt. Fix `test_errors` from `<context>` in the workspace (do not run `cargo test` yourself; the `testing` state runs it next), then call `submit_implementation`."

# --- Guards & `update(ctx, ...)` reducers ---

def all_reviews_approved(ctx: PipelineCtx, event: dict) -> bool:
    return (
        ctx.agent1_review != None
        and ctx.agent1_review.approved
        and ctx.agent2_review != None
        and ctx.agent2_review.approved
    )

def save_plan(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(
        ctx,
        plan = artifact(event, "submit_plan", PlanArtifact),
        agent1_review = None,
        agent2_review = None,
        agent2_findings = [],
        last_error = "",
    )

def save_agent2_review(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(
        ctx,
        agent2_review = artifact(event, "submit_review", ReviewVerdict),
        agent2_findings = artifact_list(event, "report_finding", Finding),
    )

def bump_plan_revision(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(
        ctx,
        last_error = "plan rejected by reviewers",
        plan_revisions = ctx.plan_revisions + 1,
    )

def save_implementation(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(
        ctx,
        implementation = artifact(event, "submit_implementation", ImplementationReport),
        test_errors = "",
        last_error = "",
    )

def record_test_failure(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    err = str(event.get("error", "test suite failed"))
    return update(
        ctx,
        test_errors = err,
        last_error = err,
        impl_attempts = ctx.impl_attempts + 1,
    )

def record_escalation_error(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return update(ctx, last_error = str(event.get("error", "workflow escalated")))

chart: StateMachine = machine(
    id = "multi_agent_acp",
    description = "Plan, parallel dual-agent review, implementation & test pipeline",
    initial = S.planning,
    context = PipelineCtx(
        task = "Typed Starlark statechart workflows across AI coding agents",
    ),
    states = {
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                context = planning_context,
                prompt = build_plan_prompt,
                tools = [
                    artifact_tool(
                        "submit_plan",
                        description = "Submit the Markdown implementation plan and file lists.",
                        schema = PlanArtifact,
                        mode = ArtifactMode.replace,
                        required = True,
                        validate = lambda _ctx, plan: "`markdown` must not be empty" if plan.markdown.strip() == "" else None,
                    ),
                ],
                on_done = outcome(target = S.reviewing, assign = save_plan),
                on_error = outcome(target = S.escalated, assign = record_escalation_error),
            ),
        ),
        # Orthogonal parallel fan-out: both reviewers finish, then `on_done` joins both verdicts.
        S.reviewing: parallel_state(
            on_done = [
                outcome(target = S.implementing, guard = all_reviews_approved),
                outcome(
                    target = S.planning,
                    guard = lambda ctx, _ev: ctx.plan_revisions < ctx.max_plan_revisions,
                    assign = bump_plan_revision,
                ),
                outcome(target = S.escalated, assign = record_escalation_error),
            ],
            states = {
                S.agent1_branch: compound_state(
                    initial = S.agent1_review,
                    states = {
                        S.agent1_review: state(
                            invoke = invoke_agent(
                                agent = agent1,
                                context = lambda ctx: {"task": ctx.task, "plan": ctx.plan},
                                prompt = "Review the design & ergonomics of `plan` in `<context>` (do not edit files), then call `submit_review`.",
                                tools = [
                                    artifact_tool(
                                        "submit_review",
                                        description = "Submit review verdict and notes.",
                                        schema = ReviewVerdict,
                                        required = True,
                                    ),
                                ],
                                on_done = outcome(
                                    target = S.agent1_done,
                                    assign = lambda ctx, ev: update(
                                        ctx,
                                        agent1_review = artifact(ev, "submit_review", ReviewVerdict),
                                    ),
                                ),
                                on_error = outcome(target = S.escalated, assign = record_escalation_error),
                            ),
                        ),
                        S.agent1_done: final_state(),
                    },
                ),
                S.agent2_branch: compound_state(
                    initial = S.agent2_review,
                    states = {
                        S.agent2_review: state(
                            invoke = invoke_agent(
                                agent = agent2,
                                context = lambda ctx: {"task": ctx.task, "plan": ctx.plan},
                                prompt = "Audit edge cases in `plan` from `<context>` (`report_finding`), then call `submit_review`.",
                                tools = [
                                    artifact_tool(
                                        "report_finding",
                                        description = "Record an edge-case finding (may be called multiple times).",
                                        schema = Finding,
                                        mode = ArtifactMode.append,
                                        required = False,
                                    ),
                                    artifact_tool(
                                        "submit_review",
                                        description = "Submit audit review verdict and notes.",
                                        schema = ReviewVerdict,
                                        required = True,
                                    ),
                                ],
                                on_done = outcome(target = S.agent2_done, assign = save_agent2_review),
                                on_error = outcome(target = S.escalated, assign = record_escalation_error),
                            ),
                        ),
                        S.agent2_done: final_state(),
                    },
                ),
            },
        ),
        S.implementing: state(
            invoke = invoke_agent(
                agent = implementer,
                context = implementation_context,
                prompt = build_implement_prompt,
                tools = [
                    artifact_tool(
                        "submit_implementation",
                        description = "Submit summary and list of modified files once edits are complete.",
                        schema = ImplementationReport,
                        required = True,
                        validate = lambda _ctx, r: "`files_changed` must not be empty" if len(r.files_changed) == 0 else None,
                    ),
                ],
                on_done = outcome(target = S.testing, assign = save_implementation),
                on_error = outcome(target = S.escalated, assign = record_escalation_error),
            ),
        ),
        S.testing: state(
            invoke = invoke_cmd(
                cmd = ["cargo", "test", "--workspace", "--offline"],
                timeout_ms = 300000,
                on_done = S.done,
                on_error = [
                    outcome(
                        target = S.implementing,
                        guard = lambda ctx, _ev: ctx.impl_attempts < ctx.max_impl_attempts,
                        assign = record_test_failure,
                    ),
                    outcome(target = S.escalated, assign = record_escalation_error),
                ],
            ),
        ),
        S.done: final_state(output = {"status": "verified"}),
        S.escalated: final_state(output = {"status": "needs_human_review"}),
    },
)

# Multi-agent orchestration across Antigravity (`agy`), Claude Code (`claude`), and Codex (`codex`)!
# - `planning`:   `agy acp` produces a concise architecture plan
# - `reviewing`:  `parallel_state` fans out concurrently to `claude` AND `codex` to review the plan!
# - `testing`:    `cargo check` verifies the workspace compiles cleanly (loops back on failure)

load("@starchart/agents", "agy_agent", "claude_agent", "codex_agent", "invoke_agent")
load("@starchart/exec", "invoke_cmd")

PipelineCtx = record(
    task = str,
    plan = str,
    claude_notes = str,
    codex_notes = str,
    test_errors = str,
    attempts = int,
    max_attempts = int,
)

S = state_space(
    "planning",
    "reviewing",
    "claude_branch",
    "claude_review",
    "claude_done",
    "codex_branch",
    "codex_review",
    "codex_done",
    "testing",
    "done",
    "escalated",
)

planner = agy_agent(
    id = "agy_planner",
    model = "gemini-3.8-flash-medium",
)

claude = claude_agent(
    id = "claude_reviewer",
    permission_mode = "plan",
)

codex = codex_agent(
    id = "codex_auditor",
    sandbox = "read-only",
)

def build_plan_prompt(ctx: PipelineCtx) -> str:
    return "Reply in 3 short bullet points summarizing an approach for: " + ctx.task + " (do not edit files)."

def save_plan(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return PipelineCtx(
        task = ctx.task,
        plan = str(event.get("output", {}).get("text", "")),
        claude_notes = ctx.claude_notes,
        codex_notes = ctx.codex_notes,
        test_errors = ctx.test_errors,
        attempts = ctx.attempts,
        max_attempts = ctx.max_attempts,
    )

def build_claude_prompt(ctx: PipelineCtx) -> str:
    return "In 2 concise sentences, review the ergonomics of this plan (do not edit files):\n" + ctx.plan

def save_claude_notes(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return PipelineCtx(
        task = ctx.task,
        plan = ctx.plan,
        claude_notes = str(event.get("output", {}).get("text", "")),
        codex_notes = ctx.codex_notes,
        test_errors = ctx.test_errors,
        attempts = ctx.attempts,
        max_attempts = ctx.max_attempts,
    )

def build_codex_prompt(ctx: PipelineCtx) -> str:
    return "In 2 concise sentences, check edge cases in this plan (do not edit files):\n" + ctx.plan

def save_codex_notes(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return PipelineCtx(
        task = ctx.task,
        plan = ctx.plan,
        claude_notes = ctx.claude_notes,
        codex_notes = str(event.get("output", {}).get("text", "")),
        test_errors = ctx.test_errors,
        attempts = ctx.attempts,
        max_attempts = ctx.max_attempts,
    )

def can_retry(ctx: PipelineCtx, event: dict) -> bool:
    return ctx.attempts < ctx.max_attempts

def record_test_failure(ctx: PipelineCtx, event: dict) -> PipelineCtx:
    return PipelineCtx(
        task = ctx.task,
        plan = ctx.plan,
        claude_notes = ctx.claude_notes,
        codex_notes = ctx.codex_notes,
        test_errors = str(event.get("error", "check failure")),
        attempts = ctx.attempts + 1,
        max_attempts = ctx.max_attempts,
    )

chart: StateMachine = machine(
    id = "multi_agent_acp",
    description = "Antigravity + Claude Code + Codex parallel review & verification pipeline",
    initial = S.planning,
    context = PipelineCtx(
        task = "Typed Starlark statechart workflows across AI coding agents",
        plan = "",
        claude_notes = "",
        codex_notes = "",
        test_errors = "",
        attempts = 0,
        max_attempts = 2,
    ),
    states = {
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = build_plan_prompt,
                on_done = outcome(target = S.reviewing, assign = save_plan),
                on_error = S.escalated,
            ),
        ),
        # Orthogonal parallel fan-out: Claude Code AND Codex review the plan simultaneously!
        S.reviewing: parallel_state(
            on_done = S.testing,
            states = {
                S.claude_branch: compound_state(
                    initial = S.claude_review,
                    states = {
                        S.claude_review: state(
                            invoke = invoke_agent(
                                agent = claude,
                                prompt = build_claude_prompt,
                                on_done = outcome(target = S.claude_done, assign = save_claude_notes),
                                on_error = S.escalated,
                            ),
                        ),
                        S.claude_done: final_state(),
                    },
                ),
                S.codex_branch: compound_state(
                    initial = S.codex_review,
                    states = {
                        S.codex_review: state(
                            invoke = invoke_agent(
                                agent = codex,
                                prompt = build_codex_prompt,
                                on_done = outcome(target = S.codex_done, assign = save_codex_notes),
                                on_error = S.escalated,
                            ),
                        ),
                        S.codex_done: final_state(),
                    },
                ),
            },
        ),
        S.testing: state(
            invoke = invoke_cmd(
                cmd = ["cargo", "check", "--quiet", "--offline"],
                on_done = S.done,
                on_error = [
                    outcome(target = S.planning, guard = can_retry, assign = record_test_failure),
                    outcome(target = S.escalated),
                ],
            ),
        ),
        S.done: final_state(output = {"status": "verified"}),
        S.escalated: final_state(output = {"status": "needs_human_review"}),
    },
)

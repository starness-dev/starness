# Lightweight single-agent CI test & autofix loop (`@starness/exec` + `@starness/agents`):
# - `testing`: runs `ctx.test_cmd` (`cargo test --workspace --offline` by default)
# - `fixing`: on test failure, pushes the error output to an ACP coding agent, collects a
#             structured `FixReport` artifact, and re-runs `testing` up to `max_attempts`

load("@starness/agents", "agy_agent", "artifact", "artifact_tool", "invoke_agent")
load("@starness/exec", "invoke_cmd")

FixReport = record(
    root_cause = str,
    files_changed = list[str],
)

AutofixCtx = record(
    test_cmd = field(list[str], ["cargo", "test", "--workspace", "--offline"]),
    attempts = field(int, 0),
    max_attempts = field(int, 3),
    last_failure = field(str, ""),
    fixes = field(list[FixReport], []),
)

S = state_space("testing", "fixing", "green", "escalated")

fixer = agy_agent(
    id = "fixer",
    args = ["acp", "--dangerously-skip-permissions"],
)

def record_failure(ctx: AutofixCtx, event: dict) -> AutofixCtx:
    return update(
        ctx,
        attempts = ctx.attempts + 1,
        last_failure = str(event.get("error", "test command failed")),
    )

def save_fix(ctx: AutofixCtx, event: dict) -> AutofixCtx:
    report = artifact(event, "submit_fix", FixReport)
    return update(ctx, fixes = ctx.fixes + [report])

chart: StateMachine = machine(
    id = "ci_autofix_acp",
    description = "Run test suite and invoke an ACP coding agent to repair failures up to max_attempts",
    initial = S.testing,
    context = AutofixCtx(),
    states = {
        S.testing: state(
            invoke = invoke_cmd(
                id = "test_suite",
                cmd = lambda ctx: ctx.test_cmd,
                timeout_ms = 300000,
                on_done = S.green,
                on_error = [
                    outcome(
                        target = S.fixing,
                        guard = lambda ctx, _ev: ctx.attempts < ctx.max_attempts,
                        assign = record_failure,
                    ),
                    outcome(target = S.escalated, assign = record_failure),
                ],
            ),
        ),
        S.fixing: state(
            invoke = invoke_agent(
                agent = fixer,
                context = lambda ctx: {
                    "attempt": ctx.attempts,
                    "test_cmd": ctx.test_cmd,
                    "failure_output": ctx.last_failure,
                    "prior_fixes": ctx.fixes,
                },
                prompt = "Inspect `failure_output` in `<context>`, fix the failing code (do not run the test suite yourself), and call `submit_fix`.",
                tools = [
                    artifact_tool(
                        "submit_fix",
                        description = "Record the root cause and modified files for this fix attempt.",
                        schema = FixReport,
                        validate = lambda _ctx, r: "`files_changed` must not be empty" if len(r.files_changed) == 0 else None,
                    ),
                ],
                on_done = outcome(target = S.testing, assign = save_fix),
                on_error = S.escalated,
            ),
        ),
        S.green: final_state(output = {"status": "green"}),
        S.escalated: final_state(output = {"status": "escalated"}),
    },
)

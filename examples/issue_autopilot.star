# Flagship 3-tier AI composition example (`@starness/decide` -> `@starness/llm` -> `@starness/acp`):
# - Tier 1 (`@starness/decide`): Fast calibrated System-1 triage classifies incoming issues by category
#   (`BUG`, `DOCS`, `FEATURE`) and reproducibility. Ambiguous or refused issues pause for a human.
# - Tier 2 (`@starness/llm`): Single-shot structured extraction produces either a typed `DocsReply`
#   (for documentation questions) or a typed `FixPlan` `Record` (root cause, target files, test command).
# - Tier 3 (`@starness/acp`): Only high-confidence reproducible bugs escalate to an autonomous coding
#   agent (`agy_agent`) with a turn-scoped `submit_patch` `artifact_tool`, followed by deterministic
#   test verification via `@starness/exec`.
#
# Inspect topology:
#   starness check examples/issue_autopilot.star
#   starness diagram examples/issue_autopilot.star

load(
    "@starness/acp",
    "ArtifactMode",
    "agy_agent",
    "artifact",
    "artifact_tool",
    "invoke_agent",
)
load(
    "@starness/decide",
    "DecisionProvider",
    "choice",
    "invoke_decision",
    "is_yes",
    "jev",
    "noul",
    "prob",
    "refused",
    "selected",
)
load("@starness/exec", "invoke_cmd")
load("@starness/llm", "LlmModel", "invoke_llm", "llm_model", "llm_structured")

IssueKind = enum("BUG", "DOCS", "FEATURE")
TriageEvent = enum("APPROVE_FIX", "CLOSE_AS_DOCS")

DocsReply = record(
    answer = str,
    doc_links = list[str],
)

FixPlan = record(
    root_cause = str,
    target_files = list[str],
    test_cmd = list[str],
)

PatchReport = record(
    summary = str,
    files_changed = list[str],
)

AutopilotCtx = record(
    issue_id = field(str, "ISSUE-108"),
    title = field(str, "normalize_base_endpoint drops /v1 when endpoint lacks trailing slash"),
    body = field(str, "Passing endpoint='http://localhost:11434/v1' posts to /chat/completions instead of /v1/chat/completions."),
    decision_endpoint = field(str, ""),
    llm_model_name = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    min_confidence = field(float, 0.80),
    kind = field(str, ""),
    reproducible = field(bool, False),
    docs_answer = field(str, ""),
    root_cause = field(str, ""),
    target_files = field(list[str], []),
    test_cmd = field(list[str], ["cargo", "test", "--workspace", "--offline", "-q"]),
    patch_summary = field(str, ""),
    reason = field(str, ""),
)

coder = agy_agent(id = "autopilot_coder", timeout_ms = 300000)

S = state_space(
    "triaging",
    "awaiting_human_triage",
    "drafting_docs_reply",
    "planning_fix",
    "implementing_fix",
    "verifying_tests",
    "resolved",
    "failed",
)

def select_decider(ctx: AutopilotCtx) -> DecisionProvider:
    if ctx.decision_endpoint != "":
        return jev(endpoint = ctx.decision_endpoint)
    return jev()

def select_llm(ctx: AutopilotCtx) -> LlmModel:
    return llm_model(
        ctx.llm_model_name,
        endpoint = ctx.llm_endpoint if ctx.llm_endpoint != "" else None,
        temperature = 0.1,
    )

def save_triage(ctx: AutopilotCtx, ev: dict) -> AutopilotCtx:
    return update(
        ctx,
        kind = selected(ev, "kind") or "UNKNOWN",
        reproducible = is_yes(ev, "reproducible"),
    )

def is_confident_bug(ctx: AutopilotCtx, ev: dict) -> bool:
    return (
        not refused(ev, "kind")
        and selected(ev, "kind") == IssueKind.BUG.value
        and prob(ev, "kind") >= ctx.min_confidence
        and is_yes(ev, "reproducible")
    )

def is_confident_docs(ctx: AutopilotCtx, ev: dict) -> bool:
    return (
        not refused(ev, "kind")
        and selected(ev, "kind") == IssueKind.DOCS.value
        and prob(ev, "kind") >= ctx.min_confidence
    )

def save_docs_reply(ctx: AutopilotCtx, ev: dict) -> AutopilotCtx:
    reply = llm_structured(ev, DocsReply)
    return update(ctx, docs_answer = reply.answer)

def save_fix_plan(ctx: AutopilotCtx, ev: dict) -> AutopilotCtx:
    plan = llm_structured(ev, FixPlan)
    return update(
        ctx,
        root_cause = plan.root_cause,
        target_files = plan.target_files,
        test_cmd = plan.test_cmd if len(plan.test_cmd) > 0 else ctx.test_cmd,
    )

def save_patch(ctx: AutopilotCtx, ev: dict) -> AutopilotCtx:
    report = artifact(ev, "submit_patch", PatchReport)
    return update(ctx, patch_summary = report.summary)

chart: StateMachine = machine(
    id = "issue_autopilot",
    description = "3-tier issue autopilot: @starness/decide triage -> @starness/llm plan -> @starness/acp fix",
    initial = S.triaging,
    context = AutopilotCtx(),
    states = {
        S.triaging: state(
            invoke = invoke_decision(
                id = "triage_issue",
                provider = select_decider,
                evidence = lambda ctx: {
                    "issue_id": ctx.issue_id,
                    "title": ctx.title,
                    "body": ctx.body,
                },
                questions = {
                    "kind": choice(
                        "What kind of issue is this?",
                        options = {
                            IssueKind.BUG: "Concrete software defect or incorrect behavior.",
                            IssueKind.DOCS: "Usage question or documentation clarification.",
                            IssueKind.FEATURE: "New feature request or architectural proposal.",
                        },
                        optional = True,
                    ),
                    "reproducible": noul(
                        "Are the reproduction details concrete enough to fix autonomously?",
                        yes = "Clear reproduction steps, file/symbol, or concrete expected vs actual behavior.",
                        no = "Vague report missing reproduction details.",
                        threshold = 0.80,
                    ),
                },
                on_done = [
                    outcome(
                        target = S.planning_fix,
                        guard = is_confident_bug,
                        assign = save_triage,
                    ),
                    outcome(
                        target = S.drafting_docs_reply,
                        guard = is_confident_docs,
                        assign = save_triage,
                    ),
                    outcome(
                        target = S.awaiting_human_triage,
                        assign = save_triage,
                    ),
                ],
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "triage_failed"))),
                ),
            ),
        ),
        S.awaiting_human_triage: state(
            on = [
                transition(event = TriageEvent.APPROVE_FIX, target = S.planning_fix),
                transition(event = TriageEvent.CLOSE_AS_DOCS, target = S.drafting_docs_reply),
            ],
        ),
        S.drafting_docs_reply: state(
            invoke = invoke_llm(
                id = "draft_docs_reply",
                model = select_llm,
                system = "Draft a clear, concise answer with relevant documentation links.",
                prompt = lambda ctx: ctx.title + "\n\n" + ctx.body,
                schema = DocsReply,
                on_done = outcome(target = S.resolved, assign = save_docs_reply),
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "docs_llm_failed"))),
                ),
            ),
        ),
        S.planning_fix: state(
            invoke = invoke_llm(
                id = "plan_fix",
                model = select_llm,
                system = "Analyze the bug report and return a concise root cause, target files, and test command.",
                prompt = lambda ctx: ctx.title + "\n\n" + ctx.body,
                schema = FixPlan,
                on_done = outcome(target = S.implementing_fix, assign = save_fix_plan),
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "plan_llm_failed"))),
                ),
            ),
        ),
        S.implementing_fix: state(
            invoke = invoke_agent(
                id = "implement_fix",
                agent = coder,
                prompt = lambda ctx: (
                    "Implement a minimal fix for " + ctx.issue_id + ": " + ctx.title +
                    "\nRoot cause hypothesis: " + ctx.root_cause +
                    "\nCall `submit_patch` when finished."
                ),
                context = lambda ctx: {
                    "target_files": ctx.target_files,
                    "test_cmd": ctx.test_cmd,
                },
                tools = [
                    artifact_tool(
                        "submit_patch",
                        description = "Record summary and changed files after applying the fix.",
                        schema = PatchReport,
                        mode = ArtifactMode.replace,
                        required = True,
                    ),
                ],
                on_done = outcome(target = S.verifying_tests, assign = save_patch),
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "agent_failed"))),
                ),
            ),
        ),
        S.verifying_tests: state(
            invoke = invoke_cmd(
                id = "run_tests",
                cmd = lambda ctx: ctx.test_cmd,
                on_done = S.resolved,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "tests_failed"))),
                ),
            ),
        ),
        S.resolved: final_state(output = {"status": "resolved"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

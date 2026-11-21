# Zero-compile release-notes synthesis & breaking-change approval statechart using `@starness/llm`:
# - Compiles a typed Starlark `ReleaseSummary` `record(...)` (including nested `ChangeEntry` records
#   and `RiskLevel` enums) into a constrained-decoding JSON Schema (`JsonSpec`) via `genai`.
# - Streams live reasoning/token chunks into `TraceHub` (`--trace -` or `/trace` SSE) while
#   rehydrating the model's output back into a validated Starlark `Record` via `llm_structured(...)`.
# - Automatically publishes routine releases, or pauses in `awaiting_release_approval` whenever
#   the model flags breaking or `HIGH`-risk changes.

load(
    "@starness/llm",
    "LlmModel",
    "invoke_llm",
    "llm_model",
    "llm_structured",
    "llm_text",
)
load("@starness/exec", "invoke_cmd")
load("@starness/secrets", "secret")

RiskLevel = enum("LOW", "MEDIUM", "HIGH")

ReviewEvent = enum("APPROVE_RELEASE", "REJECT_RELEASE")

ChangeEntry = record(
    area = str,
    summary = str,
    risk = RiskLevel,
)

ReleaseSummary = record(
    headline = str,
    breaking = bool,
    highlights = list[ChangeEntry],
    migration_note = field(str, ""),
)

ReleaseCtx = record(
    version = field(str, "v0.4.0"),
    commits = field(
        list[str],
        [
            "feat(llm): add @starness/llm single-shot text & Record extraction via genai",
            "feat(secrets): scrub resolved API keys from traces and error diagnostics",
            "docs(readme): document @starness/llm standard module",
        ],
    ),
    model = field(str, "openai::gpt-4o-mini"),
    llm_endpoint = field(str, ""),
    headline = field(str, ""),
    breaking = field(bool, False),
    high_risk_count = field(int, 0),
    migration_note = field(str, ""),
    raw_json = field(str, ""),
    approved_by = field(str, "auto"),
    reason = field(str, ""),
    publish_cmd = field(list[str], ["sh", "-c", "echo publishing release notes"]),
)

S = state_space(
    "drafting_notes",
    "awaiting_release_approval",
    "publishing",
    "published",
    "aborted",
    "failed",
)

def select_model(ctx: ReleaseCtx) -> LlmModel:
    if ctx.llm_endpoint != "":
        return llm_model(
            ctx.model,
            endpoint = ctx.llm_endpoint,
            temperature = 0.1,
        )
    return llm_model(
        ctx.model,
        api_key = secret("OPENAI_API_KEY", optional = True),
        temperature = 0.1,
    )

def requires_human_review(_ctx: ReleaseCtx, ev: dict) -> bool:
    notes = llm_structured(ev, ReleaseSummary)
    if notes.breaking:
        return True
    for item in notes.highlights:
        if item.risk == RiskLevel.HIGH:
            return True
    return False

def save_release_summary(ctx: ReleaseCtx, ev: dict) -> ReleaseCtx:
    notes = llm_structured(ev, ReleaseSummary)
    high_risk = [item for item in notes.highlights if item.risk == RiskLevel.HIGH]
    return update(
        ctx,
        headline = notes.headline,
        breaking = notes.breaking,
        high_risk_count = len(high_risk),
        migration_note = notes.migration_note,
        raw_json = llm_text(ev),
    )

chart: StateMachine = machine(
    id = "release_notes_llm",
    description = "Structured release-notes generator and breaking-change gate powered by @starness/llm",
    initial = S.drafting_notes,
    context = ReleaseCtx(),
    states = {
        S.drafting_notes: state(
            invoke = invoke_llm(
                id = "draft_release_notes",
                model = select_model,
                system = "You are a release-engineering assistant. Summarize git commits into structured release notes.",
                prompt = lambda ctx: "Draft structured release notes for version " + ctx.version + ".",
                context = lambda ctx: {
                    "version": ctx.version,
                    "commits": ctx.commits,
                },
                schema = ReleaseSummary,
                stream = True,
                timeout_ms = 30000,
                on_done = [
                    outcome(
                        target = S.awaiting_release_approval,
                        guard = requires_human_review,
                        assign = save_release_summary,
                    ),
                    outcome(
                        target = S.publishing,
                        assign = save_release_summary,
                    ),
                ],
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "llm_failed"))),
                ),
            ),
        ),
        S.awaiting_release_approval: state(
            on = [
                transition(
                    event = ReviewEvent.APPROVE_RELEASE,
                    target = S.publishing,
                    assign = lambda ctx, ev: update(ctx, approved_by = str(ev.get("reviewer", "release-lead"))),
                ),
                transition(
                    event = ReviewEvent.REJECT_RELEASE,
                    target = S.aborted,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("reason", "rejected_by_reviewer"))),
                ),
            ],
        ),
        S.publishing: state(
            invoke = invoke_cmd(
                id = "publish_release",
                cmd = lambda ctx: ctx.publish_cmd,
                on_done = S.published,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "publish_failed"))),
                ),
            ),
        ),
        S.published: final_state(output = {"status": "published"}),
        S.aborted: final_state(output = {"status": "aborted"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

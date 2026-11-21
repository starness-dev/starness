# Zero-compile parallel multi-model code review statechart using Harel `parallel_state` + `@starness/llm`:
# - Fans out a PR diff simultaneously across two independent LLM providers (`reviewer_a` and `reviewer_b`)
#   in orthogonal parallel regions, each constrained to a typed `ReviewVerdict` Starlark `record(...)`.
# - Converges automatically via `parallel_state(..., on_done = ...)` once both regions reach their
#   `final_state`: if both approve with zero blockers, auto-approves; otherwise pauses in
#   `awaiting_human_review` for an explicit `APPROVE_MERGE` or `REQUEST_CHANGES` event.
#
# Run:
#   starness check examples/parallel_review_llm.star
#   starness diagram examples/parallel_review_llm.star
#   starness run examples/parallel_review_llm.star --events-stdin --trace -

load(
    "@starness/llm",
    "LlmModel",
    "invoke_llm",
    "llm_model",
    "llm_structured",
)

Verdict = enum("APPROVE", "CONCERN", "BLOCK")
HumanEvent = enum("APPROVE_MERGE", "REQUEST_CHANGES")

ReviewVerdict = record(
    verdict = Verdict,
    summary = str,
    blockers = list[str],
)

ReviewCtx = record(
    pr_title = field(str, "Add prompt caching and multi-turn tool calling to @starness/llm"),
    diff = field(
        str,
        "+ pub fn parse_optional_cache(raw: Option<&Value>) -> anyhow::Result<Option<CacheControl>>\n" +
        "+ pub fn parse_chat_message(raw: &Value) -> anyhow::Result<ChatMessage>",
    ),
    model_a = field(str, "gpt-6-luna"),
    model_b = field(str, "gemini-2.5-flash"),
    endpoint_a = field(str, ""),
    endpoint_b = field(str, ""),
    verdict_a = field(str, ""),
    summary_a = field(str, ""),
    blockers_a = field(list[str], []),
    verdict_b = field(str, ""),
    summary_b = field(str, ""),
    blockers_b = field(list[str], []),
    decision_note = field(str, ""),
)

S = state_space(
    "reviewing",
    "region_a",
    "running_a",
    "done_a",
    "region_b",
    "running_b",
    "done_b",
    "awaiting_human_review",
    "approved",
    "changes_requested",
)

def make_model_a(ctx: ReviewCtx) -> LlmModel:
    return llm_model(
        ctx.model_a,
        endpoint = ctx.endpoint_a if ctx.endpoint_a != "" else None,
        temperature = 0.1,
    )

def make_model_b(ctx: ReviewCtx) -> LlmModel:
    return llm_model(
        ctx.model_b,
        endpoint = ctx.endpoint_b if ctx.endpoint_b != "" else None,
        temperature = 0.1,
    )

def save_review_a(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    res = llm_structured(ev, ReviewVerdict)
    return update(
        ctx,
        verdict_a = res.verdict.value,
        summary_a = res.summary,
        blockers_a = res.blockers,
    )

def save_review_b(ctx: ReviewCtx, ev: dict) -> ReviewCtx:
    res = llm_structured(ev, ReviewVerdict)
    return update(
        ctx,
        verdict_b = res.verdict.value,
        summary_b = res.summary,
        blockers_b = res.blockers,
    )

def unanimous_approval(ctx: ReviewCtx, _ev: dict) -> bool:
    return (
        ctx.verdict_a == Verdict.APPROVE.value
        and ctx.verdict_b == Verdict.APPROVE.value
        and len(ctx.blockers_a) == 0
        and len(ctx.blockers_b) == 0
    )

chart: StateMachine = machine(
    id = "parallel_review_llm",
    description = "Parallel multi-model PR review consensus statechart powered by @starness/llm",
    initial = S.reviewing,
    context = ReviewCtx(),
    states = {
        S.reviewing: parallel_state(
            states = {
                S.region_a: compound_state(
                    initial = S.running_a,
                    states = {
                        S.running_a: state(
                            invoke = invoke_llm(
                                id = "reviewer_a",
                                model = make_model_a,
                                system = "You are a strict Rust security & correctness reviewer.",
                                prompt = lambda ctx: "Review PR `" + ctx.pr_title + "`:\n\n" + ctx.diff,
                                schema = ReviewVerdict,
                                stream = True,
                                on_done = outcome(target = S.done_a, assign = save_review_a),
                                on_error = outcome(
                                    target = S.done_a,
                                    assign = lambda ctx, ev: update(
                                        ctx,
                                        verdict_a = Verdict.CONCERN.value,
                                        summary_a = "reviewer_a error: " + str(ev.get("error", "unknown")),
                                    ),
                                ),
                            ),
                        ),
                        S.done_a: final_state(),
                    },
                ),
                S.region_b: compound_state(
                    initial = S.running_b,
                    states = {
                        S.running_b: state(
                            invoke = invoke_llm(
                                id = "reviewer_b",
                                model = make_model_b,
                                system = "You are a pragmatic API design & maintainability reviewer.",
                                prompt = lambda ctx: "Review PR `" + ctx.pr_title + "`:\n\n" + ctx.diff,
                                schema = ReviewVerdict,
                                stream = True,
                                on_done = outcome(target = S.done_b, assign = save_review_b),
                                on_error = outcome(
                                    target = S.done_b,
                                    assign = lambda ctx, ev: update(
                                        ctx,
                                        verdict_b = Verdict.CONCERN.value,
                                        summary_b = "reviewer_b error: " + str(ev.get("error", "unknown")),
                                    ),
                                ),
                            ),
                        ),
                        S.done_b: final_state(),
                    },
                ),
            },
            on_done = [
                outcome(
                    target = S.approved,
                    guard = unanimous_approval,
                    assign = lambda ctx, _ev: update(ctx, decision_note = "Unanimous multi-model approval"),
                ),
                outcome(
                    target = S.awaiting_human_review,
                    assign = lambda ctx, _ev: update(ctx, decision_note = "Disagreement or blockers detected"),
                ),
            ],
        ),
        S.awaiting_human_review: state(
            on = [
                transition(
                    event = HumanEvent.APPROVE_MERGE,
                    target = S.approved,
                    assign = lambda ctx, ev: update(ctx, decision_note = str(ev.get("note", "Approved by maintainer"))),
                ),
                transition(
                    event = HumanEvent.REQUEST_CHANGES,
                    target = S.changes_requested,
                    assign = lambda ctx, ev: update(ctx, decision_note = str(ev.get("note", "Changes requested"))),
                ),
            ],
        ),
        S.approved: final_state(output = {"status": "approved"}),
        S.changes_requested: final_state(output = {"status": "changes_requested"}),
    },
)

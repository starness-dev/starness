# Zero-compile MCP issue enrichment & sync statechart using `@starness/mcp` + `@starness/llm`:
# - Fetches an issue from any Streamable HTTP MCP server (`mcp_http_server` + `invoke_mcp`),
#   passing a late-bound `@starness/secrets` Bearer token that is scrubbed from traces and errors.
# - Synthesizes structured labels, priority, and a concise triage comment via `@starness/llm`
#   (`schema = IssueEnrichment`).
# - Writes the enriched comment and labels back to the tracker deterministically via `invoke_mcp`.
#
# Run:
#   starness check examples/mcp_issue_sync.star
#   starness run examples/mcp_issue_sync.star \
#     --context '{"mcp_url":"http://127.0.0.1:9000/mcp","issue_id":"ENG-42"}' \
#     --trace -

load("@starness/llm", "LlmModel", "invoke_llm", "llm_model", "llm_structured")
load("@starness/mcp", "McpServer", "invoke_mcp", "mcp_http_server")
load("@starness/secrets", "secret")

Priority = enum("P0", "P1", "P2", "P3")

IssueEnrichment = record(
    priority = Priority,
    labels = list[str],
    comment = str,
)

SyncCtx = record(
    mcp_url = field(str, "http://127.0.0.1:9000/mcp"),
    issue_id = field(str, "ENG-42"),
    model = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    issue_title = field(str, ""),
    issue_body = field(str, ""),
    priority = field(str, ""),
    labels = field(list[str], []),
    comment = field(str, ""),
    reason = field(str, ""),
)

S = state_space(
    "fetching_issue",
    "enriching",
    "posting_update",
    "synced",
    "failed",
)

def build_tracker_server(ctx: SyncCtx) -> McpServer:
    return mcp_http_server(
        id = "tracker",
        url = ctx.mcp_url,
        headers = {
            "Authorization": secret("MCP_TRACKER_TOKEN", prefix = "Bearer ", optional = True),
        },
        timeout_ms = 15000,
    )

def select_model(ctx: SyncCtx) -> LlmModel:
    return llm_model(
        ctx.model,
        endpoint = ctx.llm_endpoint if ctx.llm_endpoint != "" else None,
        temperature = 0.1,
    )

def store_issue(ctx: SyncCtx, ev: dict) -> SyncCtx:
    data = ev.get("output", {}).get("data", {})
    if type(data) == "dict":
        return update(
            ctx,
            issue_title = str(data.get("title", ctx.issue_id)),
            issue_body = str(data.get("body", "")),
        )
    return update(ctx, issue_title = ctx.issue_id, issue_body = str(data))

def store_enrichment(ctx: SyncCtx, ev: dict) -> SyncCtx:
    res = llm_structured(ev, IssueEnrichment)
    return update(
        ctx,
        priority = res.priority.value,
        labels = res.labels,
        comment = res.comment,
    )

chart: StateMachine = machine(
    id = "mcp_issue_sync",
    description = "Fetch, enrich with @starness/llm, and sync tracker issues via @starness/mcp",
    initial = S.fetching_issue,
    context = SyncCtx(),
    states = {
        S.fetching_issue: state(
            invoke = invoke_mcp(
                id = "fetch_issue",
                server = build_tracker_server,
                tool = "get_issue",
                arguments = lambda ctx: {"id": ctx.issue_id},
                on_done = outcome(target = S.enriching, assign = store_issue),
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "mcp_fetch_failed"))),
                ),
            ),
        ),
        S.enriching: state(
            invoke = invoke_llm(
                id = "enrich_issue",
                model = select_model,
                system = "Classify the issue priority, choose 1-3 short labels, and draft a concise triage comment.",
                prompt = lambda ctx: "Issue " + ctx.issue_id + ": " + ctx.issue_title + "\n\n" + ctx.issue_body,
                schema = IssueEnrichment,
                stream = True,
                on_done = outcome(target = S.posting_update, assign = store_enrichment),
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "llm_enrich_failed"))),
                ),
            ),
        ),
        S.posting_update: state(
            invoke = invoke_mcp(
                id = "update_issue",
                server = build_tracker_server,
                tool = "update_issue",
                arguments = lambda ctx: {
                    "id": ctx.issue_id,
                    "priority": ctx.priority,
                    "labels": ctx.labels,
                    "comment": ctx.comment,
                },
                on_done = S.synced,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "mcp_update_failed"))),
                ),
            ),
        ),
        S.synced: final_state(output = {"status": "synced"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

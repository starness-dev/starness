# 03 — Coding Harness Connected to a Remote MCP Server (`@starness/mcp` + `@starness/decide` + `invoke_all`)
#
# Builds on `02_coding_harness.star` by connecting a remote Streamable HTTP MCP server (defaulting
# to Linear at `https://mcp.linear.app/mcp`, or any MCP URL via `--context '{"mcp_url": "..."}'`).
# - Zero tokens in `.star`: run `starness auth login https://mcp.linear.app/mcp` once to store
#   OAuth 2.1 PKCE credentials in the OS Keychain (`starness.mcp.oauth`).
# - Exposes both `bash` (local shell via `@starness/exec`) and `mcp_call` (remote MCP tool via
#   `@starness/mcp`) to the LLM — and executes mixed `bash` + `mcp_call` batches concurrently via
#   `invoke_all`, all gated by `@starness/decide` (`ALLOW` / `ASK_HUMAN` / `DENY`).
#
# Try it with Linear (OAuth 2.1):
#   starness auth login https://mcp.linear.app/mcp
#   starness run -i examples/03_coding_harness_mcp.star --ui examples/03_coding_harness_mcp.ui.json
#
# Or with any zero-auth remote MCP (e.g. DeepWiki):
#   starness run -i examples/03_coding_harness_mcp.star --ui examples/03_coding_harness_mcp.ui.json \
#     --context '{"mcp_url": "https://mcp.deepwiki.com/mcp", "mcp_name": "deepwiki"}'

load(
    "@starness/decide",
    "DecisionProvider",
    "choice",
    "confidence",
    "invoke_decision",
    "is_yes",
    "noul",
    "openai_decision",
    "prob",
    "refused",
    "selected",
)
load("@starness/exec", "exec_result", "exec_spec")
load(
    "@starness/llm",
    "LlmMessage",
    "LlmModel",
    "ToolCall",
    "ToolChoice",
    "assistant_msg",
    "attachments_from",
    "has_tool_calls",
    "invoke_llm",
    "llm_model",
    "llm_tool_calls",
    "tool_def",
    "tool_result",
    "user_msg",
)
load("@starness/mcp", "mcp_http_server", "mcp_result")
load("@starness/secrets", "secret")

HarnessEvent = enum("PROMPT", "APPROVE", "DENY", "INTERRUPT", "EXIT")
PermAction = enum("ALLOW", "ASK_HUMAN", "DENY")

BashArgs = record(
    command = str,
)

McpCallArgs = record(
    tool = str,
    arguments = field(dict, {}),
)

McpHarnessCtx = record(
    model = field(str, "gpt-6-luna"),
    decision_model = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    decision_endpoint = field(str, ""),
    min_auto_prob = field(float, 0.85),
    mcp_name = field(str, "linear"),
    mcp_url = field(str, "https://mcp.linear.app/mcp"),
    cwd = field(str, ""),
    messages = field(list[LlmMessage], []),
    pending_calls = field(list[ToolCall], []),
    approval_rows = field(list[list[str]], []),
    last_decision = field(str, ""),
    last_decision_prob = field(float, 0.0),
    tool_runs = field(int, 0),
    tool_errors = field(int, 0),
    error = field(str, ""),
)

S = state_space(
    "gather_input",
    "working",
    "thinking",
    "classifying_permission",
    "awaiting_approval",
    "acting",
    "done",
    "failed",
)

SYSTEM_PROMPT = """You are a careful coding assistant with two tools:
1. `bash`: execute a shell command in the local workspace.
2. `mcp_call`: call a tool on the connected remote MCP server (e.g., Linear tools like `list_teams`, `list_issues`, `get_issue`, `create_issue`, `create_comment`, or whatever tools the configured MCP server exposes).

Prefer read-only inspection first. When a command or MCP mutation is denied, explain why and propose a safer alternative."""

MAX_OUTPUT_CHARS = 8000

def truncate_text(raw: str, limit: int) -> str:
    if len(raw) <= limit:
        return raw
    half = limit // 2
    omitted = len(raw) - limit
    return raw[:half] + "\n... [truncated " + str(omitted) + " chars] ...\n" + raw[len(raw) - half:]

def is_exit_prompt(_ctx: McpHarnessCtx, ev: dict) -> bool:
    return str(ev.get("text", "")).strip().lower() == "/exit"

def has_prompt(ctx: McpHarnessCtx, ev: dict) -> bool:
    txt = str(ev.get("text", "")).strip()
    return (txt != "" or len(ev.get("files", [])) > 0) and not is_exit_prompt(ctx, ev)

def select_model(ctx: McpHarnessCtx) -> LlmModel:
    return llm_model(
        ctx.model,
        api_key = secret("OPENAI_API_KEY", optional = True),
        endpoint = ctx.llm_endpoint if ctx.llm_endpoint != "" else None,
    )

def select_decision_provider(ctx: McpHarnessCtx) -> DecisionProvider:
    return openai_decision(
        model = ctx.decision_model,
        endpoint = ctx.decision_endpoint if ctx.decision_endpoint != "" else None,
        api_key = secret("OPENAI_API_KEY", optional = True),
    )

def append_user_prompt(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    return update(
        ctx,
        messages = ctx.messages + [
            user_msg(
                str(ev.get("text", "")).strip(),
                attachments = attachments_from(ev.get("files", [])),
            ),
        ],
        pending_calls = [],
        approval_rows = [],
    )

def queue_tool_calls(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = llm_tool_calls(ev),
        approval_rows = [],
    )

def record_final_reply(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = [],
        approval_rows = [],
    )

def describe_call(call: ToolCall) -> str:
    if call.name == "bash":
        return str(call.args.get("command", ""))
    if call.name == "mcp_call":
        return str(call.args.get("tool", "")) + "(" + str(call.args.get("arguments", {})) + ")"
    return str(call.args)

def build_permission_evidence(ctx: McpHarnessCtx) -> dict:
    actions = []
    for call in ctx.pending_calls:
        actions.append({
            "tool": call.name,
            "detail": describe_call(call),
        })
    return {
        "cwd": ctx.cwd or ".",
        "mcp_server": ctx.mcp_name,
        "mcp_url": ctx.mcp_url,
        "actions": actions,
    }

def is_confident_choice(ctx: McpHarnessCtx, ev: dict) -> bool:
    if refused(ev, "action"):
        return False
    conf = confidence(ev, "action")
    conf_ok = True if conf == None else conf >= ctx.min_auto_prob
    return prob(ev, "action") >= ctx.min_auto_prob and conf_ok

def decision_allows(ctx: McpHarnessCtx, ev: dict) -> bool:
    return (
        is_confident_choice(ctx, ev)
        and selected(ev, "action") == PermAction.ALLOW.value
        and not is_yes(ev, "destructive")
    )

def decision_denies(ctx: McpHarnessCtx, ev: dict) -> bool:
    if is_yes(ev, "destructive") and prob(ev, "destructive") >= ctx.min_auto_prob:
        return True
    return is_confident_choice(ctx, ev) and selected(ev, "action") == PermAction.DENY.value

def record_decision_allow(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    return update(
        ctx,
        last_decision = "ALLOW",
        last_decision_prob = prob(ev, "action"),
    )

def record_decision_deny(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    denied_msgs = []
    for call in ctx.pending_calls:
        denied_msgs.append(tool_result(
            call,
            {
                "ok": False,
                "denied_by": "decision_policy",
                "error": "Blocked by @starness/decide permission policy.",
            },
        ))
    return update(
        ctx,
        messages = ctx.messages + denied_msgs,
        pending_calls = [],
        approval_rows = [],
        last_decision = "DENY",
        last_decision_prob = prob(ev, "action"),
        tool_errors = ctx.tool_errors + len(denied_msgs),
    )

def prepare_human_approval(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    verdict = selected(ev, "action") or "ASK_HUMAN"
    p = prob(ev, "action")
    reason = verdict + " (p=" + str(p) + ")"
    rows = []
    for call in ctx.pending_calls:
        rows.append([call.name, describe_call(call), reason])
    return update(
        ctx,
        approval_rows = rows,
        last_decision = verdict,
        last_decision_prob = p,
    )

def prepare_approval_on_decide_error(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    rows = []
    for call in ctx.pending_calls:
        rows.append([call.name, describe_call(call), "decide unavailable — manual review"])
    return update(
        ctx,
        approval_rows = rows,
        last_decision = "FALLBACK_ASK_HUMAN",
        last_decision_prob = 0.0,
    )

def record_human_denial(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    reason = str(ev.get("reason", "Denied by user during tool approval."))
    denied_msgs = []
    for call in ctx.pending_calls:
        denied_msgs.append(tool_result(
            call,
            {"ok": False, "denied_by": "human", "error": reason},
        ))
    return update(
        ctx,
        messages = ctx.messages + denied_msgs,
        pending_calls = [],
        approval_rows = [],
        tool_errors = ctx.tool_errors + len(denied_msgs),
    )

def build_mixed_batch(ctx: McpHarnessCtx) -> list[dict]:
    mcp_srv = mcp_http_server(
        id = ctx.mcp_name,
        url = ctx.mcp_url,
        timeout_ms = 30000,
    )
    items = []
    for call in ctx.pending_calls:
        if call.name == "mcp_call":
            items.append(batch_item(
                id = call.id,
                kind = "mcp",
                spec = {
                    "server": mcp_srv,
                    "tool": str(call.args.get("tool", "")),
                    "arguments": call.args.get("arguments", {}),
                    "check_tool_error": False,
                },
            ))
        else:
            items.append(batch_item(
                id = call.id,
                kind = "exec",
                spec = exec_spec(
                    cmd = ["sh", "-c", str(call.args.get("command", ""))],
                    cwd = ctx.cwd if ctx.cwd != "" else None,
                    timeout_ms = 30000,
                    check = False,
                ),
            ))
    return items

def record_batch_results(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    batch = ev.get("output", {})
    results = batch.get("results", []) if type(batch) == "dict" else []
    by_id = {}
    for item in results:
        if type(item) == "dict" and "id" in item:
            by_id[str(item["id"])] = item
    new_msgs = []
    errs = 0
    for call in ctx.pending_calls:
        item = by_id.get(call.id)
        if item == None:
            errs += 1
            new_msgs.append(tool_result(call, {"ok": False, "error": "missing batch result"}))
        elif item.get("ok", False):
            if call.name == "mcp_call":
                res = mcp_result(item)
                if res.is_error:
                    errs += 1
                new_msgs.append(tool_result(
                    call,
                    {
                        "tool": res.tool,
                        "is_error": res.is_error,
                        "text": truncate_text(res.text, MAX_OUTPUT_CHARS),
                    },
                ))
            else:
                res = exec_result(item)
                if not res.ok:
                    errs += 1
                new_msgs.append(tool_result(
                    call,
                    {
                        "code": res.code,
                        "stdout": truncate_text(res.stdout, MAX_OUTPUT_CHARS),
                        "stderr": truncate_text(res.stderr, MAX_OUTPUT_CHARS),
                    },
                ))
        else:
            errs += 1
            new_msgs.append(tool_result(
                call,
                {"ok": False, "error": str(item.get("error", "tool failed"))},
            ))
    return update(
        ctx,
        messages = ctx.messages + new_msgs,
        pending_calls = [],
        approval_rows = [],
        tool_runs = ctx.tool_runs + len(ctx.pending_calls),
        tool_errors = ctx.tool_errors + errs,
    )

def record_interrupt(ctx: McpHarnessCtx, _ev: dict) -> McpHarnessCtx:
    return update(
        ctx,
        pending_calls = [],
        approval_rows = [],
    )

def record_error(ctx: McpHarnessCtx, ev: dict) -> McpHarnessCtx:
    return update(ctx, error = str(ev.get("error", "Coding MCP harness error")))

chart: StateMachine = machine(
    id = "coding_harness_mcp",
    description = "Coding harness connected to a remote MCP server (OAuth 2.1 / Streamable HTTP)",
    initial = S.gather_input,
    context = McpHarnessCtx(),
    states = {
        S.gather_input: state(
            on = [
                transition(
                    event = HarnessEvent.PROMPT,
                    guard = is_exit_prompt,
                    target = S.done,
                ),
                transition(
                    event = HarnessEvent.PROMPT,
                    guard = has_prompt,
                    target = S.working,
                    assign = append_user_prompt,
                ),
                transition(
                    event = HarnessEvent.EXIT,
                    target = S.done,
                ),
            ],
        ),
        S.working: compound_state(
            initial = S.thinking,
            on = [
                transition(
                    event = HarnessEvent.INTERRUPT,
                    target = S.gather_input,
                    assign = record_interrupt,
                ),
                transition(
                    event = HarnessEvent.EXIT,
                    target = S.done,
                ),
            ],
            states = {
                S.thinking: state(
                    invoke = invoke_llm(
                        id = "coding_mcp_turn",
                        model = select_model,
                        system = SYSTEM_PROMPT,
                        messages = lambda ctx: ctx.messages,
                        tools = [
                            tool_def(
                                name = "bash",
                                description = "Execute a shell command in the local workspace.",
                                schema = BashArgs,
                            ),
                            tool_def(
                                name = "mcp_call",
                                description = "Call a tool on the connected remote MCP server (e.g. Linear `list_teams`, `list_issues`, `get_issue`, `create_issue`).",
                                schema = McpCallArgs,
                            ),
                        ],
                        tool_choice = ToolChoice.auto,
                        on_done = [
                            outcome(
                                guard = lambda _ctx, ev: has_tool_calls(ev),
                                target = S.classifying_permission,
                                assign = queue_tool_calls,
                            ),
                            outcome(
                                target = S.gather_input,
                                assign = record_final_reply,
                            ),
                        ],
                        on_error = outcome(
                            target = S.failed,
                            assign = record_error,
                        ),
                    ),
                ),
                S.classifying_permission: state(
                    invoke = invoke_decision(
                        id = "mcp_permission_gate",
                        provider = select_decision_provider,
                        evidence = build_permission_evidence,
                        questions = {
                            "action": choice(
                                "How should this batch of local shell and remote MCP calls be gated?",
                                options = {
                                    PermAction.ALLOW: "Strictly read-only local inspection or read-only MCP queries (list_*, get_*, search_*, read_*) with no mutations.",
                                    PermAction.ASK_HUMAN: "Modifies local files, runs git commit/push, or mutates remote state via MCP (create_*, update_*, delete_*, post_*).",
                                    PermAction.DENY: "Destructive, dangerous, or credential-exfiltrating commands.",
                                },
                            ),
                            "destructive": noul(
                                "Could any action irreversibly delete data, exfiltrate secrets, or harm external systems?",
                                yes = "Deletes critical data, exposes credentials, or runs untrusted remote code.",
                                no = "Safe or reversible workspace/MCP operation.",
                                threshold = 0.85,
                            ),
                        },
                        on_done = [
                            outcome(
                                guard = decision_allows,
                                target = S.acting,
                                assign = record_decision_allow,
                            ),
                            outcome(
                                guard = decision_denies,
                                target = S.thinking,
                                assign = record_decision_deny,
                            ),
                            outcome(
                                target = S.awaiting_approval,
                                assign = prepare_human_approval,
                            ),
                        ],
                        on_error = outcome(
                            target = S.awaiting_approval,
                            assign = prepare_approval_on_decide_error,
                        ),
                    ),
                ),
                S.awaiting_approval: state(
                    on = [
                        transition(
                            event = HarnessEvent.APPROVE,
                            target = S.acting,
                        ),
                        transition(
                            event = HarnessEvent.DENY,
                            target = S.thinking,
                            assign = record_human_denial,
                        ),
                    ],
                ),
                S.acting: state(
                    invoke = invoke_all(
                        id = "run_mixed_tools",
                        items = build_mixed_batch,
                        max_concurrency = 4,
                        on_done = outcome(
                            target = S.thinking,
                            assign = record_batch_results,
                        ),
                        on_error = outcome(
                            target = S.failed,
                            assign = record_error,
                        ),
                    ),
                ),
            },
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)

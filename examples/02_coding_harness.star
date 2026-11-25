# 02 — Coding Harness with `@starness/decide` Permission Gate
#
# Builds on `01_chat_harness.star` by adding:
# - `@starness/exec` (`bash` tool) + `invoke_all` bounded-concurrency parallel tool execution.
# - `@starness/decide` (`decision-rs` / `jev`) permission classifier inside `working`:
#     1. `thinking` invokes the LLM with the `bash` tool.
#     2. `classifying_permission` evaluates the pending command batch with calibrated probabilities
#        (`ALLOW` / `ASK_HUMAN` / `DENY` + `destructive` noul check):
#          - High-confidence `ALLOW` (non-destructive) -> executes immediately in `acting`.
#          - High-confidence `DENY` (or destructive)   -> rejects and feeds refusal back to the LLM.
#          - `ASK_HUMAN`, low-confidence, refused, or decision error -> fail-safe pause in
#            `awaiting_approval` so a human can `APPROVE` or `DENY` the batch.
#     3. `acting` runs all approved tool calls concurrently via `invoke_all` with output truncation.
# - `INTERRUPT` on `working` cancels any in-flight LLM stream, decision call, or tool batch cleanly.
#
# Try it:
#   starness run -i examples/02_coding_harness.star --ui examples/02_coding_harness.ui.json
#   starness serve examples/02_coding_harness.star --ui examples/02_coding_harness.ui.json --bind 127.0.0.1:8080

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
    "LlmModel",
    "ToolCall",
    "ToolChoice",
    "assistant_msg",
    "has_tool_calls",
    "invoke_llm",
    "llm_model",
    "llm_text",
    "llm_tool_calls",
    "tool_def",
    "tool_result",
    "user_msg",
)
load("@starness/secrets", "secret")

HarnessEvent = enum("PROMPT", "APPROVE", "DENY", "INTERRUPT", "EXIT")
PermAction = enum("ALLOW", "ASK_HUMAN", "DENY")

BashArgs = record(
    command = str,
)

CodingCtx = record(
    model = field(str, "gpt-6-luna"),
    decision_model = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    decision_endpoint = field(str, ""),
    min_auto_prob = field(float, 0.85),
    cwd = field(str, ""),
    messages = field(list[dict], []),
    pending_calls = field(list[ToolCall], []),
    approval_rows = field(list[list[str]], []),
    last_decision = field(str, ""),
    last_decision_prob = field(float, 0.0),
    last_reply = field(str, ""),
    turns = field(int, 0),
    tool_runs = field(int, 0),
    tool_errors = field(int, 0),
    reason = field(str, ""),
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

SYSTEM_PROMPT = """You are a careful, concise coding assistant with access to a `bash` tool.
Inspect the repository with read-only commands before proposing edits.
When a command is denied by the permission policy or user, explain why and offer a safer alternative."""

MAX_OUTPUT_CHARS = 8000

def truncate_text(raw: str, limit: int) -> str:
    if len(raw) <= limit:
        return raw
    half = limit // 2
    omitted = len(raw) - limit
    return raw[:half] + "\n... [truncated " + str(omitted) + " chars] ...\n" + raw[len(raw) - half:]

def prompt_text(ev: dict) -> str:
    return str(ev.get("text", ev.get("message", ""))).strip()

def is_exit_prompt(ctx: CodingCtx, ev: dict) -> bool:
    return prompt_text(ev).lower() in ["/exit", "/quit", "exit", "quit"]

def has_prompt(ctx: CodingCtx, ev: dict) -> bool:
    txt = prompt_text(ev)
    return txt != "" and not is_exit_prompt(ctx, ev)

def select_model(ctx: CodingCtx) -> LlmModel:
    if ctx.llm_endpoint != "":
        return llm_model(ctx.model, endpoint = ctx.llm_endpoint)
    return llm_model(ctx.model, api_key = secret("OPENAI_API_KEY", optional = True))

def select_decision_provider(ctx: CodingCtx) -> DecisionProvider:
    if ctx.decision_endpoint != "":
        return openai_decision(
            model = ctx.decision_model,
            endpoint = ctx.decision_endpoint,
            api_key = secret("OPENAI_API_KEY", optional = True),
        )
    return openai_decision(model = ctx.decision_model)

def append_user_prompt(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(
        ctx,
        messages = ctx.messages + [user_msg(prompt_text(ev))],
        pending_calls = [],
        approval_rows = [],
        turns = ctx.turns + 1,
    )

def queue_tool_calls(ctx: CodingCtx, ev: dict) -> CodingCtx:
    calls = llm_tool_calls(ev)
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = calls,
        approval_rows = [],
        last_reply = llm_text(ev),
    )

def record_final_reply(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = [],
        approval_rows = [],
        last_reply = llm_text(ev),
    )

def build_permission_evidence(ctx: CodingCtx) -> dict:
    commands = []
    for call in ctx.pending_calls:
        commands.append({
            "tool": call.name,
            "command": str(call.args.get("command", "")),
        })
    return {
        "cwd": ctx.cwd or ".",
        "commands": commands,
    }

def is_confident_choice(ctx: CodingCtx, ev: dict) -> bool:
    if refused(ev, "action"):
        return False
    conf = confidence(ev, "action")
    conf_ok = True if conf == None else conf >= ctx.min_auto_prob
    return prob(ev, "action") >= ctx.min_auto_prob and conf_ok

def decision_allows(ctx: CodingCtx, ev: dict) -> bool:
    return (
        is_confident_choice(ctx, ev)
        and selected(ev, "action") == PermAction.ALLOW.value
        and not is_yes(ev, "destructive")
    )

def decision_denies(ctx: CodingCtx, ev: dict) -> bool:
    if is_yes(ev, "destructive") and prob(ev, "destructive") >= ctx.min_auto_prob:
        return True
    return is_confident_choice(ctx, ev) and selected(ev, "action") == PermAction.DENY.value

def record_decision_allow(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(
        ctx,
        last_decision = "ALLOW",
        last_decision_prob = prob(ev, "action"),
    )

def record_decision_deny(ctx: CodingCtx, ev: dict) -> CodingCtx:
    p = prob(ev, "action")
    denied_msgs = []
    for call in ctx.pending_calls:
        denied_msgs.append(tool_result(
            call,
            {
                "ok": False,
                "denied_by": "decision_policy",
                "error": "Blocked by @starness/decide permission policy (high-risk/destructive command).",
            },
        ))
    return update(
        ctx,
        messages = ctx.messages + denied_msgs,
        pending_calls = [],
        approval_rows = [],
        last_decision = "DENY",
        last_decision_prob = p,
        tool_errors = ctx.tool_errors + len(denied_msgs),
    )

def prepare_human_approval(ctx: CodingCtx, ev: dict) -> CodingCtx:
    verdict = selected(ev, "action") or "ASK_HUMAN"
    p = prob(ev, "action")
    reason = verdict + " (p=" + str(p) + ")"
    rows = []
    for call in ctx.pending_calls:
        rows.append([call.name, str(call.args.get("command", "")), reason])
    return update(
        ctx,
        approval_rows = rows,
        last_decision = verdict,
        last_decision_prob = p,
    )

def prepare_approval_on_decide_error(ctx: CodingCtx, ev: dict) -> CodingCtx:
    reason = "decide unavailable — manual review required"
    rows = []
    for call in ctx.pending_calls:
        rows.append([call.name, str(call.args.get("command", "")), reason])
    return update(
        ctx,
        approval_rows = rows,
        last_decision = "FALLBACK_ASK_HUMAN",
        last_decision_prob = 0.0,
    )

def record_human_denial(ctx: CodingCtx, ev: dict) -> CodingCtx:
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

def build_exec_batch(ctx: CodingCtx) -> list[dict]:
    items = []
    for call in ctx.pending_calls:
        items.append(batch_item(
            id = call.id,
            spec = exec_spec(
                cmd = ["sh", "-c", str(call.args.get("command", ""))],
                cwd = ctx.cwd if ctx.cwd != "" else None,
                timeout_ms = 30000,
                check = False,
            ),
        ))
    return items

def record_batch_results(ctx: CodingCtx, ev: dict) -> CodingCtx:
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
                {"ok": False, "error": str(item.get("error", "command failed"))},
            ))
    return update(
        ctx,
        messages = ctx.messages + new_msgs,
        pending_calls = [],
        approval_rows = [],
        tool_runs = ctx.tool_runs + len(ctx.pending_calls),
        tool_errors = ctx.tool_errors + errs,
    )

def record_interrupt(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(
        ctx,
        pending_calls = [],
        approval_rows = [],
        reason = str(ev.get("reason", "interrupted by user")),
    )

def record_exit(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(ctx, reason = str(ev.get("reason", "user_exit")))

def record_error(ctx: CodingCtx, ev: dict) -> CodingCtx:
    return update(ctx, reason = str(ev.get("error", ev)))

chart: StateMachine = machine(
    id = "coding_harness",
    description = "02: Multi-turn coding harness with @starness/decide permission gate and invoke_all",
    initial = S.gather_input,
    context = CodingCtx(),
    states = {
        S.gather_input: state(
            on = [
                transition(
                    event = HarnessEvent.PROMPT,
                    guard = is_exit_prompt,
                    target = S.done,
                    assign = record_exit,
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
                    assign = record_exit,
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
                    assign = record_exit,
                ),
            ],
            states = {
                S.thinking: state(
                    invoke = invoke_llm(
                        id = "coding_turn",
                        model = select_model,
                        system = SYSTEM_PROMPT,
                        messages = lambda ctx: ctx.messages,
                        tools = [
                            tool_def(
                                name = "bash",
                                description = "Execute a shell command in the workspace.",
                                schema = BashArgs,
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
                        id = "permission_gate",
                        provider = select_decision_provider,
                        evidence = build_permission_evidence,
                        questions = {
                            "action": choice(
                                "How should this batch of shell commands be gated?",
                                options = {
                                    PermAction.ALLOW: "Strictly read-only workspace inspection (ls, cat, rg, git status/diff/log, cargo check/test) with no file writes or side effects.",
                                    PermAction.ASK_HUMAN: "Modifies files, installs packages, runs git commit/push, or executes unfamiliar commands requiring human confirmation.",
                                    PermAction.DENY: "Destructive, dangerous, or credential-exfiltrating commands (rm -rf /, curl | sh, dumping secrets/keys).",
                                },
                            ),
                            "destructive": noul(
                                "Could any command irreversibly delete data, exfiltrate credentials, or harm the host?",
                                yes = "Deletes critical data, exposes secrets, or executes untrusted remote scripts.",
                                no = "Safe or reversible workspace operation.",
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
                        id = "run_bash_batch",
                        items = build_exec_batch,
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

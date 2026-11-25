# Interactive multi-turn coding harness powered by `@starness/llm` + `invoke_all` + `@starness/exec`:
# - Maintains full conversation history in typed `SessionCtx.messages`, round-tripping assistant
#   messages via `assistant_msg(ev)` so tool calls, reasoning blocks, and thought signatures are
#   preserved across turns and across providers (`genai`).
# - Uses a Safe-First + Batch Human Permission Gate inside `working`:
#     1. `thinking` classifies incoming tool calls into `safe_calls` (read-only inspection) and
#        `risky_calls` (mutating / shell writes / unknown commands).
#     2. `acting_safe` executes all `safe_calls` concurrently via `invoke_all` (bounded parallelism)
#        with per-call error isolation and structured `{code, stdout, stderr, hint}` recovery.
#     3. If `risky_calls` remain, `awaiting_approval` pauses with all safe outputs already visible
#        in `Conversation` so a human can `APPROVE` or `DENY` the batch at once.
#     4. `acting_approved` runs approved `risky_calls` via `invoke_all` before returning to `thinking`.
# - A single `INTERRUPT` event on `working` cancels any in-flight LLM stream, parallel batch, or
#   pending approval cleanly and returns to `gather_input`.
#
# Try interactively:
#   starness serve examples/coding_session_llm.star --ui examples/coding_session_llm.ui.json --bind 127.0.0.1:8080
#   starness run -i examples/coding_session_llm.star --ui examples/coding_session_llm.ui.json

load("@starness/exec", "exec_spec")
load(
    "@starness/llm",
    "CachePolicy",
    "LlmMessage",
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

SessionEvent = enum("PROMPT", "APPROVE", "DENY", "INTERRUPT", "EXIT")

BashArgs = record(
    command = str,
)

SessionCtx = record(
    model = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    cwd = field(str, ""),
    messages = field(list[LlmMessage], []),
    safe_calls = field(list[ToolCall], []),
    risky_calls = field(list[ToolCall], []),
    approval_rows = field(list[list[str]], []),
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
    "acting_safe",
    "awaiting_approval",
    "acting_approved",
    "done",
    "failed",
)

SAFE_PREFIXES = [
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "find",
    "rg",
    "grep",
    "uname",
    "sw_vers",
    "whoami",
    "which",
    "echo",
    "printf",
    "date",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git ls-files",
    "git rev-parse",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "rustc",
]

def is_safe_segment(seg: str) -> bool:
    s = seg.strip()
    if s == "":
        return True
    # Block redirections, subshells, and backgrounding in auto-approved commands
    for forbidden in [">", "`", "$(", "&&", "||"]:
        if forbidden in s:
            return False
    for prefix in SAFE_PREFIXES:
        if s == prefix or s.startswith(prefix + " "):
            return True
    return False

def classify_bash_command(cmd: str) -> (bool, str):
    trimmed = cmd.strip()
    if trimmed == "":
        return (False, "empty command")
    # Allow simple read-only pipelines / semicolon lists only if every segment is safe
    if ">" in trimmed and not ("2>/dev/null" in trimmed and trimmed.replace("2>/dev/null", "").find(">") == -1):
        return (False, "contains output redirection")
    cleaned = trimmed.replace("2>/dev/null", "").replace("2>&1", "")
    for part in cleaned.split(";"):
        for pipe_seg in part.split("|"):
            if not is_safe_segment(pipe_seg):
                return (False, "mutating or non-allowlisted command")
    return (True, "read-only command")

def partition_calls(calls: list[ToolCall]) -> (list[ToolCall], list[ToolCall], list[list[str]]):
    safe = []
    risky = []
    rows = []
    for call in calls:
        cmd = str(call.args.get("command", ""))
        ok, why = classify_bash_command(cmd)
        if ok:
            safe.append(call)
        else:
            risky.append(call)
            rows.append([call.name, cmd, why])
    return (safe, risky, rows)

def select_model(ctx: SessionCtx) -> LlmModel:
    if ctx.llm_endpoint != "":
        return llm_model(
            ctx.model,
            endpoint = ctx.llm_endpoint,
        )
    return llm_model(
        ctx.model,
        api_key = secret("OPENAI_API_KEY", optional = True),
    )

def is_exit_prompt(_ctx: SessionCtx, ev: dict) -> bool:
    return str(ev.get("text", "")).strip() in ["/exit", "/quit"]

def has_prompt_text(_ctx: SessionCtx, ev: dict) -> bool:
    txt = str(ev.get("text", "")).strip()
    return txt != "" and txt not in ["/exit", "/quit"]

def append_user_prompt(ctx: SessionCtx, ev: dict) -> SessionCtx:
    txt = str(ev.get("text", "")).strip()
    return update(
        ctx,
        messages = ctx.messages + [user_msg(txt)],
        safe_calls = [],
        risky_calls = [],
        approval_rows = [],
    )

def has_any_safe_calls(_ctx: SessionCtx, ev: dict) -> bool:
    if not has_tool_calls(ev):
        return False
    safe, _risky, _rows = partition_calls(llm_tool_calls(ev))
    return len(safe) > 0

def has_only_risky_calls(_ctx: SessionCtx, ev: dict) -> bool:
    if not has_tool_calls(ev):
        return False
    safe, risky, _rows = partition_calls(llm_tool_calls(ev))
    return len(safe) == 0 and len(risky) > 0

def record_classified_calls(ctx: SessionCtx, ev: dict) -> SessionCtx:
    safe, risky, rows = partition_calls(llm_tool_calls(ev))
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        safe_calls = safe,
        risky_calls = risky,
        approval_rows = rows,
    )

def record_final_reply(ctx: SessionCtx, ev: dict) -> SessionCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        safe_calls = [],
        risky_calls = [],
        approval_rows = [],
        last_reply = llm_text(ev),
        turns = ctx.turns + 1,
    )

def build_batch_for_calls(calls: list[ToolCall], cwd: str) -> list[dict]:
    items = []
    for call in calls:
        cmd_str = str(call.args.get("command", "true"))
        items.append(
            batch_item(
                id = call.id,
                kind = "exec",
                spec = exec_spec(
                    cmd = ["sh", "-c", cmd_str],
                    cwd = cwd if cwd != "" else None,
                    timeout_ms = 30000,
                    check = False,
                ),
            ),
        )
    return items

def build_safe_batch(ctx: SessionCtx) -> list[dict]:
    return build_batch_for_calls(ctx.safe_calls, ctx.cwd)

def build_approved_batch(ctx: SessionCtx) -> list[dict]:
    return build_batch_for_calls(ctx.risky_calls, ctx.cwd)

MAX_TOOL_STREAM_CHARS = 16000

def truncate_stream(text: str, limit: int = MAX_TOOL_STREAM_CHARS) -> str:
    n = len(text)
    if n <= limit:
        return text
    head = int(limit * 3 / 4)
    tail = limit - head
    omitted = n - head - tail
    return text[:head] + "\n... [truncated " + str(omitted) + " chars; refine command or pipe through head/tail] ...\n" + text[n - tail:]

def format_batch_tool_results(calls: list[ToolCall], ev: dict) -> (list[LlmMessage], int, int):
    out = ev.get("output", {})
    results = out.get("results", []) if type(out) == "dict" else []
    by_id = {}
    for r in results:
        if type(r) == "dict" and "id" in r:
            by_id[str(r["id"])] = r

    new_msgs = []
    err_count = 0
    for call in calls:
        res = by_id.get(call.id)
        if res == None:
            err_count += 1
            new_msgs.append(tool_result(call, {"error": "missing batch result"}))
        elif not res.get("ok", False):
            err_count += 1
            new_msgs.append(
                tool_result(
                    call,
                    {
                        "error": truncate_stream(str(res.get("error", "exec_failed"))),
                        "hint": "Inspect the error and retry with corrected arguments.",
                    },
                ),
            )
        else:
            cmd_out = res.get("output", {})
            if type(cmd_out) == "dict":
                sanitized = dict(cmd_out)
                if "stdout" in sanitized:
                    sanitized["stdout"] = truncate_stream(str(sanitized["stdout"]))
                if "stderr" in sanitized:
                    sanitized["stderr"] = truncate_stream(str(sanitized["stderr"]))
                if int(sanitized.get("code", 0)) != 0:
                    err_count += 1
                    sanitized["hint"] = "Command exited non-zero; inspect stderr and adjust command or arguments."
                new_msgs.append(tool_result(call, sanitized))
            else:
                new_msgs.append(tool_result(call, truncate_stream(str(cmd_out))))
    return (new_msgs, len(calls), err_count)

def record_safe_batch_results(ctx: SessionCtx, ev: dict) -> SessionCtx:
    new_msgs, ran, errs = format_batch_tool_results(ctx.safe_calls, ev)
    return update(
        ctx,
        messages = ctx.messages + new_msgs,
        safe_calls = [],
        tool_runs = ctx.tool_runs + ran,
        tool_errors = ctx.tool_errors + errs,
    )

def record_approved_batch_results(ctx: SessionCtx, ev: dict) -> SessionCtx:
    new_msgs, ran, errs = format_batch_tool_results(ctx.risky_calls, ev)
    return update(
        ctx,
        messages = ctx.messages + new_msgs,
        risky_calls = [],
        approval_rows = [],
        tool_runs = ctx.tool_runs + ran,
        tool_errors = ctx.tool_errors + errs,
    )

def deny_risky_calls(ctx: SessionCtx, _ev: dict) -> SessionCtx:
    denied_msgs = [
        tool_result(
            call,
            {
                "status": "denied",
                "error": "User denied permission to run this command.",
            },
        )
        for call in ctx.risky_calls
    ]
    return update(
        ctx,
        messages = ctx.messages + denied_msgs,
        risky_calls = [],
        approval_rows = [],
    )

def has_pending_risky_calls(ctx: SessionCtx, _ev: dict) -> bool:
    return len(ctx.risky_calls) > 0

chart: StateMachine = machine(
    id = "coding_session_llm",
    description = "Interactive multi-turn coding agent with parallel invoke_all, safe-first split, and human approval gate",
    initial = S.gather_input,
    context = SessionCtx(),
    states = {
        S.gather_input: state(
            on = [
                transition(
                    event = SessionEvent.PROMPT,
                    target = S.done,
                    guard = is_exit_prompt,
                ),
                transition(
                    event = SessionEvent.PROMPT,
                    target = S.working,
                    guard = has_prompt_text,
                    assign = append_user_prompt,
                ),
                transition(
                    event = SessionEvent.EXIT,
                    target = S.done,
                ),
            ],
        ),
        S.working: compound_state(
            initial = S.thinking,
            on = [
                transition(
                    event = SessionEvent.INTERRUPT,
                    target = S.gather_input,
                    assign = lambda ctx, _ev: update(
                        ctx,
                        safe_calls = [],
                        risky_calls = [],
                        approval_rows = [],
                    ),
                ),
            ],
            states = {
                S.thinking: state(
                    invoke = invoke_llm(
                        id = "llm_turn",
                        model = select_model,
                        system = "You are a pragmatic local coding assistant running on the user's machine. Use the provided tools proactively whenever they can help answer the user's request.",
                        system_cache = CachePolicy.ephemeral,
                        messages = lambda ctx: ctx.messages,
                        tools = [
                            tool_def(
                                "bash",
                                description = "Execute a shell command in the workspace and return stdout, stderr, and exit_code.",
                                schema = BashArgs,
                            ),
                        ],
                        tool_choice = ToolChoice.auto,
                        stream = True,
                        timeout_ms = 60000,
                        on_done = [
                            outcome(
                                target = S.acting_safe,
                                guard = has_any_safe_calls,
                                assign = record_classified_calls,
                            ),
                            outcome(
                                target = S.awaiting_approval,
                                guard = has_only_risky_calls,
                                assign = record_classified_calls,
                            ),
                            outcome(
                                target = S.gather_input,
                                assign = record_final_reply,
                            ),
                        ],
                        on_error = outcome(
                            target = S.failed,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "llm_failed"))),
                        ),
                    ),
                ),
                S.acting_safe: state(
                    invoke = invoke_all(
                        id = "run_safe_tools",
                        items = build_safe_batch,
                        max_concurrency = 4,
                        on_done = [
                            outcome(
                                target = S.awaiting_approval,
                                guard = has_pending_risky_calls,
                                assign = record_safe_batch_results,
                            ),
                            outcome(
                                target = S.thinking,
                                assign = record_safe_batch_results,
                            ),
                        ],
                        on_error = outcome(
                            target = S.failed,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "batch_failed"))),
                        ),
                    ),
                ),
                S.awaiting_approval: state(
                    on = [
                        transition(
                            event = SessionEvent.APPROVE,
                            target = S.acting_approved,
                        ),
                        transition(
                            event = SessionEvent.DENY,
                            target = S.thinking,
                            assign = deny_risky_calls,
                        ),
                    ],
                ),
                S.acting_approved: state(
                    invoke = invoke_all(
                        id = "run_approved_tools",
                        items = build_approved_batch,
                        max_concurrency = 4,
                        on_done = outcome(
                            target = S.thinking,
                            assign = record_approved_batch_results,
                        ),
                        on_error = outcome(
                            target = S.failed,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "batch_failed"))),
                        ),
                    ),
                ),
            },
        ),
        S.done: final_state(output = {"status": "done"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

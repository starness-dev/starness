# Zero-compile interactive multi-turn coding harness using `@starness/llm` + `@starness/exec`:
# - Maintains full conversation history in typed `SessionCtx.messages`, round-tripping assistant
#   messages via `assistant_msg(ev)` so tool calls, reasoning blocks, and thought signatures are
#   preserved across turns and across providers (`genai`).
# - Uses a hierarchical Harel `compound_state` (`working { thinking <-> acting }`) so a single
#   `INTERRUPT` event cancels either an in-flight LLM stream or a running `bash` subprocess
#   automatically (`kill_on_drop`) and returns cleanly to `gather_input`.
#
# Try interactively:
#   starness run examples/coding_session_llm.star --events-stdin --trace -
#   > {"type":"PROMPT","text":"Check the current working directory with bash"}
#   > {"type":"PROMPT","text":"/exit"}

load("@starness/exec", "ExecSpec", "exec_spec", "invoke_cmd")
load(
    "@starness/llm",
    "CachePolicy",
    "LlmModel",
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

SessionEvent = enum("PROMPT", "INTERRUPT", "EXIT")

BashArgs = record(
    command = str,
)

SessionCtx = record(
    model = field(str, "gpt-6-luna"),
    llm_endpoint = field(str, ""),
    cwd = field(str, ""),
    messages = field(list[dict], []),
    pending_calls = field(list[dict], []),
    last_reply = field(str, ""),
    turns = field(int, 0),
    tool_runs = field(int, 0),
    reason = field(str, ""),
)

S = state_space(
    "gather_input",
    "working",
    "thinking",
    "acting",
    "done",
    "failed",
)

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
        pending_calls = [],
    )

def record_tool_calls(ctx: SessionCtx, ev: dict) -> SessionCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = llm_tool_calls(ev),
    )

def record_final_reply(ctx: SessionCtx, ev: dict) -> SessionCtx:
    return update(
        ctx,
        messages = ctx.messages + [assistant_msg(ev)],
        pending_calls = [],
        last_reply = llm_text(ev),
        turns = ctx.turns + 1,
    )

def has_more_tool_calls(ctx: SessionCtx, _ev: dict) -> bool:
    return len(ctx.pending_calls) > 1

def build_bash_spec(ctx: SessionCtx) -> ExecSpec:
    call = ctx.pending_calls[0] if len(ctx.pending_calls) > 0 else None
    args = call.get("args", {}) if call != None else {}
    cmd_str = str(args.get("command", "true"))
    return exec_spec(
        cmd = ["sh", "-c", cmd_str],
        cwd = ctx.cwd if ctx.cwd != "" else None,
        timeout_ms = 30000,
        check = False,
    )

def record_bash_output(ctx: SessionCtx, ev: dict) -> SessionCtx:
    call = ctx.pending_calls[0] if len(ctx.pending_calls) > 0 else None
    rest = ctx.pending_calls[1:] if len(ctx.pending_calls) > 1 else []
    out = ev.get("output", {})
    tr = tool_result(call if call != None else "bash", out)
    return update(
        ctx,
        messages = ctx.messages + [tr],
        pending_calls = rest,
        tool_runs = ctx.tool_runs + 1,
    )

def record_bash_error(ctx: SessionCtx, ev: dict) -> SessionCtx:
    call = ctx.pending_calls[0] if len(ctx.pending_calls) > 0 else None
    rest = ctx.pending_calls[1:] if len(ctx.pending_calls) > 1 else []
    tr = tool_result(call if call != None else "bash", {"error": str(ev.get("error", "exec_failed"))})
    return update(
        ctx,
        messages = ctx.messages + [tr],
        pending_calls = rest,
        tool_runs = ctx.tool_runs + 1,
    )

chart: StateMachine = machine(
    id = "coding_session_llm",
    description = "Interactive multi-turn coding agent harness powered by @starness/llm and @starness/exec",
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
                    assign = lambda ctx, _ev: update(ctx, pending_calls = []),
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
                                target = S.acting,
                                guard = lambda _ctx, ev: has_tool_calls(ev),
                                assign = record_tool_calls,
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
                S.acting: state(
                    invoke = invoke_cmd(
                        id = "run_bash",
                        cmd = build_bash_spec,
                        on_done = [
                            outcome(
                                target = S.acting,
                                guard = has_more_tool_calls,
                                assign = record_bash_output,
                            ),
                            outcome(
                                target = S.thinking,
                                assign = record_bash_output,
                            ),
                        ],
                        on_error = [
                            outcome(
                                target = S.acting,
                                guard = has_more_tool_calls,
                                assign = record_bash_error,
                            ),
                            outcome(
                                target = S.thinking,
                                assign = record_bash_error,
                            ),
                        ],
                    ),
                ),
            },
        ),
        S.done: final_state(output = {"status": "done"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

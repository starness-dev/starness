# 01 — Multi-turn AI Chat Harness (`@starness/llm` + streaming `Conversation` UI)
#
# The simplest starting point in Starness:
# - Typed `ChatCtx` holds the model name, conversation history (`list[LlmMessage]`), and any error.
# - `attachments_from(ev.get("files", []))` turns dragged/uploaded files into typed `LlmAttachment`s.
# - `assistant_msg(ev)` round-trips full assistant turns (including reasoning/thought blocks).
# - `INTERRUPT` cancels an in-flight LLM stream cleanly and returns to `gather_input`.
#
# Try it:
#   export OPENAI_API_KEY="sk-..."   # or `starness secrets set OPENAI_API_KEY`
#   starness run -i examples/01_chat_harness.star --ui examples/01_chat_harness.ui.json
#   starness serve examples/01_chat_harness.star --ui examples/01_chat_harness.ui.json --bind 127.0.0.1:8080

load(
    "@starness/llm",
    "LlmMessage",
    "assistant_msg",
    "attachments_from",
    "invoke_llm",
    "llm_model",
    "user_msg",
)
load("@starness/secrets", "secret")

# Events the user (TUI, Web Studio, HTTP, or stdin) can send to this machine.
ChatEvent = enum("PROMPT", "INTERRUPT")

# Immutable statechart context persisted across turns and rendered by `01_chat_harness.ui.json`.
ChatCtx = record(
    model = field(str, "gpt-6-luna"),
    messages = field(list[LlmMessage], []),
    error = field(str, ""),
)

# All valid state targets — statically typechecked by `starness check` so typos fail before runtime.
S = state_space("gather_input", "thinking", "done", "failed")

SYSTEM_PROMPT = "You are a concise, helpful assistant."

# Statechart definition:
#   gather_input --(PROMPT)--> thinking --(on_done)--> gather_input
#        |                        |----(INTERRUPT)---> gather_input
#        |--(PROMPT "/exit")--> done
#                                 |----(on_error)----> failed
chart: StateMachine = machine(
    id = "chat_harness",
    description = "Minimal multi-turn AI chat harness powered by @starness/llm",
    initial = S.gather_input,
    context = ChatCtx(),
    states = {
        # Quiescent state: waits for a PROMPT event (text and/or attached files).
        S.gather_input: state(
            on = [
                # Transitions are evaluated in order: `/exit` terminates the session cleanly.
                transition(
                    event = ChatEvent.PROMPT,
                    guard = lambda _ctx, ev: str(ev.get("text", "")).strip().lower() == "/exit",
                    target = S.done,
                ),
                # Any non-empty prompt or file attachment appends a typed `user_msg` and enters `thinking`.
                transition(
                    event = ChatEvent.PROMPT,
                    guard = lambda _ctx, ev: str(ev.get("text", "")).strip() != "" or len(ev.get("files", [])) > 0,
                    target = S.thinking,
                    assign = lambda ctx, ev: update(
                        ctx,
                        messages = ctx.messages + [
                            user_msg(
                                str(ev.get("text", "")).strip(),
                                attachments = attachments_from(ev.get("files", [])),
                            ),
                        ],
                    ),
                ),
            ],
        ),
        # Active state: streams a single LLM turn; exiting on INTERRUPT cancels the HTTP stream automatically.
        S.thinking: state(
            on = [
                transition(
                    event = ChatEvent.INTERRUPT,
                    target = S.gather_input,
                ),
            ],
            invoke = invoke_llm(
                id = "chat_turn",
                # `secret(..., optional = True)` resolves from env or OS Keychain at runtime and is scrubbed from traces.
                model = lambda ctx: llm_model(
                    ctx.model,
                    api_key = secret("OPENAI_API_KEY", optional = True),
                ),
                system = SYSTEM_PROMPT,
                messages = lambda ctx: ctx.messages,
                stream = True,
                # On completion, append the lossless assistant message and return to `gather_input`.
                on_done = outcome(
                    target = S.gather_input,
                    assign = lambda ctx, ev: update(
                        ctx,
                        messages = ctx.messages + [assistant_msg(ev)],
                    ),
                ),
                # Capture any provider/network error into `ctx.error` so the `failed` UI view displays it.
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(
                        ctx,
                        error = str(ev.get("error", "LLM request failed")),
                    ),
                ),
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)

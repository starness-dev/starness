# 01 — Multi-turn AI Chat Harness (`@starness/llm` + streaming `Conversation` UI)
#
# The simplest starting point in Starness:
# - Typed `ChatCtx` holds conversation history (`messages`) and model name.
# - `assistant_msg(ev)` round-trips full assistant messages (including reasoning/thought blocks).
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

ChatEvent = enum("PROMPT", "INTERRUPT")

ChatCtx = record(
    model = field(str, "gpt-6-luna"),
    messages = field(list[LlmMessage], []),
    error = field(str, ""),
)

S = state_space("gather_input", "thinking", "done", "failed")

SYSTEM_PROMPT = "You are a concise, helpful assistant."

chart: StateMachine = machine(
    id = "chat_harness",
    description = "Minimal multi-turn AI chat harness powered by @starness/llm",
    initial = S.gather_input,
    context = ChatCtx(),
    states = {
        S.gather_input: state(
            on = [
                transition(
                    event = ChatEvent.PROMPT,
                    guard = lambda _ctx, ev: str(ev.get("text", "")).strip().lower() == "/exit",
                    target = S.done,
                ),
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
        S.thinking: state(
            on = [
                transition(
                    event = ChatEvent.INTERRUPT,
                    target = S.gather_input,
                ),
            ],
            invoke = invoke_llm(
                id = "chat_turn",
                model = lambda ctx: llm_model(
                    ctx.model,
                    api_key = secret("OPENAI_API_KEY", optional = True),
                ),
                system = SYSTEM_PROMPT,
                messages = lambda ctx: ctx.messages,
                stream = True,
                on_done = outcome(
                    target = S.gather_input,
                    assign = lambda ctx, ev: update(
                        ctx,
                        messages = ctx.messages + [assistant_msg(ev)],
                    ),
                ),
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

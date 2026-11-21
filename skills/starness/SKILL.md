---
name: starness
description: Design, write, verify, diagram, and operate typed Starlark workflows, AI agents, and coding harnesses with `starness`. Use whenever creating or editing `.star` statechart files, choosing between `@starness/{decide,llm,acp,mcp,http,exec,secrets}`, debugging `starness check` / `run` / `diagram` output, or hosting workflows with `starness serve`.
---

# Starness Coding & Architecture Playbook

Starness lets you declare AI workflows, agents, and harnesses as **statically typechecked hierarchical state machines** in Starlark (a deterministic subset of Python). Every `.star` file is parsed, typechecked, and graph-verified before a single token, subprocess, or HTTP request executes.

## 0. Ensure `starness` Is Installed

Check `command -v starness`. If missing:
- Inside a local checkout: `cargo install --path bins/starness-cli` (or run `cargo run -q -p starness-cli -- ...`).
- From Git: `cargo install --git https://github.com/floodfx/starness starness-cli`.

---

## 1. How to Design a Starness Workflow

Follow this 4-step design sequence before writing code:

### Step 1: Shape the Control Flow with Hierarchical States
Do not flatten complex workflows into a giant list of unrelated states with duplicated error/cancel edges. Pick the state node that matches the shape of the problem:
- **`state(...)`** — One atomic step (wait for an external event, or run one `invoke` and branch on `on_done` / `on_error`).
- **`compound_state(...)`** — Group a multi-step sub-workflow (e.g., `working { thinking <-> acting }` or `rolling_out { canary -> baking -> promoting }`). Put shared `INTERRUPT`, `CANCEL`, or `PAUSE` transitions **once** on the parent `compound_state`; exiting it automatically cancels whichever child `invoke` or `after` timer is active.
- **`parallel_state(...)`** — Run independent branches concurrently (e.g., 3 coding harnesses reviewing a PR for quorum, or smoke tests + security scan). Each region is a `compound_state` ending in its own `final_state()`; the parent's `on_done` fires automatically once **every** region completes.
- **`shallow_history()` / `deep_history()`** — Support pause/resume. Transition out of a `compound_state` to `paused`, then transition back to its history node to resume in the exact substate you left off.
- **`final_state(output = ...)`** — Terminal outcome (`published`, `resolved`, `aborted`, `failed`).

> Read [`references/statecharts.md`](references/statecharts.md) for complete patterns on `compound_state`, `parallel_state`, history, `after` exponential backoff, guards, actions, and multi-file `load(...)`.

### Step 2: Pick the Right Execution Tier per State
Never use a heavyweight autonomous agent for work that belongs to a lighter, faster, or deterministic tier:

| Need | Module | Why |
| :--- | :--- | :--- |
| Yes/no gate, enum routing, urgency score | `@starness/decide` | Fast, calibrated probabilities (`is_yes`, `prob`, `refused`) with clean human fallback when uncertain |
| Text synthesis, typed `Record` extraction, custom multi-turn tool loop | `@starness/llm` | Direct provider-neutral LLM calls with constrained-decoding `schema = MyRecord`, native `tool_def`s, attachments, and prompt caching |
| Multi-step repo editing / autonomous coding | `@starness/acp` | Delegates to Claude Code, Codex, Antigravity, or any ACP v1 harness with typed `read_tool` / `artifact_tool` MCP handoffs |
| External tool server | `@starness/mcp` | Deterministic MCP tool calls over `stdio` or Streamable HTTP |
| REST API / webhook | `@starness/http` | Async HTTP/JSON requests cancelled automatically on state exit |
| Shell command / build / test / CLI | `@starness/exec` | Subprocess execution with automatic `kill_on_drop` cancellation on state exit |
| Human-in-the-loop UI (Web & TUI) | `@starness/ui` | `json-render` specs (`ui_spec`, `ui_markdown_review`, `ui_bind`, buttons/inputs) with static event verification, embedded Web Studio (`/ui`), and `starness run -i` |
| API keys & tokens | `@starness/secrets` | Late-bound `env` / `file` / OS `keychain` secrets scrubbed as `<secret:NAME>` from traces & errors |

> Read [`references/modules.md`](references/modules.md) for when, why, and how to use each `@starness/*` module with full idioms and signatures.

### Step 3: Write Typed, Immutable Starlark
Structure every `.star` file in this clean order:
1. `load("@starness/...", ...)` imports (and shared local modules via `load("types.star", ...)`).
2. Typed data declarations: `enum(...)` for events/categories, `record(...)` for context and structured LLM/tool schemas, and `state_space(...)` for all `StateId`s.
3. Pure helper functions: guards (`(ctx, ev) -> bool`), reducers (`(ctx, ev) -> Ctx` using `update(ctx, ...)`), and dynamic input builders (`(ctx) -> ...`).
4. `chart: StateMachine = machine(id = ..., initial = ..., context = Ctx(), states = { ... })`.

```python
load("@starness/llm", "invoke_llm", "llm_structured")

Verdict = record(approved = bool, summary = str)
Ctx = record(diff = field(str, ""), summary = field(str, ""), reason = field(str, ""))
S = state_space("reviewing", "awaiting_human", "done", "failed")

def save_verdict(ctx: Ctx, ev: dict) -> Ctx:
    return update(ctx, summary = llm_structured(ev, Verdict).summary)

chart: StateMachine = machine(
    id = "pr_gate",
    initial = S.reviewing,
    context = Ctx(),
    states = {
        S.reviewing: state(
            invoke = invoke_llm(
                model = "gpt-6-luna",
                prompt = lambda ctx: "Review:\n" + ctx.diff,
                schema = Verdict,
                on_done = [
                    outcome(target = S.done, guard = lambda _c, ev: llm_structured(ev, Verdict).approved, assign = save_verdict),
                    outcome(target = S.awaiting_human, assign = save_verdict),
                ],
                on_error = outcome(target = S.failed, assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "")))),
            ),
        ),
        S.awaiting_human: state(on = [transition(event = "APPROVE", target = S.done)]),
        S.done: final_state(output = {"status": "done"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)
```

### Step 4: Run the Verification Loop (Zero I/O First)
Always verify before executing any external calls:
1. **`starness check <file.star>`** — Static typecheck, graph reachability, dangling `StateId`s, duplicate MCP tools, and `secret(...)`-in-`context` check. Fix every error here first.
2. **`starness diagram <file.star>`** — Inspect the Mermaid `stateDiagram-v2` to confirm hierarchy, parallel regions, `on_done`/`on_error` branches, and terminal states match intent.
3. **`starness run <file.star> --trace -`** (add `--context '{...}'` and `--events-stdin` for interactive workflows) — Inspect lossless JSONL trace records (`started`, `transition`, `service_started`, `service_progress`, `service_finished`).

> Read [`references/serve-and-ops.md`](references/serve-and-ops.md) when hosting durable multi-instance workflows with `starness serve --db`, wiring webhooks/human approvals over HTTP + SSE, or exposing a workflow via `starness acp`.

---

## 2. Golden Rules (Prevent 95% of Errors)

1. **Never pass raw strings as state targets** — Always use `S.my_state` from `state_space(...)` or `state_id(...)`; raw strings fail static typecheck before evaluation.
2. **Never mutate `ctx` in place** — Starlark values are immutable. Always return `update(ctx, field = new_val)` from `assign` reducers.
3. **Never place `secret(...)` inside `machine(context = ...)`** — `starness check` rejects it so credentials can never leak into SQLite snapshots or traces. Pass `secret(...)` only in runtime I/O fields (`api_key`, `headers`, `env`, `account_id`).
4. **Always wire `on_error` on every `invoke`** — Unhandled service errors terminate the actor; route `on_error` to a retry state, fallback state, or `failed` `final_state()`.
5. **Round-trip LLM turns with `assistant_msg(ev)`** — Never rebuild assistant turns from `llm_text(ev)` in multi-turn tool loops. `assistant_msg(ev)` preserves `ToolCall`s, reasoning blocks, and Gemini/OpenAI thought signatures losslessly.
6. **Pass the full call dict to `tool_result(call, content)`** — Passing `call` (from `llm_tool_call(ev)` / `llm_tool_calls(ev)`) populates both `call_id` (OpenAI/Anthropic) and `fn_name` (required by Gemini).
7. **Always name guards/reducers for readability** — Named `def` functions show up cleanly in `starness diagram` (`[has_quorum] / assign tally`) instead of generic `[lambda]`.

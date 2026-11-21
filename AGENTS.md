# Starness Agent Authoring Guide

This file is a concise reference for AI coding agents (and humans) writing, validating, and running `.star` statecharts with `starness`.

---

## 1. The Authoring & Verification Loop

Always follow this tight loop — `starness check` and `starness diagram` are 100% offline and execute zero I/O:

1. **Write or edit** `<workflow>.star` exporting `chart: StateMachine = machine(...)`.
2. **Typecheck & graph-verify**:
   ```bash
   starness check <workflow>.star
   ```
   Catches syntax errors, Starlark type mismatches, raw-string targets, dangling `StateId`s, duplicate MCP tool names, and `secret(...)` values placed inside `context`.
3. **Inspect topology**:
   ```bash
   starness diagram <workflow>.star
   ```
   Confirm every state is reachable, `on_done`/`on_error` branches cover failure paths, and `final_state()` nodes are wired.
4. **Run with lossless JSONL tracing**:
   ```bash
   starness run <workflow>.star --trace -
   # Or with context overrides and interactive stdin events:
   starness run <workflow>.star --context '{"key":"val"}' --events-stdin --trace -
   ```

*(In this repo, substitute `cargo run -q -p starness-cli --` for `starness`.)*

---

## 2. Golden Rules (Prevent 95% of Errors)

1. **Always use `state_space(...)` or `state_id(...)` for state targets** — raw strings like `target = "done"` fail static typecheck before evaluation.
2. **Use `record(...)` + `update(ctx, field = val)` for typed context** — Starlark values are immutable; `update(ctx, ...)` returns a validated shallow copy of a `record` or `dict`.
3. **Never store `secret(...)` in `machine(context = ...)`** — `starness check` rejects it to prevent secret persistence in snapshots/SQLite. Pass `secret(...)` only to runtime I/O fields (`headers`, `env`, `api_key`, `account_id`).
4. **Always wire `on_error` on every `invoke`** — unhandled service errors terminate the actor; route `on_error` to a retry or `failed` state.
5. **Round-trip LLM assistant messages with `assistant_msg(ev)`** — never reconstruct assistant messages from `llm_text(ev)` in tool loops. `assistant_msg(ev)` preserves `ToolCall`s, `ReasoningContent`, `ThoughtSignature`s (Gemini), and provider-native `CustomPart`s.
6. **Pass the tool-call dict to `tool_result(call, content)`** — passing `call` (from `llm_tool_call(ev)` / `llm_tool_calls(ev)`) populates both `call_id` (OpenAI/Anthropic) and `fn_name` (required by Gemini `functionResponse`).
7. **Pick the lightest AI tier**:
   - Deterministic I/O → `@starness/http`, `@starness/exec`, `@starness/mcp`
   - Fast yes/no, enum choice, or scored routing → `@starness/decide`
   - Single-shot text, structured `Record` extraction, or custom tool loops → `@starness/llm`
   - Full autonomous coding/file-editing agent session → `@starness/acp`

---

## 3. Core Starlark DSL Reference

### State & Data Declarations
- `S = state_space("a", "b", "c")` — typed namespace of `StateId` handles (`S.a`, `S.b`, `S.c`).
- `state_id("name")` — single `StateId`.
- `EnumType = enum("A", "B")` — typed enum (`EnumType.A.value` is `"A"`).
- `Ctx = record(x = int, y = field(str, "default"), opt = field(str | None, None))` — typed immutable record.
- `update(ctx, x = ctx.x + 1)` — validated shallow copy of a `record` or `dict`.

### State Nodes
- `machine(id = str, initial = StateId, states = {StateId: StateNode, ...}, context = None, output = None, description = None) -> StateMachine`
- `state(on = [], always = [], after = [], invoke = [], entry = [], exit = [], tags = [], meta = None, description = None)`
- `compound_state(initial = StateId, states = {...}, on = [], always = [], after = [], invoke = [], on_done = [], entry = [], exit = [], ...)`
- `parallel_state(states = {...}, on = [], always = [], after = [], invoke = [], on_done = [], entry = [], exit = [], ...)` — `on_done` fires automatically once every child region reaches a `final_state`.
- `final_state(output = None, entry = [], exit = [], tags = [], meta = None, description = None)`
- `shallow_history(target = None)` / `deep_history(target = None)`

### Transitions, Timers, Guards & Actions
- `transition(event = str | Enum, target = StateId | None, guard = None, actions = [], assign = None, reenter = False, description = None)`
- `internal_transition(event = str | Enum, guard = None, actions = [], assign = None)` — runs actions/reducers without exiting or re-entering the state.
- `forbid(event = str | Enum)` — consumes an event so ancestor superstates do not handle it.
- `outcome(target = StateId | None, guard = None, actions = [], assign = None, reenter = False)` — branch used in `always`, `after`, `on_done`, and `invoke`.
- `after(delay = int | str | Callable, target = ..., guard = ..., assign = ..., outcomes = [...])` — delay in ms (`int`) or `lambda ctx: int`.
- Guards: `lambda ctx, ev: bool`, `state_in(S.other)`, `all_of(g1, g2)`, `any_of(g1, g2)`, `not_(g)`, `named_guard("NAME")`.
- Reducers (`assign`): `lambda ctx, ev: Ctx` (returns updated context).
- Built-in actions: `raise_event(event = ..., payload = None, delay = None, id = None)`, `cancel_send(id = ...)`, `emit_event(event = ..., payload = None)`, `send_to(to = ..., event = ..., payload = None, delay = None, id = None)`, `send_parent(event = ..., payload = None)`, `log_action(expr)`.

---

## 4. `@starness/*` Standard Modules Cheat Sheet

### `@starness/secrets`
```python
load("@starness/secrets", "SecretSource", "secret")
# source: "env" (default) | "file" | "keychain"
secret("OPENAI_API_KEY", source = "env", service = "starness", prefix = "", optional = False)
```

### `@starness/http`
```python
load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")
req = http_request(url = str, method = "GET", headers = {}, json = None, body = None, timeout_ms = 30000)
invoke_http(request = req_or_fn, id = None, on_done = ..., on_error = ...)
# ev["output"] -> {"status": int, "ok": bool, "headers": dict, "body": str, "json": Any}
```

### `@starness/exec`
```python
load("@starness/exec", "ExecSpec", "exec_spec", "invoke_cmd")
spec = exec_spec(cmd = ["sh", "-c", "..."], cwd = None, env = {}, stdin = None, timeout_ms = 60000, check = True)
invoke_cmd(cmd = list_or_spec_or_fn, id = None, on_done = ..., on_error = ...)
# ev["output"] -> {"code": int, "stdout": str, "stderr": str}
```

### `@starness/mcp`
```python
load("@starness/mcp", "McpServer", "invoke_mcp", "mcp_http_server", "mcp_stdio_server")
srv = mcp_http_server(id = "tracker", url = "http://...", headers = {}, timeout_ms = 30000)
# or mcp_stdio_server(id = "gh", command = "npx", args = [...], cwd = None, env = {}, timeout_ms = 30000)
invoke_mcp(server = srv_or_fn, tool = "tool_name", arguments = dict_or_fn, check_tool_error = True, id = None, on_done = ..., on_error = ...)
# ev["output"] -> {"text": str, "data": Any, "is_error": bool, ...}
```

### `@starness/decide`
```python
load("@starness/decide", "choice", "confidence", "invoke_decision", "is_yes", "jev", "level", "noul", "prob", "refused", "score", "score_value", "selected")
# Providers: jev(), gemini_decision(), openai_decision(), openrouter_decision(), cloudflare_decision()
invoke_decision(
    provider = jev(endpoint = None),
    evidence = dict_or_str_or_fn,
    questions = {
        "q1": noul("Yes/no question?", yes = "...", no = "...", threshold = 0.85, optional = False),
        "q2": choice("Pick one", options = {MyEnum.A: "desc A", MyEnum.B: "desc B"}, optional = True),
        "q3": score("Rate urgency", levels = ["low", "medium", "high"]),
    },
    on_done = ..., on_error = ...,
)
```

### `@starness/llm`
```python
load(
    "@starness/llm",
    "CachePolicy", "LlmModel", "ReasoningEffort", "ToolChoice",
    "assistant_msg", "attachment", "has_tool_calls", "invoke_llm",
    "llm_model", "llm_structured", "llm_text", "llm_tool_call", "llm_tool_calls",
    "system_msg", "tool_def", "tool_result", "tool_results_msg", "user_msg",
)

m = llm_model("gpt-6-luna", api_key = None, endpoint = None, temperature = None, max_tokens = None, top_p = None, reasoning_effort = None)
invoke_llm(
    model = m_or_str_or_fn,
    prompt = str_or_fn_or_None,          # required if `messages` is None
    messages = list_or_fn_or_None,       # required if `prompt` is None
    system = str_or_fn_or_None,
    system_cache = CachePolicy.ephemeral, # "ephemeral" | "memory" | "5m" | "1h" | "24h"
    context = None,                      # optional <context>...</context> block
    tools = [tool_def("bash", description = "...", schema = BashArgs)],
    tool_choice = ToolChoice.auto,       # "auto" | "none" | "required" | "<tool_name>"
    schema = MyRecord_or_json_schema_dict_or_None,
    stream = True,
    timeout_ms = 60000,
    id = None, on_done = ..., on_error = ...,
)
# Output helpers:
#   llm_text(ev) -> str
#   llm_structured(ev, MyRecord) -> MyRecord (validated typed Record)
#   has_tool_calls(ev) -> bool
#   llm_tool_calls(ev) -> list[{"id": str, "name": str, "args": dict}]
#   llm_tool_call(ev) -> dict | None
#   assistant_msg(ev) -> lossless round-trippable message dict for ctx.messages
#   tool_result(call, content) -> tool message dict preserving call_id + fn_name
```

### `@starness/acp`
```python
load("@starness/acp", "ArtifactMode", "acp_agent", "agy_agent", "artifact", "artifact_list", "artifact_tool", "claude_agent", "codex_agent", "invoke_agent", "read_tool")
invoke_agent(
    agent = agy_agent(id = "coder", model = "", cwd = None, timeout_ms = 300000),
    prompt = str_or_fn,
    context = None,
    tools = [
        read_tool("get_errors", description = "...", schema = QueryArgs, handler = fn),
        artifact_tool("submit_plan", description = "...", schema = PlanRecord, mode = ArtifactMode.replace, required = True, validate = fn_or_None),
    ],
    id = None, on_done = ..., on_error = ...,
)
# Extract validated artifacts in reducers/guards: artifact(ev, "submit_plan", PlanRecord)
```

---

## 5. HTTP Daemon API (`starness serve`)

- `GET /health` — liveness, loaded machines, live actor count, DB target.
- `GET /machines` / `GET /machines/:machine` / `GET /machines/:machine/diagram`.
- `POST /machines/:machine/instances` — body `{"id"?, "context"?, "idempotent"?}` → returns `InstanceSnapshot` (instance ID is `.id`).
- `GET /machines/:machine/instances` (`?status=active|waiting|completed|failed|cancelled`).
- `GET /machines/:machine/instances/:id` — current `InstanceSnapshot`.
- `POST /machines/:machine/instances/:id/events` — non-blocking; dispatches `{"type": "...", ...}` and immediately returns the updated `InstanceSnapshot`.
- `GET /machines/:machine/instances/:id/trace` — per-instance SSE (`text/event-stream`) or `?format=jsonl`.
- `DELETE /machines/:machine/instances/:id` — cancels instance (`?purge=true` removes from SQLite).

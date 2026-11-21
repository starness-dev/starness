# `@starness/*` Standard Modules Guide

When, why, and how to use every built-in `@starness/*` module.

---

## 1. `@starness/decide` — Calibrated Decision Models

**When & Why:** Use for fast, cheap, high-volume routing gates where you need **calibrated probabilities** and explicit **refusal handling** (yes/no `noul`, multi-class `choice`, ordered `score`). Unlike free-form LLM prompts, `@starness/decide` lets you branch deterministically on `prob(ev, name) >= 0.85` and fall back to a human whenever `refused(ev, name)` is `True` or confidence is low.

**Providers:** `jev()` (default), `gemini_decision()`, `openai_decision()`, `openrouter_decision()`, `cloudflare_decision()`. All accept `model = ...`, `api_key = secret(...)`, and `endpoint = ...`.

**Pattern:**
```python
load(
    "@starness/decide",
    "choice", "confidence", "invoke_decision", "is_yes", "jev",
    "level", "noul", "prob", "refused", "score", "score_value", "selected",
)

Team = enum("BILLING", "TECHNICAL", "SECURITY")

invoke_decision(
    provider = jev(),
    evidence = lambda ctx: {"subject": ctx.subject, "body": ctx.body},
    questions = {
        "refund": noul("Is the customer asking for a refund?", yes = "Explicitly requests refund.", no = "No refund requested.", threshold = 0.85),
        "team": choice("Which team owns this ticket?", options = {
            Team.BILLING: "Invoices and refunds.",
            Team.TECHNICAL: "Bugs and outages.",
            Team.SECURITY: "Account takeover or vulnerabilities.",
        }, optional = True),
        "urgency": score("How urgent is this?", levels = ["routine", "today", "urgent", "critical"]),
    },
    on_done = [
        outcome(
            target = S.auto_refund,
            guard = lambda _c, ev: not refused(ev, "team") and selected(ev, "team") == Team.BILLING.value and is_yes(ev, "refund") and prob(ev, "team") >= 0.85,
            assign = save_triage,
        ),
        outcome(target = S.awaiting_human, assign = save_triage),
    ],
    on_error = S.failed,
)
```

---

## 2. `@starness/llm` — LLMs, Structured `Record`s & Custom Tool Loops

**When & Why:** Use for:
1. **Single-shot text or structured `Record` extraction** (`prompt = ...`, `schema = MyRecord`) — Starness compiles your Starlark `record(...)` (including nested `record`s and `enum`s) into a constrained-decoding JSON Schema and rehydrates the reply back into a typed Starlark `Record` via `llm_structured(ev, MyRecord)`.
2. **Custom multi-turn chat & tool-calling harnesses** (`messages = lambda ctx: ctx.messages`, `tools = [tool_def(...)]`) — ideal when you want full statechart control over the tool loop, human interrupts, prompt caching (`CachePolicy`), and multimodal `attachment(...)` inputs.

**Model naming:** Pass a model string (`"gpt-6-luna"`, `"gemini-2.5-flash"`, `"claude-sonnet-4-5"`) or `llm_model(model, *, api_key = None, endpoint = None, temperature = None, max_tokens = None, top_p = None, reasoning_effort = None)`. Provider env vars (`OPENAI_API_KEY`, `GEMINI_API_KEY`, `ANTHROPIC_API_KEY`, etc.) are automatically resolved and scrubbed when `api_key` is omitted.

**Pattern A — Single-shot typed `Record` extraction:**
```python
load("@starness/llm", "invoke_llm", "llm_structured", "llm_text")

Summary = record(headline = str, breaking = bool, tags = list[str])

invoke_llm(
    model = "gpt-6-luna",
    system = "Summarize the release.",
    prompt = lambda ctx: ctx.commits,
    context = lambda ctx: {"version": ctx.version},  # optional <context>...</context> block
    schema = Summary,
    stream = True,
    on_done = outcome(
        target = S.next,
        assign = lambda ctx, ev: update(ctx, headline = llm_structured(ev, Summary).headline),
    ),
    on_error = S.failed,
)
```

**Pattern B — Multi-turn tool loop (lossless round-tripping):**
```python
load(
    "@starness/llm",
    "CachePolicy", "ToolChoice", "assistant_msg", "attachment",
    "has_tool_calls", "invoke_llm", "llm_text", "llm_tool_call", "llm_tool_calls",
    "system_msg", "tool_def", "tool_result", "tool_results_msg", "user_msg",
)

BashArgs = record(command = str)

# 1. Append user message (with optional image/PDF attachments):
#    ctx.messages + [user_msg("Inspect screenshot", attachments = [attachment(path = "ui.png")])]
# 2. Invoke LLM with tools + system prompt caching:
invoke_llm(
    model = "gpt-6-luna",
    system = "You are a coding assistant.",
    system_cache = CachePolicy.ephemeral,  # "ephemeral" | "memory" | "5m" | "1h" | "24h"
    messages = lambda ctx: ctx.messages,
    tools = [tool_def("bash", description = "Run a shell command", schema = BashArgs)],
    tool_choice = ToolChoice.auto,         # "auto" | "none" | "required" | "<tool_name>"
    on_done = [
        outcome(
            target = S.acting,
            guard = lambda _c, ev: has_tool_calls(ev),
            assign = lambda ctx, ev: update(
                ctx,
                messages = ctx.messages + [assistant_msg(ev)],  # preserves tool calls + reasoning + thought signatures!
                pending_call = llm_tool_call(ev),
            ),
        ),
        outcome(
            target = S.gather_input,
            assign = lambda ctx, ev: update(ctx, messages = ctx.messages + [assistant_msg(ev)], last_reply = llm_text(ev)),
        ),
    ],
    on_error = S.failed,
)
# 3. After executing the tool in S.acting, append `tool_result(ctx.pending_call, ev["output"])`
#    (passing the call dict preserves BOTH `call_id` and `fn_name` for OpenAI, Anthropic, and Gemini).
#    For parallel tool calls, batch them with `tool_results_msg([tool_result(c, out), ...])`.
```

---

## 3. `@starness/acp` — Orchestrating Existing Coding Harnesses

**When & Why:** Use when a state needs deep, multi-step repository exploration, file editing, or autonomous debugging — delegating to **Claude Code** (`claude_agent`), **Codex** (`codex_agent`), **Antigravity** (`agy_agent`), or any ACP v1 agent (`acp_agent`).

**Typed Handoffs via Turn-Scoped MCP Tools:** Give the agent `read_tool(...)`s (pure Starlark queries over `ctx`) and `artifact_tool(...)`s (validated against a Starlark `record(...)` schema with optional `validate = fn`). If `required = True`, the turn cannot succeed until the agent submits a valid artifact, which you extract cleanly with `artifact(ev, "tool_name", RecordType)` or `artifact_list(ev, "tool_name", RecordType)`.

```python
load(
    "@starness/acp",
    "ArtifactMode", "acp_agent", "agy_agent", "artifact", "artifact_list",
    "artifact_tool", "claude_agent", "codex_agent", "invoke_agent", "read_tool",
)

Plan = record(summary = str, files = list[str])

invoke_agent(
    agent = agy_agent(id = "planner", model = "", cwd = None, timeout_ms = 300000),
    prompt = lambda ctx: "Plan the fix for: " + ctx.task,
    context = lambda ctx: {"errors": ctx.test_errors},
    tools = [
        read_tool("get_errors", description = "Inspect failing test output", handler = lambda ctx, _args: ctx.test_errors),
        artifact_tool("submit_plan", description = "Submit plan", schema = Plan, mode = ArtifactMode.replace, required = True),
    ],
    on_done = outcome(
        target = S.coding,
        assign = lambda ctx, ev: update(ctx, plan = artifact(ev, "submit_plan", Plan).summary),
    ),
    on_error = S.failed,
)
```

---

## 4. `@starness/mcp` — Deterministic MCP Client Calls

**When & Why:** Use when a state should call a specific tool on an external MCP server directly (no LLM in the loop) — e.g., fetching a Linear/GitHub issue before an LLM step or writing results back after approval.

```python
load("@starness/mcp", "McpServer", "invoke_mcp", "mcp_http_server", "mcp_stdio_server")

tracker = mcp_http_server(
    id = "tracker",
    url = "http://127.0.0.1:9000/mcp",
    headers = {"Authorization": secret("TRACKER_TOKEN", prefix = "Bearer ")},
)
# Or stdio: mcp_stdio_server(id = "gh", command = "npx", args = ["-y", "@modelcontextprotocol/server-github"])

invoke_mcp(
    server = tracker,
    tool = "get_issue",
    arguments = lambda ctx: {"id": ctx.issue_id},
    check_tool_error = True,
    on_done = ...,  # ev["output"] -> {"text": str, "data": Any, "is_error": bool}
    on_error = S.failed,
)
```

---

## 5. `@starness/http` & `@starness/exec` — Deterministic I/O

### `@starness/http`
```python
load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

invoke_http(
    request = lambda ctx: http_request(
        method = HttpMethod.POST,
        url = ctx.webhook_url,
        headers = {"Authorization": secret("API_TOKEN", prefix = "Bearer ")},
        json = {"status": "ok"},
        timeout_ms = 15000,
    ),
    on_done = ...,  # ev["output"] -> {"status": int, "ok": bool, "headers": dict, "body": str, "json": Any}
    on_error = S.failed,
)
```

### `@starness/exec`
Subprocesses are killed automatically (`kill_on_drop`) if the state exits or is interrupted. Set `check = False` when you want non-zero exit codes delivered to `on_done` (e.g., passing `bash` tool stderr back to an LLM) instead of `on_error`.

```python
load("@starness/exec", "ExecSpec", "exec_spec", "invoke_cmd")

invoke_cmd(
    cmd = lambda ctx: exec_spec(
        cmd = ["sh", "-c", ctx.command],
        cwd = ctx.cwd if ctx.cwd != "" else None,
        env = {"TOKEN": secret("DEPLOY_TOKEN", optional = True)},
        timeout_ms = 60000,
        check = True,
    ),
    on_done = ...,  # ev["output"] -> {"code": int, "stdout": str, "stderr": str}
    on_error = S.failed,
)
```

---

## 6. `@starness/secrets` — Late-Bound Secret Resolution & Scrubbing

**When & Why:** Keeps plaintext credentials out of `.star` files, machine `context`, SQLite snapshots, JSONL traces, and error messages. Secrets resolve only at the moment an `invoke` runs and are zeroized on drop and scrubbed as `<secret:NAME>`.

```python
load("@starness/secrets", "SecretSource", "secret")

secret("OPENAI_API_KEY")                                                        # env var (default)
secret("/run/secrets/db_pass", source = SecretSource.file)                      # file on disk
secret("PROD_TOKEN", source = SecretSource.keychain, service = "starness", prefix = "Bearer ", optional = False)
```

---

## 7. `@starness/ui` — Declarative & Generative `json-render` UIs

**When & Why:** Attach interactive human-in-the-loop interfaces directly to any waiting `state(..., ui = ui_spec(...))` or `compound_state(...)` — or bind dynamically to a spec generated into `context` via `state(..., ui = ui_bind("/generated_ui"))`. Static UI button/review events are verified by `starness check` against outgoing `on` transitions (excluding `forbid`s), and the same spec renders in both the embedded Web Studio (`starness serve` → `/ui`) and the terminal TUI (`starness run -i`).

```python
load(
    "@starness/ui",
    "ui_badge", "ui_bind", "ui_button", "ui_card", "ui_confirm", "ui_key_value",
    "ui_markdown", "ui_markdown_review", "ui_select", "ui_spec", "ui_stack",
    "ui_table", "ui_text_input", "ui_textarea",
)

S.human_review: state(
    ui = ui_spec(
        ui_stack([
            ui_card("Status", [ui_badge(ui_bind("/status"), variant = "warning")]),
            ui_markdown_review(
                "Implementation Plan",
                markdown = ui_bind("/plan"),
                agent_notes = ui_bind("/notes"),
                comments_path = "/draft_comments",
                approve_event = "APPROVE",
                revise_event = "REQUEST_CHANGES",
            ),
        ]),
    ),
    on = [
        transition(event = "APPROVE", target = S.approved),
        transition(event = "REQUEST_CHANGES", target = S.revising, assign = capture_comments),
    ],
)
```

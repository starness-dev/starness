# Operating, Hosting & Debugging Starness Workflows

When, why, and how to run workflows via CLI, host them as durable HTTP/SSE services (`starness serve`), expose them as ACP agents, or manage secrets.

---

## 1. Local Execution & Interactive Debugging (`starness run`)

Use `starness run` for one-shot automation, CI jobs, or local interactive sessions:

```bash
# Run to completion and stream lossless JSONL traces to stdout
starness run workflow.star --context '{"repo":"floodfx/starness"}' --trace -

# Interactive / human-in-the-loop workflow (events sent line-by-line on stdin)
starness run examples/coding_session_llm.star --events-stdin --trace session.jsonl

# Validate a statechart and its optional external UI view-sheet
starness check examples/plan_review.star --ui examples/plan_review.ui.json

# Interactive terminal TUI (renders --ui view-sheet or falls back to an allowed_events picker)
starness run -i examples/plan_review.star --ui examples/plan_review.ui.json
starness run -i examples/plan_review.star

# Multi-machine HTTP + SSE daemon + embedded Web Studio (/ui)
starness serve examples/plan_review.star --ui examples/plan_review.ui.json --bind 127.0.0.1:8080
```

**Stdin event formats (`--events-stdin`):**
- Bare event name: `APPROVE` or `INTERRUPT`
- JSON event with payload: `{"type":"PROMPT","text":"Check git status with bash"}`
- Test pause helper: `sleep:250` (waits 250ms before dispatching the next line)

**Reading JSONL traces (`--trace -` or `--trace file.jsonl`):**
Every record has a `kind`:
- `started` — Initial active states and context.
- `transition` — `event`, `from`, `to`, `actions`, updated `context`, `status`, `output`, `error`.
- `service_started` / `service_finished` — Lifecycle of an `invoke` (secrets redacted as `<secret:NAME>`).
- `service_progress` — Live asynchronous chunks (`stream`: `agent_text`, `agent_thought`, `tool_call`, `artifact`, `stdout`, `stderr`, `http`, `log`).

---

## 2. Durable Multi-Instance HTTP & SSE Hosting (`starness serve`)

**When & Why:** Use `starness serve` whenever workflows need to:
- Survive restarts or pause for hours/days waiting on human approvals or webhooks (`--db starness.db` uses SQLite WAL persistence and automatically rehydrates waiting/active instances on restart).
- Run many concurrent instances of one or more `.star` machines behind a single HTTP API.
- Stream live progress (LLM tokens, tool calls, state transitions) to a web UI, Slack bot, or CLI over Server-Sent Events (SSE).

```bash
# Serve a single file or an entire directory of .star workflows
starness serve examples/ --bind 127.0.0.1:8080 --db starness.db
```

### Lifecycle & Mental Model
1. **Create an instance** (`POST /machines/:machine/instances`): Starts the machine in its initial state and returns an `InstanceSnapshot`. **The instance ID field is `.id`**.
2. **Subscribe to live trace** (`GET /machines/:machine/instances/:id/trace`): Replays history from the start and streams live SSE events (`text/event-stream`, or pass `?format=jsonl`).
3. **Dispatch events** (`POST /machines/:machine/instances/:id/events`): **Non-blocking!** It applies the synchronous transition (e.g., `gather_input` → `working.thinking`), kicks off any `invoke`s asynchronously in the background, and immediately returns the updated `InstanceSnapshot`.
4. **Inspect current state** (`GET /machines/:machine/instances/:id`): Poll or fetch after an SSE `transition` event to read final `context`, `output`, and `status`:
   - `active` — Non-final state with an `invoke` or `after` timer currently running.
   - `waiting` — Non-final quiescent state waiting on an external event (e.g., human approval or next `PROMPT`).
   - `completed` — Reached a top-level `final_state` (`done: true`).
   - `failed` — Terminated with an unhandled runtime error (`error` populated).
   - `cancelled` — Stopped via `DELETE /machines/:machine/instances/:id`.

### Complete `curl` Walkthrough

```bash
# 1. Start an instance (optional: {"id":"pr-42","context":{...},"idempotent":true})
ID=$(curl -s -X POST http://127.0.0.1:8080/machines/coding_session_llm/instances \
  -H 'content-type: application/json' -d '{}' | jq -r .id)

# 2. Stream live tokens, tool calls, and transitions over SSE
curl -N http://127.0.0.1:8080/machines/coding_session_llm/instances/$ID/trace &

# 3. Send an event (non-blocking)
curl -s -X POST http://127.0.0.1:8080/machines/coding_session_llm/instances/$ID/events \
  -H 'content-type: application/json' \
  -d '{"type":"PROMPT","text":"Check git status with bash"}' | jq '{status, active_states}'

# 4. Inspect snapshot once status returns to "waiting" or "completed"
curl -s http://127.0.0.1:8080/machines/coding_session_llm/instances/$ID | jq '{status, active_states, context}'

# 5. Cancel (add ?purge=true to also delete from SQLite)
curl -s -X DELETE "http://127.0.0.1:8080/machines/coding_session_llm/instances/$ID"
```

### All HTTP Routes
- `GET /ui` (or `GET /` with `Accept: text/html`) — Embedded Starness Studio (`@json-render/react` + `<MarkdownReview>` + Mermaid diagram + live SSE trace). Set `STARNESS_UI_DIR` to serve a custom local build.
- `GET /health` — Liveness, loaded machines, live actor count, DB target.
- `GET /trace` — Server-wide SSE / JSONL firehose across all machines and instances.
- `GET /machines` / `GET /machines/:machine` / `GET /machines/:machine/diagram`.
- `POST /machines/:machine/instances` / `GET /machines/:machine/instances` (`?status=active|waiting|completed|failed|cancelled`).
- `GET /machines/:machine/instances/:id` — Returns `InstanceSnapshot` enriched with active `ui` spec and `allowed_events`.
- `DELETE /machines/:machine/instances/:id` (`?purge=true`).
- `POST /machines/:machine/instances/:id/events` / `POST /machines/:machine/instances/:id/cancel`.
- `GET /machines/:machine/instances/:id/trace` (SSE or `?format=jsonl`).

---

## 3. Secrets, ACP Server & Editor LSP

- **OS Keychain secrets**:
  ```bash
  pbpaste | starness secrets set OPENAI_API_KEY
  starness secrets check OPENAI_API_KEY
  starness secrets delete OPENAI_API_KEY
  ```
- **Expose any `.star` workflow as an ACP v1 Agent** (for Zed, Neovim, JetBrains, or parent workflows):
  ```bash
  starness acp examples/multi_agent_acp.star
  ```
- **Starness Editor LSP** (live static typechecking & autocomplete for the Starness DSL and `@starness/*` modules):
  ```bash
  starness lsp
  ```

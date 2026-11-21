# Starness

Typed Starlark Harel statechart engine in Rust, backed by `xstate-statemachine` (`xstate-core` + `xstate-tokio`).

## Workspace Layout

- `crates/starness-ir` — Pure Rust statechart intermediate representation and graph verifier (`MachineDef::validate`).
- `crates/starness-starlark` — Strongly typed Starlark 0.14 Harel / XState v5 DSL (`state_id`, `state_space`, `record`, `transition`, `internal_transition`, `forbid`, `always`, `after`, `invoke`, `outcome`, `on_done`, `shallow_history`, `deep_history`, `state_in` / `all_of` / `any_of` / `not_` / `named_guard`, `raise_event` / `cancel_send` / `emit_event` / `send_to` / `send_parent` / `log_action`, `tags` / `meta` / `output` / `description`, `state`, `compound_state`, `parallel_state`, `final_state`, `machine`), ahead-of-time static typechecker, multi-file `load()` compiler, and virtual `@starness/*` standard modules.
- `crates/starness-runtime` — Compiles `CompiledChart` into `xstate_core::Machine`, bridges frozen Starlark guards, delays, `invoke` input builders & `assign` reducers with Rust `HostCallbacks` (`StarnessSyncRunner` + Tokio `StarnessActor` + multi-instance SQLite HTTP/SSE `serve` daemon + lossless off-thread `TraceHub` / `TraceReceiver` observer stream + Harel Mermaid exporter), and ships built-in runtime drivers for HTTP, subprocess execution, MCP clients, `decision-rs` typed decisions, OS keychain/env/file secrets, and Agent Client Protocol (ACP v1).
- `bins/starness-cli` (`starness`) — Standalone CLI (`check`, `diagram`, `run` with `--events-stdin` & `--trace`, `serve` with `--db`, `secrets`, `acp`, `lsp`).

## Built-in `@starness/*` Standard Modules (Zero Rust Required)

- `load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")` — Async HTTP/TLS requests via `reqwest` (`rustls`).
- `load("@starness/exec", "ExecSpec", "exec_spec", "invoke_cmd")` — Async subprocess execution with automatic cancellation (`kill_on_drop`) on state exit.
- `load("@starness/mcp", "McpServer", "invoke_mcp", "mcp_http_server", "mcp_stdio_server")` — Deterministic MCP client invocations over `stdio` or Streamable HTTP (with automatic JSON payload extraction).
- `load("@starness/acp", "AcpAgent", "ArtifactMode", "acp_agent", "artifact", "artifact_list", "artifact_tool", "invoke_agent", "read_tool")` — Drives ACP v1, Claude Code, and Codex coding agents over `stdio`, with pushed state `context` and turn-scoped MCP `read_tool`s / Starlark `record`-validated `artifact_tool`s (`submit_plan`, `submit_review`, etc.).
- `load("@starness/decide", "choice", "invoke_decision", "is_yes", "jev", "level", "noul", "prob", "score", "selected")` — Provider-neutral typed `noul`, `choice`, and `score` decisions via `decision-rs` across TypeSafe System One (`jev`), Gemini, OpenAI, OpenRouter, and Cloudflare Clef.
- `load("@starness/secrets", "SecretSource", "secret")` — Late-bound secrets (`env`, `file`, and OS `keychain`) resolved in `ZeroizeOnDrop` memory with automatic `<secret:NAME>` trace/error redaction.

## Quick Start

```bash
# Typecheck and validate statecharts (offline, no I/O executed)
cargo run -p starness-cli -- check examples/*.star

# Render a Harel Mermaid stateDiagram-v2 graph
cargo run -p starness-cli -- diagram examples/deployment_pipeline.star

# Execute a multi-agent workflow and stream lossless JSONL traces (`-` for stdout or a file path)
cargo run -p starness-cli -- run examples/multi_agent_acp.star \
  --trace trace.jsonl \
  --context '{"task":"Add retry jitter to @starness/http"}'

# Stream live external events over stdin while observing JSONL traces on stdout
cargo run -p starness-cli -- run examples/deployment_pipeline.star \
  --events-stdin \
  --trace -

# Host multiple machines & instances on one port (in-memory SQLite by default, or WAL file via `--db`)
cargo run -p starness-cli -- serve examples/ \
  --bind 127.0.0.1:8080 \
  --db starness.db

# Store/check secrets in the OS Keychain
pbpaste | cargo run -p starness-cli -- secrets set TYPESAFE_API_KEY
cargo run -p starness-cli -- secrets check TYPESAFE_API_KEY

# Expose any .star workflow as an ACP v1 Agent over stdio (for Zed, Neovim, JetBrains, etc.)
cargo run -p starness-cli -- acp examples/multi_agent_acp.star

# Launch the Starlark LSP server over stdio
cargo run -p starness-cli -- lsp
```

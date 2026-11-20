# Starchart

Typed Starlark Harel statechart engine in Rust, backed by `xstate-statemachine` (`xstate-core` + `xstate-tokio`).

## Workspace Layout

- `crates/starchart-ir` — Pure Rust statechart intermediate representation and graph verifier (`MachineDef::validate`).
- `crates/starchart-starlark` — Strongly typed Starlark 0.14 Harel / XState v5 DSL (`state_id`, `state_space`, `record`, `transition`, `internal_transition`, `forbid`, `always`, `after`, `invoke`, `outcome`, `on_done`, `shallow_history`, `deep_history`, `state_in` / `all_of` / `any_of` / `not_` / `named_guard`, `raise_event` / `cancel_send` / `emit_event` / `send_to` / `send_parent` / `log_action`, `tags` / `meta` / `output` / `description`, `state`, `compound_state`, `parallel_state`, `final_state`, `machine`), ahead-of-time static typechecker, multi-file `load()` compiler, and virtual `@starchart/*` standard modules.
- `crates/starchart-runtime` — Compiles `CompiledChart` into `xstate_core::Machine`, bridges frozen Starlark guards, delays, `invoke` input builders & `assign` reducers with Rust `HostCallbacks` (`StarchartSyncRunner` + Tokio `StarchartActor` + lossless off-thread `TraceHub` / `TraceReceiver` observer stream + Harel Mermaid exporter), and ships built-in runtime drivers for HTTP, subprocess execution, and Agent Client Protocol (ACP v1).
- `bins/starchart-cli` (`starchart`) — Standalone CLI (`check`, `diagram`, `run` with `--trace`, `acp`, `lsp`).

## Built-in `@starchart/*` Standard Modules (Zero Rust Required)

- `load("@starchart/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")` — Async HTTP/TLS requests via `reqwest` (`rustls`).
- `load("@starchart/exec", "ExecSpec", "exec_spec", "invoke_cmd")` — Async subprocess execution with automatic cancellation (`kill_on_drop`) on state exit.
- `load("@starchart/acp", "AcpAgent", "ArtifactMode", "acp_agent", "artifact_tool", "invoke_agent", "read_tool")` — Drives ACP v1, Claude Code, and Codex coding agents over `stdio`, with turn-scoped MCP `read_tool`s and Starlark `record`-validated `artifact_tool`s (`submit_plan`, `submit_review`, etc.).

## Quick Start

```bash
# Typecheck and validate statecharts (offline, no I/O executed)
cargo run -p starchart-cli -- check examples/*.star

# Render a Harel Mermaid stateDiagram-v2 graph
cargo run -p starchart-cli -- diagram examples/deployment_pipeline.star

# Execute a multi-agent workflow and stream lossless JSONL traces (`-` for stdout or a file path)
cargo run -p starchart-cli -- run examples/multi_agent_acp.star \
  --trace trace.jsonl \
  --context '{"task":"Add retry jitter to @starchart/http"}'

# Expose any .star workflow as an ACP v1 Agent over stdio (for Zed, Neovim, JetBrains, etc.)
cargo run -p starchart-cli -- acp examples/multi_agent_acp.star

# Launch the Starlark LSP server over stdio
cargo run -p starchart-cli -- lsp
```

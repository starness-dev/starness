# Starchart

Typed Starlark Harel statechart engine in Rust, backed by `xstate-statemachine` (`xstate-core` + `xstate-tokio`).

## Workspace Layout

- `crates/starchart-ir` — Pure Rust statechart intermediate representation and graph verifier (`MachineDef::validate`).
- `crates/starchart-starlark` — Strongly typed Starlark 0.14 DSL (`state_id`, `state_space`, `record`, `transition`, `internal_transition`, `all_of` / `any_of` / `not_` / `named_guard`, `state`, `compound_state`, `parallel_state`, `final_state`, `machine`), ahead-of-time static typechecker, and multi-file `load()` compiler.
- `crates/starchart-runtime` — Compiles `CompiledChart` into `xstate_core::Machine` and bridges frozen Starlark guards & `assign` reducers with Rust `HostCallbacks` (`ChartSyncRunner` + Tokio `ChartActor` + Harel Mermaid exporter).
- `bins/starchart-cli` (`starchart`) — CLI (`check`, `diagram`, `run`, `lsp`).

## Quick Start

```bash
# Typecheck and validate statecharts
cargo run -p starchart-cli -- check examples/order_workflow.star examples/deployment_pipeline.star

# Render a Mermaid stateDiagram-v2 graph
cargo run -p starchart-cli -- diagram examples/order_workflow.star

# Execute a statechart against events
cargo run -p starchart-cli -- run examples/order_workflow.star \
  --event '{"type":"APPROVED","amount":250}' \
  --event CHARGE_OK

# Launch the Starlark LSP server over stdio
cargo run -p starchart-cli -- lsp
```

# Developing Starness (Agent Contributor Guide)

> **Authoring `.star` workflows?** Read [`skills/starness/SKILL.md`](skills/starness/SKILL.md) (and its `references/`). Do not duplicate DSL docs here.

## Workspace Crates
- `crates/starness-ir` — Pure Rust statechart IR and static graph verifier (`MachineDef::validate`).
- `crates/starness-starlark` — Starlark 0.14 DSL, static typechecker, multi-file `load()` compiler, JSON Schema derivation, and `@starness/*` virtual modules.
- `crates/starness-runtime` — `xstate-statemachine` compiler, `StarnessSyncRunner` / `StarnessActor`, SQLite HTTP/SSE `serve` daemon, `TraceHub`, Mermaid exporter, and built-in drivers (`http`, `exec`, `mcp`, `acp`, `decide`, `llm`, `secrets`).
- `bins/starness-cli` (`starness`) — CLI (`check`, `diagram`, `run`, `serve`, `secrets`, `acp`, `lsp`) and E2E integration tests.

## Verification Checklist
Run before committing Rust or example changes:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -q -p starness-cli -- check examples/*.star
```

## Repo Conventions
- **Project name is `starness`** (all crates, CLI, env vars, `@starness/*` modules).
- **Secrets hygiene**: Never store `secret(...)` in persisted `context`; always route credentials through `SecretScrubber` + `secrecy::SecretString`.
- **`serde_json/arbitrary_precision`**: Enabled workspace-wide by `starlark 0.14` — avoid `#[serde(flatten)]` on deserialized structs with numeric fields (`serde-rs/json#505`).
- **Logical commits**: Split changes cleanly by layer (`starlark`, `runtime`, `examples/tests/docs`).

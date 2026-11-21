# Hierarchical State Machines in Starness

This reference explains **when, why, and how** to use Starness's statechart primitives to structure real workflows cleanly.

---

## 1. Core Building Blocks: `state_space`, `record`, `enum`, `update`

Declare all state identifiers, event enums, and context records upfront so `starness check` validates every reference statically:

```python
Event = enum("START", "PAUSE", "RESUME", "CANCEL")

Ctx = record(
    attempt = field(int, 0),
    status = field(str, "init"),
    error = field(str | None, None),
)

S = state_space("idle", "working", "step_a", "step_b", "paused", "hist", "done", "failed")
```

- **`update(ctx, field = val)`**: Returns a validated shallow copy of a `record` (or `dict`). Unknown field names or mismatched types fail immediately.
- **`EnumType.VAL.value`**: Returns the underlying string (`"START"`) when storing an enum value in a `str` field or comparing against JSON output.

---

## 2. Nested States (`compound_state`) — Shared Lifecycle & Cancellation

**When to use:** Whenever multiple states share a common `INTERRUPT`, `CANCEL`, `PAUSE`, or timeout rule — such as a multi-turn agent loop (`thinking <-> acting`) or a multi-step deployment rollout.

**Why:** Defining `INTERRUPT` once on the parent `compound_state` handles it from *any* child state and automatically cancels the active child `invoke` (killing subprocesses via `kill_on_drop` or aborting an in-flight LLM stream).

```python
S.working: compound_state(
    initial = S.thinking,
    on = [
        transition(event = "INTERRUPT", target = S.gather_input),
    ],
    states = {
        S.thinking: state(invoke = ...),
        S.acting: state(invoke = ...),
    },
)
```

---

## 3. Concurrent Regions (`parallel_state`) — Fan-Out & Quorum Convergence

**When to use:** Whenever independent tasks should run simultaneously and join once all finish — e.g., multi-harness PR review quorum (`claude` + `codex` + `agy`), multi-model consensus, or parallel smoke tests + security scans.

**Why:** Each child of `parallel_state` is a `compound_state` with its own `initial` state and `final_state()`. You do not write manual join counters for completion: `parallel_state(..., on_done = [...])` fires automatically the moment **every** region reaches its `final_state()`.

```python
S.reviewing: parallel_state(
    states = {
        S.region_a: compound_state(
            initial = S.run_a,
            states = {
                S.run_a: state(invoke = invoke_llm(..., on_done = outcome(target = S.done_a, assign = save_a), on_error = S.done_a)),
                S.done_a: final_state(),
            },
        ),
        S.region_b: compound_state(
            initial = S.run_b,
            states = {
                S.run_b: state(invoke = invoke_llm(..., on_done = outcome(target = S.done_b, assign = save_b), on_error = S.done_b)),
                S.done_b: final_state(),
            },
        ),
    },
    on_done = [
        outcome(target = S.approved, guard = unanimous_approval),
        outcome(target = S.awaiting_human),
    ],
)
```

**Tip:** Use a Starlark helper function `def reviewer(agent, run_state, done_state): ...` to stamp out identical parallel regions without duplicating code.

---

## 4. Pause & Resume (`shallow_history` / `deep_history`)

**When to use:** Long-running workflows (deployments, approvals, investigations) that can be paused at any substate and resumed without restarting from step 1.

**How:** Place a `shallow_history(target = S.default_child)` (remembers direct child) or `deep_history(target = ...)` (remembers nested leaf) inside the `compound_state`, and target that history state on `RESUME`:

```python
S.deploying: compound_state(
    initial = S.canary,
    on = [transition(event = "PAUSE", target = S.paused)],
    states = {
        S.hist: shallow_history(target = S.canary),
        S.canary: state(...),
        S.baking: state(...),
        S.promoting: state(...),
    },
),
S.paused: state(
    on = [transition(event = "RESUME", target = S.hist)],
),
```

---

## 5. Transitions, Outcomes, Timers & Guards

### External vs Internal vs Forbidden Transitions
- **`transition(event = ..., target = S.next, guard = fn, assign = fn, actions = [], reenter = False)`** — Exits the current state (cancelling active invokes/timers), runs `assign` and `actions`, and enters `target`.
- **`internal_transition(event = ..., guard = fn, assign = fn, actions = [])`** — Updates `ctx` or runs actions **without** exiting/re-entering the state (keeps active invokes/timers alive).
- **`forbid(event = ...)`** — Explicitly swallows an event in a child state so a parent `compound_state` cannot handle it (e.g., forbidding `PAUSE` during a critical commit step).

### Ordered Outcome Branches (`on_done`, `on_error`, `always`, `after`)
`outcome(target = ..., guard = ..., assign = ..., actions = [])` branches are evaluated **in list order** — the first branch whose `guard` returns `True` (or has no guard) wins. Always place your specific guarded cases first and your default fallback last:

```python
on_done = [
    outcome(target = S.fast_path, guard = is_high_confidence, assign = save_result),
    outcome(target = S.human_review, assign = save_result),  # fallback
]
```

### Timers & Exponential Backoff (`after`)
`after` fires after a fixed millisecond delay or a dynamic `lambda ctx: int` delay. If another event leaves the state first, the timer is cancelled automatically:

```python
def backoff_ms(ctx: Ctx) -> int:
    return 500 * (2 ** ctx.attempt)

S.retrying: state(
    after = after(
        delay = backoff_ms,
        target = S.calling_api,
        assign = lambda ctx, _ev: update(ctx, attempt = ctx.attempt + 1),
    ),
)
```

### Guards & Built-in Actions
- **Guards**: `lambda ctx, ev: bool`, `state_in(S.other_region_state)` (check active state in a sibling parallel region), `all_of(g1, g2)`, `any_of(g1, g2)`, `not_(g)`, `named_guard("HOST_GUARD")`.
- **Actions**: `raise_event(event = ..., payload = None, delay = None, id = None)`, `cancel_send(id = ...)`, `emit_event(event = ..., payload = None)`, `send_to(to = ..., event = ..., payload = None, delay = None, id = None)`, `send_parent(event = ..., payload = None)`, `log_action(expr)`.

---

## 6. Modularizing Across Multiple `.star` Files

Starlark supports `load("relative_file.star", "SymbolA", "SymbolB")` for sharing `record(...)` schemas, `enum(...)` definitions, `llm_model(...)` configs, and guard/reducer helpers across workflows. `starness check` typechecks the entire dependency graph together.

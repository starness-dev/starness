# Showcase of `always` (eventless choice routing) + `after` (fixed & dynamic Starlark timers):
# - `connecting` times out after 1000ms if neither `CONNECTED` nor `CONN_ERR` arrives
# - `evaluate_retry` uses `always = [...]` to route immediately without waiting for an event
# - `waiting_backoff` uses a pure Starlark function `backoff_ms(ctx, event) -> int` for exponential backoff

ConnEvent = enum("CONNECTED", "CONN_ERR", "ABORT")
Action = enum("OPEN_SOCKET", "ALERT_EXHAUSTED")

RetryCtx = record(
    attempts = int,
    max_attempts = int,
)

def has_retries_left(ctx: RetryCtx, event: dict) -> bool:
    return ctx.attempts <= ctx.max_attempts

def record_attempt(ctx: RetryCtx, event: dict) -> RetryCtx:
    return RetryCtx(
        attempts = ctx.attempts + 1,
        max_attempts = ctx.max_attempts,
    )

def backoff_ms(ctx: RetryCtx, event: dict) -> int:
    # Exponential backoff: 200ms, 400ms, 800ms...
    shift = ctx.attempts - 1 if ctx.attempts > 0 else 0
    return 200 * (1 << shift)

S = state_space(
    "connecting",
    "evaluate_retry",
    "waiting_backoff",
    "online",
    "failed",
)

chart: StateMachine = machine(
    id = "retry_backoff",
    initial = S.connecting,
    context = RetryCtx(attempts = 0, max_attempts = 2),
    states = {
        S.connecting: state(
            entry = [Action.OPEN_SOCKET],
            on = [
                transition(event = ConnEvent.CONNECTED, target = S.online),
                transition(
                    event = ConnEvent.CONN_ERR,
                    target = S.evaluate_retry,
                    assign = record_attempt,
                ),
            ],
            after = after(
                delay = 1000,
                target = S.evaluate_retry,
                assign = record_attempt,
            ),
        ),
        S.evaluate_retry: state(
            always = [
                outcome(target = S.waiting_backoff, guard = has_retries_left),
                outcome(target = S.failed, actions = [Action.ALERT_EXHAUSTED]),
            ],
        ),
        S.waiting_backoff: state(
            after = after(delay = backoff_ms, target = S.connecting),
            on = [transition(event = ConnEvent.ABORT, target = S.failed)],
        ),
        S.online: final_state(),
        S.failed: final_state(),
    },
)

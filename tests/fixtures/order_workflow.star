# Static types enabled: helper functions are statically typechecked!
OrderEvent = enum("VALIDATE", "APPROVED", "REJECTED", "CHARGE_OK", "CHARGE_ERR", "CANCEL")
Action = enum("LOG_REJECTION", "CHARGE_CARD", "REFUND", "NOTIFY_USER")

def is_under_limit(ctx: dict, event: dict) -> bool:
    return event.get("amount", 0) < ctx.get("limit", 1000)

def increment_attempts(ctx: dict, event: dict) -> dict:
    next_ctx = dict(ctx)
    next_ctx["attempts"] = ctx.get("attempts", 0) + 1
    return next_ctx

# Declare typed state handles up front (prevents string typos in transitions)
validating = state_id("validating")
processing = state_id("processing")
charging   = state_id("charging")
fulfilled  = state_id("fulfilled")
rejected   = state_id("rejected")
failed     = state_id("failed")

def retryable_step(on_ok: StateId, on_err: StateId) -> StateNode:
    return state(
        entry = [Action.CHARGE_CARD],
        on = [
            transition(event = OrderEvent.CHARGE_OK, target = on_ok),
            transition(
                event = OrderEvent.CHARGE_ERR,
                target = on_err,
                assign = increment_attempts,
            ),
        ],
    )

chart: StateMachine = machine(
    id = "order_workflow",
    initial = validating,
    context = {"limit": 1000, "attempts": 0},
    states = {
        validating: state(
            on = [
                transition(
                    event = OrderEvent.APPROVED,
                    target = processing,
                    guard = is_under_limit,
                ),
                transition(
                    event = OrderEvent.REJECTED,
                    target = rejected,
                    actions = [Action.LOG_REJECTION],
                ),
            ],
        ),
        processing: compound_state(
            initial = charging,
            on = [
                transition(event = OrderEvent.CANCEL, target = rejected),
            ],
            states = {
                charging: retryable_step(on_ok = fulfilled, on_err = failed),
            },
        ),
        fulfilled: final_state(entry = [Action.NOTIFY_USER]),
        rejected: final_state(),
        failed: final_state(),
    },
)

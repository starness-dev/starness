# Showcase of `invoke(...)` + `outcome(...)` for async/sync service execution:
# - Entering `authorizing` automatically starts `Service.AUTHORIZE_CARD`
# - `on_done` receives `event["output"]` in a pure Starlark reducer and branches via guards
# - `on_error` catches service failures and records `event["error"]`
# - Sending `CANCEL` while `authorizing` is active cancels the in-flight service immediately

CheckoutEvent = enum("SUBMIT_PAYMENT", "APPROVE_3DS", "CANCEL")
Service = enum("AUTHORIZE_CARD", "CAPTURE_FUNDS")
Action = enum("SEND_RECEIPT", "LOG_DECLINE")

CheckoutCtx = record(
    amount_cents = int,
    requires_3ds = bool,
    auth_id = str,
    error_reason = str,
)

def needs_3ds(ctx: CheckoutCtx, event: dict) -> bool:
    return bool(event.get("output", {}).get("requires_3ds", False))

def record_auth(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    out = event.get("output", {})
    return CheckoutCtx(
        amount_cents = ctx.amount_cents,
        requires_3ds = bool(out.get("requires_3ds", False)),
        auth_id = str(out.get("auth_id", "")),
        error_reason = "",
    )

def record_failure(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    return CheckoutCtx(
        amount_cents = ctx.amount_cents,
        requires_3ds = ctx.requires_3ds,
        auth_id = ctx.auth_id,
        error_reason = str(event.get("error", "service_error")),
    )

S = state_space(
    "cart",
    "authorizing",
    "challenge_3ds",
    "capturing",
    "completed",
    "declined",
    "cancelled",
)

chart: StateMachine = machine(
    id = "payment_checkout",
    initial = S.cart,
    context = CheckoutCtx(
        amount_cents = 12500,
        requires_3ds = False,
        auth_id = "",
        error_reason = "",
    ),
    states = {
        S.cart: state(
            on = [transition(event = CheckoutEvent.SUBMIT_PAYMENT, target = S.authorizing)],
        ),
        S.authorizing: state(
            invoke = invoke(
                src = Service.AUTHORIZE_CARD,
                id = "card_auth",
                input = {"currency": "USD"},
                on_done = [
                    outcome(
                        target = S.challenge_3ds,
                        guard = needs_3ds,
                        assign = record_auth,
                    ),
                    outcome(
                        target = S.capturing,
                        assign = record_auth,
                    ),
                ],
                on_error = outcome(
                    target = S.declined,
                    assign = record_failure,
                    actions = [Action.LOG_DECLINE],
                ),
            ),
            on = [transition(event = CheckoutEvent.CANCEL, target = S.cancelled)],
        ),
        S.challenge_3ds: state(
            on = [
                transition(event = CheckoutEvent.APPROVE_3DS, target = S.capturing),
                transition(event = CheckoutEvent.CANCEL, target = S.cancelled),
            ],
        ),
        S.capturing: state(
            invoke = invoke(
                src = Service.CAPTURE_FUNDS,
                id = "capture_funds",
                on_done = outcome(
                    target = S.completed,
                    actions = [Action.SEND_RECEIPT],
                ),
                on_error = outcome(
                    target = S.declined,
                    assign = record_failure,
                    actions = [Action.LOG_DECLINE],
                ),
            ),
        ),
        S.completed: final_state(),
        S.declined: final_state(),
        S.cancelled: final_state(),
    },
)

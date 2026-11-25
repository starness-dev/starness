# Zero-compile HTTP checkout workflow using `@starness/http`:
# - Dynamically builds POST requests from typed `CheckoutCtx`
# - Retries `429`/`5xx` and transient network failures via `S.retry_backoff`
#   using a dynamic Starlark `after(delay = backoff_ms)` exponential timer
# - Supports `CANCEL` while in-flight or waiting in backoff
# - Branches on `200` + `paid == True` to fire a follow-up webhook POST

load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

CheckoutEvent = enum("CANCEL")

CheckoutCtx = record(
    order_id = str,
    amount_cents = field(int, 4900),
    api_base = field(str, "http://127.0.0.1:8080"),
    charge_id = field(str, ""),
    attempts = field(int, 0),
    max_attempts = field(int, 3),
    base_backoff_ms = field(int, 25),
)

S = state_space(
    "charging",
    "retry_backoff",
    "notifying",
    "completed",
    "declined",
    "cancelled",
    "failed",
)

def build_charge_request(ctx: CheckoutCtx) -> HttpRequest:
    return http_request(
        method = HttpMethod.POST,
        url = ctx.api_base + "/v1/charges",
        headers = {"Idempotency-Key": ctx.order_id},
        json = {
            "order_id": ctx.order_id,
            "amount_cents": ctx.amount_cents,
            "currency": "USD",
        },
        timeout_ms = 5000,
    )

def build_webhook_request(ctx: CheckoutCtx) -> HttpRequest:
    return http_request(
        method = HttpMethod.POST,
        url = ctx.api_base + "/v1/webhooks/order-paid",
        json = {"order_id": ctx.order_id, "charge_id": ctx.charge_id},
        timeout_ms = 5000,
    )

def is_2xx_paid(ctx: CheckoutCtx, event: dict) -> bool:
    res = event["output"]
    body = res.get("body", {})
    return res.get("status") == 200 and type(body) == "dict" and body.get("paid", False) == True

def is_retryable_status(ctx: CheckoutCtx, event: dict) -> bool:
    status = event.get("output", {}).get("status", 503)
    return status in [429, 502, 503, 504] and ctx.attempts < ctx.max_attempts

def backoff_ms(ctx: CheckoutCtx, event: dict) -> int:
    shift = ctx.attempts - 1 if ctx.attempts > 0 else 0
    return ctx.base_backoff_ms * (1 << shift)

chart: StateMachine = machine(
    id = "checkout_http",
    description = "Idempotent HTTP charge + exponential backoff retry + order-paid webhook",
    initial = S.charging,
    context = CheckoutCtx(order_id = "ord_1042"),
    states = {
        S.charging: state(
            invoke = invoke_http(
                request = build_charge_request,
                on_done = [
                    outcome(
                        target = S.notifying,
                        guard = is_2xx_paid,
                        assign = lambda ctx, ev: update(
                            ctx,
                            charge_id = str(ev["output"]["body"].get("charge_id", "")),
                        ),
                    ),
                    outcome(
                        target = S.retry_backoff,
                        guard = is_retryable_status,
                        assign = lambda ctx, _ev: update(ctx, attempts = ctx.attempts + 1),
                    ),
                    outcome(target = S.declined),
                ],
                on_error = [
                    outcome(
                        target = S.retry_backoff,
                        guard = lambda ctx, _ev: ctx.attempts < ctx.max_attempts,
                        assign = lambda ctx, _ev: update(ctx, attempts = ctx.attempts + 1),
                    ),
                    outcome(target = S.failed),
                ],
            ),
            on = [transition(event = CheckoutEvent.CANCEL, target = S.cancelled)],
        ),
        S.retry_backoff: state(
            after = after(delay = backoff_ms, target = S.charging),
            on = [transition(event = CheckoutEvent.CANCEL, target = S.cancelled)],
        ),
        S.notifying: state(
            invoke = invoke_http(
                request = build_webhook_request,
                on_done = S.completed,
                on_error = S.failed,
            ),
        ),
        S.completed: final_state(output = {"status": "paid_and_notified"}),
        S.declined: final_state(output = {"status": "declined"}),
        S.cancelled: final_state(output = {"status": "cancelled"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

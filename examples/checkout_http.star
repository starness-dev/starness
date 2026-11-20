# Standalone HTTP workflow using `@starchart/http` (no Rust code required):
# - Dynamically builds POST requests from typed `CheckoutCtx`
# - Retries `429`/`5xx` responses up to `max_attempts` (`reenter = True`)
# - Branches on `200` + `paid == True` to fire a follow-up webhook POST

load("@starchart/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

CheckoutCtx = record(
    order_id = str,
    amount_cents = int,
    api_base = str,
    charge_id = str,
    attempts = int,
    max_attempts = int,
)

S = state_space("charging", "notifying", "completed", "declined", "failed")

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

def can_retry_http(ctx: CheckoutCtx, event: dict) -> bool:
    status = event.get("output", {}).get("status", 503)
    return status in [429, 502, 503, 504] and ctx.attempts < ctx.max_attempts

def can_retry_network_err(ctx: CheckoutCtx, event: dict) -> bool:
    return ctx.attempts < ctx.max_attempts

def save_charge_id(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    return CheckoutCtx(
        order_id = ctx.order_id,
        amount_cents = ctx.amount_cents,
        api_base = ctx.api_base,
        charge_id = str(event["output"]["body"].get("charge_id", "")),
        attempts = ctx.attempts,
        max_attempts = ctx.max_attempts,
    )

def bump_attempts(ctx: CheckoutCtx, event: dict) -> CheckoutCtx:
    return CheckoutCtx(
        order_id = ctx.order_id,
        amount_cents = ctx.amount_cents,
        api_base = ctx.api_base,
        charge_id = ctx.charge_id,
        attempts = ctx.attempts + 1,
        max_attempts = ctx.max_attempts,
    )

chart: StateMachine = machine(
    id = "checkout_http",
    initial = S.charging,
    context = CheckoutCtx(
        order_id = "ord_1042",
        amount_cents = 4900,
        api_base = "http://127.0.0.1:8080",
        charge_id = "",
        attempts = 0,
        max_attempts = 3,
    ),
    states = {
        S.charging: state(
            invoke = invoke_http(
                request = build_charge_request,
                on_done = [
                    outcome(target = S.notifying, guard = is_2xx_paid, assign = save_charge_id),
                    outcome(
                        target = S.charging,
                        guard = can_retry_http,
                        assign = bump_attempts,
                        reenter = True,
                    ),
                    outcome(target = S.declined),
                ],
                on_error = [
                    outcome(
                        target = S.charging,
                        guard = can_retry_network_err,
                        assign = bump_attempts,
                        reenter = True,
                    ),
                    outcome(target = S.failed),
                ],
            ),
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
        S.failed: final_state(output = {"status": "failed"}),
    },
)

# Zero-compile progressive deployment workflow using `@starchart/exec` + `@starchart/http`:
# - `awaiting_approval`: collects live `APPROVE` events until quorum, then starts on `START_ROLLOUT`
# - `preflight`: queries a deploy-window HTTP endpoint to ensure no active change freeze
# - `rolling_out`: compound superstate with:
#     * `shallow_history` (`S.rollout_hist`) so `PAUSE` -> `RESUME` returns to the exact active stage
#     * superstate `ROLLBACK` handler that cancels any in-flight deploy/check and triggers rollback
#     * canary rollout (`invoke_cmd`) -> timed bake loop (`after` + `invoke_http` metrics check)
#     * parallel `smoke_tests` + `security_scan` (`parallel_state` + `invoke_cmd`) -> `promoting`
# - `rolling_back`: executes rollback command and posts an incident alert webhook (`invoke_http`)

load("@starchart/exec", "invoke_cmd")
load("@starchart/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")

DeployEvent = enum(
    "APPROVE",
    "START_ROLLOUT",
    "PAUSE",
    "RESUME",
    "ROLLBACK",
)

DeployCtx = record(
    service = field(str, "payments-api"),
    version = field(str, "2026.11.20"),
    control_api = field(str, "http://127.0.0.1:9090"),
    approvals = field(int, 0),
    required_approvals = field(int, 2),
    approved_by = field(list[str], []),
    bake_checks = field(int, 0),
    required_bake_checks = field(int, 2),
    bake_interval_ms = field(int, 1000),
    max_error_rate = field(float, 0.01),
    canary_cmd = field(list[str], ["sh", "-c", "echo deploying canary"]),
    smoke_cmd = field(list[str], ["sh", "-c", "echo running smoke suite"]),
    security_cmd = field(list[str], ["sh", "-c", "echo running security scan"]),
    promote_cmd = field(list[str], ["sh", "-c", "echo promoting 100% prod traffic"]),
    rollback_cmd = field(list[str], ["sh", "-c", "echo reverting canary traffic"]),
    reason = field(str, ""),
)

S = state_space(
    "awaiting_approval",
    "preflight",
    "rolling_out",
    "rollout_hist",
    "deploying_canary",
    "bake_wait",
    "checking_canary",
    "verifying",
    "smoke_lane",
    "smoke_running",
    "smoke_done",
    "security_lane",
    "security_running",
    "security_done",
    "promoting",
    "paused",
    "rolling_back",
    "alerting_oncall",
    "live",
    "blocked",
    "rolled_back",
)

def record_approval(ctx: DeployCtx, event: dict) -> DeployCtx:
    who = str(event.get("by", "approver"))
    if who in ctx.approved_by:
        return ctx
    return update(
        ctx,
        approvals = ctx.approvals + 1,
        approved_by = ctx.approved_by + [who],
    )

def window_open(ctx: DeployCtx, event: dict) -> bool:
    res = event.get("output", {})
    body = res.get("body", {})
    return res.get("status") == 200 and type(body) == "dict" and body.get("freeze", False) == False

def canary_unhealthy(ctx: DeployCtx, event: dict) -> bool:
    res = event.get("output", {})
    body = res.get("body", {})
    if res.get("status") != 200 or type(body) != "dict":
        return True
    err_rate = float(body.get("error_rate", 1.0))
    return err_rate > ctx.max_error_rate

def bake_complete(ctx: DeployCtx, event: dict) -> bool:
    return (ctx.bake_checks + 1) >= ctx.required_bake_checks

def build_alert_request(ctx: DeployCtx) -> HttpRequest:
    return http_request(
        method = HttpMethod.POST,
        url = ctx.control_api + "/v1/incidents",
        json = {
            "service": ctx.service,
            "version": ctx.version,
            "reason": ctx.reason,
        },
        timeout_ms = 5000,
    )

chart: StateMachine = machine(
    id = "deployment_pipeline",
    description = "Progressive canary deployment with approval quorum, pause/resume history, and auto-rollback",
    initial = S.awaiting_approval,
    context = DeployCtx(),
    states = {
        S.awaiting_approval: state(
            on = [
                internal_transition(event = DeployEvent.APPROVE, assign = record_approval),
                transition(
                    event = DeployEvent.START_ROLLOUT,
                    target = S.preflight,
                    guard = lambda ctx, _ev: ctx.approvals >= ctx.required_approvals,
                ),
            ],
        ),
        S.preflight: state(
            invoke = invoke_http(
                id = "check_freeze",
                request = lambda ctx: http_request(url = ctx.control_api + "/v1/deploy-window", timeout_ms = 5000),
                on_done = [
                    outcome(target = S.rolling_out, guard = window_open),
                    outcome(
                        target = S.blocked,
                        assign = lambda ctx, _ev: update(ctx, reason = "change_freeze_active"),
                    ),
                ],
                on_error = outcome(
                    target = S.blocked,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "preflight_failed"))),
                ),
            ),
        ),
        S.rolling_out: compound_state(
            initial = S.deploying_canary,
            on = [
                transition(event = DeployEvent.PAUSE, target = S.paused),
                transition(
                    event = DeployEvent.ROLLBACK,
                    target = S.rolling_back,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("reason", "manual_rollback"))),
                ),
            ],
            states = {
                S.rollout_hist: shallow_history(target = S.deploying_canary),
                S.deploying_canary: state(
                    invoke = invoke_cmd(
                        id = "deploy_canary",
                        cmd = lambda ctx: ctx.canary_cmd,
                        on_done = S.bake_wait,
                        on_error = outcome(
                            target = S.rolling_back,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "canary_deploy_failed"))),
                        ),
                    ),
                ),
                S.bake_wait: state(
                    after = after(
                        delay = lambda ctx, _ev: ctx.bake_interval_ms,
                        target = S.checking_canary,
                    ),
                ),
                S.checking_canary: state(
                    invoke = invoke_http(
                        id = "canary_metrics",
                        request = lambda ctx: http_request(url = ctx.control_api + "/v1/canary-metrics", timeout_ms = 5000),
                        on_done = [
                            outcome(
                                target = S.rolling_back,
                                guard = canary_unhealthy,
                                assign = lambda ctx, _ev: update(ctx, reason = "canary_error_rate_exceeded"),
                            ),
                            outcome(
                                target = S.verifying,
                                guard = bake_complete,
                                assign = lambda ctx, _ev: update(ctx, bake_checks = ctx.bake_checks + 1),
                            ),
                            outcome(
                                target = S.bake_wait,
                                assign = lambda ctx, _ev: update(ctx, bake_checks = ctx.bake_checks + 1),
                            ),
                        ],
                        on_error = outcome(
                            target = S.rolling_back,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "metrics_unreachable"))),
                        ),
                    ),
                ),
                S.verifying: parallel_state(
                    on_done = S.promoting,
                    states = {
                        S.smoke_lane: compound_state(
                            initial = S.smoke_running,
                            states = {
                                S.smoke_running: state(
                                    invoke = invoke_cmd(
                                        id = "smoke_tests",
                                        cmd = lambda ctx: ctx.smoke_cmd,
                                        on_done = S.smoke_done,
                                        on_error = outcome(
                                            target = S.rolling_back,
                                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "smoke_failed"))),
                                        ),
                                    ),
                                ),
                                S.smoke_done: final_state(),
                            },
                        ),
                        S.security_lane: compound_state(
                            initial = S.security_running,
                            states = {
                                S.security_running: state(
                                    invoke = invoke_cmd(
                                        id = "security_scan",
                                        cmd = lambda ctx: ctx.security_cmd,
                                        on_done = S.security_done,
                                        on_error = outcome(
                                            target = S.rolling_back,
                                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "security_failed"))),
                                        ),
                                    ),
                                ),
                                S.security_done: final_state(),
                            },
                        ),
                    },
                ),
                S.promoting: state(
                    invoke = invoke_cmd(
                        id = "promote_prod",
                        cmd = lambda ctx: ctx.promote_cmd,
                        on_done = S.live,
                        on_error = outcome(
                            target = S.rolling_back,
                            assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "promote_failed"))),
                        ),
                    ),
                ),
            },
        ),
        S.paused: state(
            on = [
                transition(event = DeployEvent.RESUME, target = S.rollout_hist),
                transition(
                    event = DeployEvent.ROLLBACK,
                    target = S.rolling_back,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("reason", "rollback_while_paused"))),
                ),
            ],
        ),
        S.rolling_back: state(
            invoke = invoke_cmd(
                id = "revert_traffic",
                cmd = lambda ctx: ctx.rollback_cmd,
                on_done = S.alerting_oncall,
                on_error = S.alerting_oncall,
            ),
        ),
        S.alerting_oncall: state(
            invoke = invoke_http(
                id = "incident_webhook",
                request = build_alert_request,
                on_done = S.rolled_back,
                on_error = S.rolled_back,
            ),
        ),
        S.live: final_state(output = {"status": "live"}),
        S.blocked: final_state(output = {"status": "blocked"}),
        S.rolled_back: final_state(output = {"status": "rolled_back"}),
    },
)

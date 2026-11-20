# Showcase statechart using all enhanced Starchart DSL features:
# - `state_space(...)` for DRY typed StateId handles
# - `record(...)` for statically typed context fields (`ctx.approvals`, `ctx.canary_errors`)
# - `internal_transition(...)` for in-place context updates
# - `all_of(...)` / `not_(...)` / `named_guard(...)` combinators
# - `compound_state(...)` + `parallel_state(...)` hierarchy

DeployEvent = enum(
    "APPROVE",
    "START_ROLLOUT",
    "CANARY_METRIC_ERR",
    "CHECKS_PASSED",
    "ROLLBACK",
)
DeployAction = enum("DEPLOY_CANARY", "PROMOTE_PROD", "PAGE_ONCALL", "REVERT_TRAFFIC")

DeployCtx = record(
    approvals = int,
    required_approvals = int,
    canary_errors = int,
)

def has_quorum(ctx: DeployCtx, event: dict) -> bool:
    return ctx.approvals >= ctx.required_approvals

def is_canary_healthy(ctx: DeployCtx, event: dict) -> bool:
    return ctx.canary_errors == 0

def add_approval(ctx: DeployCtx, event: dict) -> DeployCtx:
    return DeployCtx(
        approvals = ctx.approvals + 1,
        required_approvals = ctx.required_approvals,
        canary_errors = ctx.canary_errors,
    )

def record_canary_error(ctx: DeployCtx, event: dict) -> DeployCtx:
    return DeployCtx(
        approvals = ctx.approvals,
        required_approvals = ctx.required_approvals,
        canary_errors = ctx.canary_errors + 1,
    )

S = state_space(
    "awaiting_approval",
    "rolling_out",
    "canary",
    "verifying",
    "smoke_tests",
    "security_scan",
    "live",
    "rolled_back",
)

chart: StateMachine = machine(
    id = "deployment_pipeline",
    initial = S.awaiting_approval,
    context = DeployCtx(approvals = 0, required_approvals = 2, canary_errors = 0),
    states = {
        S.awaiting_approval: state(
            on = [
                internal_transition(event = DeployEvent.APPROVE, assign = add_approval),
                transition(
                    event = DeployEvent.START_ROLLOUT,
                    target = S.rolling_out,
                    guard = all_of(has_quorum, not_(named_guard("CHANGE_FREEZE"))),
                ),
            ],
        ),
        S.rolling_out: compound_state(
            initial = S.canary,
            on = [
                transition(
                    event = DeployEvent.ROLLBACK,
                    target = S.rolled_back,
                    actions = [DeployAction.REVERT_TRAFFIC, DeployAction.PAGE_ONCALL],
                ),
            ],
            states = {
                S.canary: state(
                    entry = [DeployAction.DEPLOY_CANARY],
                    on = [
                        internal_transition(
                            event = DeployEvent.CANARY_METRIC_ERR,
                            assign = record_canary_error,
                        ),
                        transition(
                            event = DeployEvent.CHECKS_PASSED,
                            target = S.verifying,
                            guard = is_canary_healthy,
                        ),
                    ],
                ),
                S.verifying: parallel_state(
                    on = [
                        transition(
                            event = DeployEvent.CHECKS_PASSED,
                            target = S.live,
                            guard = is_canary_healthy,
                            actions = [DeployAction.PROMOTE_PROD],
                        ),
                    ],
                    states = {
                        S.smoke_tests: state(),
                        S.security_scan: state(),
                    },
                ),
            },
        ),
        S.live: final_state(),
        S.rolled_back: final_state(),
    },
)

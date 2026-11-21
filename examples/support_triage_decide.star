# Zero-compile customer support triage & auto-remediation statechart using `@starchart/decide`:
# - Evaluates structured `noul` (yes/no), `choice` (typed `SupportTeam` enum), and `score` (`urgency`)
#   questions in one provider-neutral `decision-rs` call (`jev` by default, or self-hosted via
#   `--context '{"decision_endpoint": "http://127.0.0.1:9091/v1/systemone"}'`).
# - Gates autonomous actions (security paging, instant refund, queue dispatch) on calibrated
#   probabilities (`is_yes`, `prob`, `confidence`, `refused`), falling back to `awaiting_human_triage`
#   whenever a question is refused or below `min_route_prob`.

load(
    "@starchart/decide",
    "DecisionProvider",
    "choice",
    "confidence",
    "invoke_decision",
    "is_yes",
    "jev",
    "level",
    "noul",
    "prob",
    "refused",
    "score",
    "score_value",
    "selected",
)
load("@starchart/exec", "invoke_cmd")

SupportTeam = enum("BILLING", "TECHNICAL", "SECURITY", "ACCOUNT")

HumanEvent = enum("ASSIGN_TEAM", "ESCALATE_SECURITY")

TriageCtx = record(
    ticket_id = field(str, "TKT-1042"),
    customer_tier = field(str, "enterprise"),
    subject = field(str, "Duplicate charge on November invoice"),
    body = field(str, "We were billed twice for invoice #8821 ($490). Please refund the duplicate charge."),
    decision_endpoint = field(str, ""),
    min_route_prob = field(float, 0.80),
    team = field(str, ""),
    refund_approved = field(bool, False),
    urgency = field(str, ""),
    urgency_score = field(float, 0.0),
    team_prob = field(float, 0.0),
    reason = field(str, ""),
    page_cmd = field(list[str], ["sh", "-c", "echo paging security on-call"]),
    refund_cmd = field(list[str], ["sh", "-c", "echo issuing automated billing refund"]),
    route_cmd = field(list[str], ["sh", "-c", "echo routing ticket to team queue"]),
)

S = state_space(
    "classifying",
    "paging_security",
    "issuing_refund",
    "routing_queue",
    "awaiting_human_triage",
    "resolved",
    "failed",
)

def select_provider(ctx: TriageCtx) -> DecisionProvider:
    if ctx.decision_endpoint != "":
        return jev(endpoint = ctx.decision_endpoint)
    return jev()

def record_triage(ctx: TriageCtx, ev: dict) -> TriageCtx:
    return update(
        ctx,
        team = selected(ev, "team") or "UNASSIGNED",
        refund_approved = is_yes(ev, "refund_requested"),
        urgency = level(ev, "urgency") or "routine",
        urgency_score = score_value(ev, "urgency") or 0.0,
        team_prob = prob(ev, "team"),
    )

def confident_team(ctx: TriageCtx, ev: dict) -> bool:
    if refused(ev, "team"):
        return False
    conf = confidence(ev, "team")
    conf_ok = True if conf == None else conf >= ctx.min_route_prob
    return prob(ev, "team") >= ctx.min_route_prob and conf_ok

def is_security_incident(ctx: TriageCtx, ev: dict) -> bool:
    return (
        confident_team(ctx, ev)
        and selected(ev, "team") == SupportTeam.SECURITY.value
        and (level(ev, "urgency") in ["urgent", "critical"])
    )

def is_auto_refundable(ctx: TriageCtx, ev: dict) -> bool:
    return (
        confident_team(ctx, ev)
        and selected(ev, "team") == SupportTeam.BILLING.value
        and is_yes(ev, "refund_requested")
    )

chart: StateMachine = machine(
    id = "support_triage_decide",
    description = "Confidence-gated support ticket triage and remediation powered by @starchart/decide",
    initial = S.classifying,
    context = TriageCtx(),
    states = {
        S.classifying: state(
            invoke = invoke_decision(
                id = "triage_ticket",
                provider = select_provider,
                evidence = lambda ctx: {
                    "ticket_id": ctx.ticket_id,
                    "customer_tier": ctx.customer_tier,
                    "subject": ctx.subject,
                    "body": ctx.body,
                },
                questions = {
                    "refund_requested": noul(
                        "Does the customer explicitly request a refund or credit?",
                        yes = "Customer explicitly asks for money back, a refund, or a billing reversal.",
                        no = "No refund or billing reversal is requested.",
                        threshold = 0.85,
                    ),
                    "team": choice(
                        "Which support team should own this ticket?",
                        options = {
                            SupportTeam.BILLING: "Invoices, duplicate charges, refunds, and payment methods.",
                            SupportTeam.TECHNICAL: "API errors, crashes, bugs, latency, and integration issues.",
                            SupportTeam.SECURITY: "Account takeover, leaked credentials, or security vulnerabilities.",
                            SupportTeam.ACCOUNT: "Seat changes, SSO setup, and general account administration.",
                        },
                        optional = True,
                    ),
                    "urgency": score(
                        "How urgently does this ticket require intervention?",
                        levels = [
                            ("routine", "General question or minor request; standard SLA."),
                            ("today", "Degraded workflow or billing discrepancy needing same-day review."),
                            ("urgent", "Production impact or significant financial issue."),
                            ("critical", "Active security breach or complete production outage."),
                        ],
                    ),
                },
                on_done = [
                    outcome(
                        target = S.paging_security,
                        guard = is_security_incident,
                        assign = record_triage,
                    ),
                    outcome(
                        target = S.issuing_refund,
                        guard = is_auto_refundable,
                        assign = record_triage,
                    ),
                    outcome(
                        target = S.routing_queue,
                        guard = confident_team,
                        assign = record_triage,
                    ),
                    outcome(
                        target = S.awaiting_human_triage,
                        assign = record_triage,
                    ),
                ],
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "decision_failed"))),
                ),
            ),
        ),
        S.awaiting_human_triage: state(
            on = [
                transition(
                    event = HumanEvent.ASSIGN_TEAM,
                    target = S.routing_queue,
                    assign = lambda ctx, ev: update(ctx, team = str(ev.get("team", "TECHNICAL"))),
                ),
                transition(
                    event = HumanEvent.ESCALATE_SECURITY,
                    target = S.paging_security,
                    assign = lambda ctx, _ev: update(ctx, team = SupportTeam.SECURITY.value, urgency = "critical"),
                ),
            ],
        ),
        S.paging_security: state(
            invoke = invoke_cmd(
                id = "page_secops",
                cmd = lambda ctx: ctx.page_cmd,
                on_done = S.resolved,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "page_failed"))),
                ),
            ),
        ),
        S.issuing_refund: state(
            invoke = invoke_cmd(
                id = "auto_refund",
                cmd = lambda ctx: ctx.refund_cmd,
                on_done = S.resolved,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "refund_failed"))),
                ),
            ),
        ),
        S.routing_queue: state(
            invoke = invoke_cmd(
                id = "dispatch_queue",
                cmd = lambda ctx: ctx.route_cmd,
                on_done = S.resolved,
                on_error = outcome(
                    target = S.failed,
                    assign = lambda ctx, ev: update(ctx, reason = str(ev.get("error", "route_failed"))),
                ),
            ),
        ),
        S.resolved: final_state(output = {"status": "resolved"}),
        S.failed: final_state(output = {"status": "failed"}),
    },
)

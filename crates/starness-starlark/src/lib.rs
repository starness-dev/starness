//! Starlark DSL, static typechecker, and multi-file loader for `starness`.

pub mod dsl;
pub mod loader;
pub mod schema;
pub mod stdlib;
pub mod types;

pub use dsl::{chart_dialect, chart_globals, register_dsl};
pub use loader::{
    ChartCompileError, CompiledAgentTool, CompiledChart, FsResolver, InMemoryResolver,
    SourceResolver, compile_starlark_file, compile_starlark_str, compile_with_resolver,
};
pub use schema::{
    RecordFieldShape, TypeRegistry, TypeShape, alloc_plain_json, instantiate_record_with_registry,
    parse_type_expr, with_active_type_registry,
};
pub use stdlib::{
    ACP_MODULE_ID, BUILTIN_ACP_SERVICE, BUILTIN_DECIDE_SERVICE, BUILTIN_EXEC_SERVICE,
    BUILTIN_HTTP_SERVICE, BUILTIN_LLM_SERVICE, BUILTIN_MCP_SERVICE, DECIDE_MODULE_ID,
    EXEC_MODULE_ID, HTTP_MODULE_ID, LLM_MODULE_ID, MCP_MODULE_ID, SECRETS_MODULE_ID,
    is_builtin_service, resolve_builtin_module,
};
pub use types::{
    ArtifactMode, FrozenStarlarkMachine, INVOKE_INPUT_SLOT_KEY, INVOKE_TOOL_SLOT_KEY,
    StarlarkAction, StarlarkAfter, StarlarkAgentTool, StarlarkGuard, StarlarkInvoke,
    StarlarkMachine, StarlarkOutcome, StarlarkSecret, StarlarkStateId, StarlarkStateNode,
    StarlarkStateSpace, StarlarkTransition, SymbolOrEnum, starlark_input_slot_name,
    starlark_tool_slot_name,
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use starness_ir::GuardRef;

    use super::*;

    fn workspace_example(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    #[test]
    fn order_workflow_example_typechecks_evaluates_and_freezes() {
        let path = workspace_example("tests/fixtures/order_workflow.star");
        let compiled = compile_starlark_file(&path, None).expect("order_workflow.star should compile");

        assert_eq!(compiled.ir.id, "order_workflow");
        assert_eq!(compiled.ir.initial.as_str(), "validating");
        assert_eq!(compiled.guards.len(), 1);
        assert_eq!(compiled.reducers.len(), 1);
        assert_eq!(compiled.ir.validate(), Ok(()));
    }

    #[test]
    fn passing_raw_string_target_is_rejected_by_static_typechecker_before_eval() {
        let src = r#"
# If evaluation ran, `fail(...)` would execute; static typecheck must reject first!
fail("evaluation must not run")

idle = state_id("idle")
chart = machine(
    id = "bad_target",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(event = "GO", target = "charging"),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("bad_target.star", src, None)
            .expect_err("raw str target must fail static typecheck");
        assert!(
            matches!(err, ChartCompileError::Typecheck(_)),
            "expected static typecheck error, got: {err}"
        );
    }

    #[test]
    fn unknown_named_argument_fails_immediately_in_static_typecheck() {
        let src = r#"
idle = state_id("idle")
chart = machine(
    id = "bad_arg",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(evnt = "FOO", target = idle),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("bad_arg.star", src, None)
            .expect_err("unknown named argument must fail");
        assert!(
            matches!(err, ChartCompileError::Typecheck(_)),
            "expected static typecheck failure, got: {err}"
        );
    }

    #[test]
    fn dangling_state_id_fails_inside_machine_with_span_info() {
        let src = r#"
idle = state_id("idle")
ghost = state_id("ghost")

chart = machine(
    id = "dangling",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(event = "JUMP", target = ghost),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("dangling.star", src, None)
            .expect_err("dangling StateId must fail inside machine(...)");
        let msg = err.to_string();
        assert!(msg.contains("dangling.star:5:"), "expected line/col span in error:\n{msg}");
        assert!(msg.contains("ghost"), "expected undeclared target name in error:\n{msg}");
    }

    #[test]
    fn multi_file_load_resolves_and_typechecks_across_modules() {
        let mut files = HashMap::new();
        files.insert(
            "events.star".to_owned(),
            r#"
OrderEvent = enum("START", "FINISH")
OrderCtx = record(retries = int)
"#
            .to_owned(),
        );

        let main_src = r#"
load("events.star", "OrderEvent", "OrderCtx")

S = state_space("idle", "done")

def bump(ctx: OrderCtx, event: dict) -> OrderCtx:
    return OrderCtx(retries = ctx.retries + 1)

chart = machine(
    id = "multi_file",
    initial = S.idle,
    context = OrderCtx(retries = 0),
    states = {
        S.idle: state(
            on = [
                internal_transition(event = OrderEvent.START, assign = bump),
                transition(event = OrderEvent.FINISH, target = S.done),
            ],
        ),
        S.done: final_state(),
    },
)
"#;
        let resolver = InMemoryResolver::new(files);
        let compiled = compile_with_resolver("main.star", main_src, "chart", &resolver)
            .expect("multi-file load with enum, record, state_space, and internal_transition should compile");
        assert_eq!(compiled.ir.id, "multi_file");
        assert!(
            compiled.context_record_type.is_some(),
            "expected OrderCtx RecordType to be captured from loaded module"
        );
    }

    #[test]
    fn guard_combinators_and_named_guards_compile_to_ir() {
        let src = r#"
S = state_space("locked", "unlocked")

def has_pin(ctx: dict, event: dict) -> bool:
    return event.get("pin") == ctx.get("pin")

chart = machine(
    id = "smart_lock",
    initial = S.locked,
    context = {"pin": 1234},
    states = {
        S.locked: state(
            on = [
                transition(
                    event = "UNLOCK",
                    target = S.unlocked,
                    guard = all_of(
                        not_(named_guard("IS_TAMPERED")),
                        any_of(has_pin, "HAS_MASTER_KEY"),
                    ),
                ),
            ],
        ),
        S.unlocked: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("smart_lock.star", src, None)
            .expect("guard combinators should compile");
        let guard = compiled.ir.states[&starness_ir::StateId::from("locked")].on[0]
            .guard
            .as_ref()
            .expect("expected guard");
        assert_eq!(
            guard,
            &GuardRef::AllOf(vec![
                GuardRef::Not(Box::new(GuardRef::Named("IS_TAMPERED".into()))),
                GuardRef::AnyOf(vec![
                    GuardRef::StarlarkSlot(0),
                    GuardRef::Named("HAS_MASTER_KEY".into()),
                ]),
            ])
        );
    }

    #[test]
    fn builtin_virtual_modules_http_exec_acp_resolve_and_typecheck() {
        let src = r#"
load("@starness/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")
load("@starness/exec", "invoke_cmd")
load("@starness/acp", "acp_agent", "invoke_agent")

Ctx = record(api_url = str, task = str)
S = state_space("fetching", "coding", "testing", "done", "failed")

coder = acp_agent(id = "coder", command = "agy", args = ["acp"], model = "gemini-2.5-pro")

def make_req(ctx: Ctx) -> HttpRequest:
    return http_request(method = HttpMethod.POST, url = ctx.api_url + "/items", json = {"ok": True})

def make_prompt(ctx: Ctx) -> str:
    return "Implement: " + ctx.task

chart = machine(
    id = "builtin_mods",
    initial = S.fetching,
    context = Ctx(api_url = "http://localhost:8080", task = "healthcheck"),
    states = {
        S.fetching: state(
            invoke = invoke_http(request = make_req, on_done = S.coding, on_error = S.failed),
        ),
        S.coding: state(
            invoke = invoke_agent(agent = coder, prompt = make_prompt, on_done = S.testing, on_error = S.failed),
        ),
        S.testing: state(
            invoke = invoke_cmd(cmd = ["echo", "ok"], on_done = S.done, on_error = S.failed),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("builtin_mods.star", src, None)
            .expect("built-in @starness/* modules should typecheck and compile");
        assert_eq!(compiled.inputs.len(), 2);
        assert_eq!(
            compiled.ir.states[&starness_ir::StateId::from("fetching")].invoke[0].src,
            BUILTIN_HTTP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starness_ir::StateId::from("coding")].invoke[0].src,
            BUILTIN_ACP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starness_ir::StateId::from("testing")].invoke[0].src,
            BUILTIN_EXEC_SERVICE
        );
    }

    #[test]
    fn builtin_http_module_rejects_invalid_parameter_types_statically() {
        let src = r#"
load("@starness/http", "http_request")
fail("must fail static typecheck before evaluation")
req = http_request(url = 12345)
"#;
        let err = compile_starlark_str("bad_http.star", src, None)
            .expect_err("non-string url must fail static typecheck");
        assert!(
            matches!(err, ChartCompileError::Typecheck(_)),
            "expected Typecheck error, got: {err}"
        );
    }

    #[test]
    fn agent_mcp_tools_typecheck_and_compile_schemas() {
        let src = r#"
load("@starness/acp", "ArtifactMode", "acp_agent", "artifact_tool", "invoke_agent", "read_tool")

Ctx = record(task = str, test_errors = str)
Plan = record(
    summary = str,
    steps = list[str],
    notes = field(str, ""),
)
Finding = record(severity = str, message = str)
QueryArgs = record(section = str)

planner = acp_agent(id = "planner", command = "agy", args = ["acp"])

def check_plan(ctx: Ctx, plan: Plan) -> str | None:
    if len(plan.steps) == 0:
        return "steps must not be empty"
    return None

def read_errors(ctx: Ctx, args: QueryArgs) -> dict:
    return {"section": args.section, "errors": ctx.test_errors}

S = state_space("planning", "done", "failed")

chart = machine(
    id = "mcp_tools_check",
    initial = S.planning,
    context = Ctx(task = "auth", test_errors = "none"),
    states = {
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = "Plan the task",
                tools = [
                    read_tool(
                        "get_errors",
                        description = "Read test errors",
                        schema = QueryArgs,
                        handler = read_errors,
                    ),
                    artifact_tool(
                        "submit_plan",
                        description = "Submit implementation plan",
                        schema = Plan,
                        mode = ArtifactMode.replace,
                        required = True,
                        validate = check_plan,
                    ),
                    artifact_tool(
                        "report_finding",
                        description = "Append a finding",
                        schema = Finding,
                        mode = ArtifactMode.append,
                        required = False,
                    ),
                ],
                on_done = S.done,
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("mcp_tools_check.star", src, None)
            .expect("agent MCP tools should typecheck and compile");
        assert_eq!(compiled.tools.len(), 3);
        let submit = compiled
            .tools
            .values()
            .find(|t| t.name() == "submit_plan")
            .expect("submit_plan tool");
        assert_eq!(
            submit.input_schema()["required"],
            serde_json::json!(["summary", "steps"])
        );
    }

    #[test]
    fn duplicate_mcp_tool_name_in_invoke_agent_fails_compile() {
        let src = r#"
load("@starness/acp", "acp_agent", "artifact_tool", "invoke_agent")

Plan = record(summary = str)
planner = acp_agent(id = "planner", command = "agy", args = ["acp"])
S = state_space("planning", "done")

chart = machine(
    id = "dup_tools",
    initial = S.planning,
    states = {
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = "Go",
                tools = [
                    artifact_tool("submit_plan", schema = Plan),
                    artifact_tool("submit_plan", schema = Plan),
                ],
                on_done = S.done,
            ),
        ),
        S.done: final_state(),
    },
)
"#;
        let err = compile_starlark_str("dup_tools.star", src, None)
            .expect_err("duplicate tool names must fail compile");
        assert!(
            err.to_string().contains("duplicate MCP tool name `submit_plan`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn builtin_mcp_client_module_typechecks_and_compiles_stdio_and_http_servers() {
        let src = r#"
load("@starness/mcp", "McpServer", "invoke_mcp", "mcp_http_server", "mcp_stdio_server")

Ctx = record(repo = str, pr_url = field(str, ""))
gh = mcp_stdio_server(id = "github", command = "npx", args = ["-y", "@modelcontextprotocol/server-github"])
linear = mcp_http_server(id = "linear", url = "http://127.0.0.1:9000/mcp")

S = state_space("opening_pr", "updating_ticket", "done", "failed")

chart = machine(
    id = "mcp_client_check",
    initial = S.opening_pr,
    context = Ctx(repo = "floodfx/starness"),
    states = {
        S.opening_pr: state(
            invoke = invoke_mcp(
                server = gh,
                tool = "create_pull_request",
                arguments = lambda ctx: {"repo": ctx.repo, "title": "Release"},
                on_done = outcome(
                    target = S.updating_ticket,
                    assign = lambda ctx, ev: update(ctx, pr_url = str(ev["output"]["data"].get("url", ""))),
                ),
                on_error = S.failed,
            ),
        ),
        S.updating_ticket: state(
            invoke = invoke_mcp(
                server = linear,
                tool = "add_comment",
                arguments = lambda ctx: {"body": ctx.pr_url},
                on_done = S.done,
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("mcp_client_check.star", src, None)
            .expect("@starness/mcp should typecheck and compile");
        assert_eq!(compiled.inputs.len(), 2);
        assert_eq!(
            compiled.ir.states[&starness_ir::StateId::from("opening_pr")].invoke[0].src,
            BUILTIN_MCP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starness_ir::StateId::from("opening_pr")].invoke[0].id.as_deref(),
            Some("github_create_pull_request")
        );
    }

    #[test]
    fn secrets_module_compiles_without_env_and_rejects_secrets_in_context() {
        let valid_src = r#"
load("@starness/secrets", "SecretSource", "secret")
load("@starness/http", "http_request", "invoke_http")
load("@starness/exec", "invoke_cmd")
load("@starness/mcp", "invoke_mcp", "mcp_http_server")

S = state_space("calling_http", "calling_exec", "calling_mcp", "done")

mcp_srv = mcp_http_server(
    id = "remote_mcp",
    url = "https://mcp.example.com",
    headers = {"Authorization": secret("STARNESS_UNSET_MCP_TOKEN", prefix = "Bearer ")},
)

chart = machine(
    id = "secrets_compile_ok",
    initial = S.calling_http,
    states = {
        S.calling_http: state(
            invoke = invoke_http(
                request = http_request(
                    url = "https://api.example.com/v1",
                    headers = {
                        "Authorization": secret(
                            "STARNESS_UNSET_KEYCHAIN_TOKEN",
                            source = SecretSource.keychain,
                            prefix = "Bearer ",
                        ),
                    },
                ),
                on_done = S.calling_exec,
            ),
        ),
        S.calling_exec: state(
            invoke = invoke_cmd(
                cmd = ["echo", "ok"],
                env = {"API_KEY": secret("STARNESS_UNSET_ENV_KEY")},
                on_done = S.calling_mcp,
            ),
        ),
        S.calling_mcp: state(
            invoke = invoke_mcp(
                server = mcp_srv,
                tool = "ping",
                on_done = S.done,
            ),
        ),
        S.done: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("secrets_compile_ok.star", valid_src, None)
            .expect("@starness/secrets should compile without resolving env/keychain");
        let http_input = &compiled.ir.states[&starness_ir::StateId::from("calling_http")].invoke[0].input;
        assert_eq!(
            http_input["headers"]["Authorization"]["$starness_secret"]["source"],
            "keychain"
        );

        let leaked_ctx_src = r#"
load("@starness/secrets", "secret")

S = state_space("idle")

chart = machine(
    id = "bad_secret_ctx",
    initial = S.idle,
    context = {"api_key": secret("STRIPE_SECRET")},
    states = {
        S.idle: state(),
    },
)
"#;
        let err = compile_starlark_str("bad_secret_ctx.star", leaked_ctx_src, None)
            .expect_err("secret(...) inside `context` must fail validation");
        let msg = format!("{err}");
        assert!(
            msg.contains("context.api_key") && msg.contains("cannot be stored in machine `context`"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn decide_module_typechecks_noul_choice_enum_score_providers_and_helpers() {
        let src = r#"
load(
    "@starness/decide",
    "answer",
    "choice",
    "cloudflare_decision",
    "confidence",
    "gemini_decision",
    "invoke_decision",
    "is_yes",
    "jev",
    "level",
    "noul",
    "openai_decision",
    "openrouter_decision",
    "prob",
    "refused",
    "score",
    "score_value",
    "selected",
)

Team = enum("BILLING", "TECHNICAL", "SECURITY")

Ctx = record(
    ticket = field(str, "Double charged on invoice #42"),
    team = field(str, ""),
    urgency = field(str, ""),
    urgency_raw = field(float, 0.0),
)

_gemini = gemini_decision()
_openai = openai_decision()
_openrouter = openrouter_decision()
_cf = cloudflare_decision()

def high_confidence_refund(ctx: Ctx, ev: dict) -> bool:
    conf = confidence(ev, "team")
    return (
        not refused(ev, "refund")
        and is_yes(ev, "refund")
        and prob(ev, "team") >= 0.8
        and (conf == None or conf >= 0.7)
    )

def save_decision(ctx: Ctx, ev: dict) -> Ctx:
    _raw = answer(ev, "urgency")
    return update(
        ctx,
        team = selected(ev, "team") or "GENERAL",
        urgency = level(ev, "urgency") or "routine",
        urgency_raw = score_value(ev, "urgency") or 0.0,
    )

S = state_space("triaging", "auto_refund", "human_review", "failed")

chart = machine(
    id = "decide_check",
    initial = S.triaging,
    context = Ctx(),
    states = {
        S.triaging: state(
            invoke = invoke_decision(
                provider = jev(),
                evidence = lambda ctx: {"ticket": ctx.ticket},
                questions = {
                    "refund": noul(
                        "Does the customer request a refund?",
                        yes = "Customer explicitly asks for money back.",
                        no = "No refund requested.",
                        threshold = 0.85,
                    ),
                    "team": choice(
                        "Which team owns this ticket?",
                        options = {
                            Team.BILLING: "Invoices, charges, and refunds.",
                            Team.TECHNICAL: "Product bugs and outages.",
                            Team.SECURITY: "Account compromise or vulnerabilities.",
                        },
                    ),
                    "urgency": score(
                        "How urgent is this issue?",
                        levels = ["routine", "today", "urgent", "critical"],
                    ),
                },
                on_done = [
                    outcome(
                        target = S.auto_refund,
                        guard = high_confidence_refund,
                        assign = save_decision,
                    ),
                    outcome(target = S.human_review, assign = save_decision),
                ],
                on_error = S.failed,
            ),
        ),
        S.auto_refund: final_state(),
        S.human_review: final_state(),
        S.failed: final_state(),
    },
)
"#;

        let compiled = compile_starlark_str("decide_check.star", src, None)
            .expect("@starness/decide should statically typecheck and compile");
        let inv = &compiled.ir.states[&starness_ir::StateId::from("triaging")].invoke[0];
        assert_eq!(inv.src, BUILTIN_DECIDE_SERVICE);
        assert_eq!(inv.id.as_deref(), Some("typesafe_decision"));
        assert_eq!(
            inv.input["provider"]["api_key"]["$starness_secret"]["name"],
            "TYPESAFE_API_KEY"
        );
        assert_eq!(inv.input["questions"]["refund"]["kind"], "noul");
        assert_eq!(
            inv.input["questions"]["team"]["options"][0][0],
            "BILLING"
        );
        assert_eq!(
            inv.input["questions"]["urgency"]["levels"][3][0],
            "critical"
        );
    }

    #[test]
    fn llm_module_typechecks_record_schema_derivation_and_rehydration() {
        let src = r#"
load(
    "@starness/llm",
    "LlmModel",
    "ReasoningEffort",
    "invoke_llm",
    "llm_model",
    "llm_structured",
    "llm_text",
)
load("@starness/secrets", "secret")

RiskLevel = enum("LOW", "MEDIUM", "HIGH")
ChangeItem = record(
    component = str,
    summary = str,
    risk = RiskLevel,
)
ReleaseSummary = record(
    headline = str,
    breaking = bool,
    changes = list[ChangeItem],
    notes = field(str, ""),
)

Ctx = record(
    diff = field(str, "feat: add @starness/llm"),
    headline = field(str, ""),
    high_risk_count = field(int, 0),
    plain_text = field(str, ""),
)

writer: LlmModel = llm_model(
    "openai::gpt-4o-mini",
    api_key = secret("OPENAI_API_KEY", optional = True),
    temperature = 0.2,
    max_tokens = 1024,
    top_p = 0.95,
    reasoning_effort = ReasoningEffort.low,
)

def store_structured(ctx: Ctx, ev: dict) -> Ctx:
    parsed = llm_structured(ev, ReleaseSummary)
    risky = [c for c in parsed.changes if c.risk == RiskLevel.HIGH]
    return update(
        ctx,
        headline = parsed.headline,
        high_risk_count = len(risky),
        plain_text = llm_text(ev),
    )

S = state_space("drafting", "summarizing", "done", "failed")

chart = machine(
    id = "llm_compile_check",
    initial = S.drafting,
    context = Ctx(),
    states = {
        S.drafting: state(
            invoke = invoke_llm(
                model = writer,
                system = "Extract structured release notes.",
                prompt = lambda ctx: "Summarize:\n" + ctx.diff,
                context = lambda ctx: {"diff": ctx.diff},
                schema = ReleaseSummary,
                stream = True,
                timeout_ms = 30000,
                on_done = outcome(target = S.summarizing, assign = store_structured),
                on_error = S.failed,
            ),
        ),
        S.summarizing: state(
            invoke = invoke_llm(
                model = "gemini-2.5-flash",
                prompt = lambda ctx: "Write a tweet for: " + ctx.headline,
                schema = {"title": "TweetBox", "type": "object", "properties": {"tweet": {"type": "string"}}},
                stream = False,
                on_done = S.done,
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    },
)
"#;

        let compiled = compile_starlark_str("llm_compile_check.star", src, None)
            .expect("@starness/llm should statically typecheck and compile");
        let inv = &compiled.ir.states[&starness_ir::StateId::from("drafting")].invoke[0];
        assert_eq!(inv.src, BUILTIN_LLM_SERVICE);
        assert_eq!(inv.input["model"]["model"], "openai::gpt-4o-mini");
        assert_eq!(inv.input["model"]["reasoning_effort"], "low");
        assert_eq!(
            inv.input["model"]["api_key"]["$starness_secret"]["name"],
            "OPENAI_API_KEY"
        );
        assert_eq!(inv.input["schema"]["name"], "ReleaseSummary");
        assert_eq!(
            inv.input["schema"]["schema"]["properties"]["changes"]["items"]["properties"]["risk"]["enum"],
            serde_json::json!(["LOW", "MEDIUM", "HIGH"])
        );
        assert_eq!(
            inv.input["schema"]["schema"]["additionalProperties"],
            serde_json::json!(false)
        );

        let inv2 = &compiled.ir.states[&starness_ir::StateId::from("summarizing")].invoke[0];
        assert_eq!(inv2.input["model"]["model"], "gemini-2.5-flash");
        assert_eq!(inv2.input["schema"]["name"], "TweetBox");
        assert_eq!(inv2.input["stream"], false);
    }
}

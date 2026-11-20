//! Starlark DSL, static typechecker, and multi-file loader for `starchart`.

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
    ACP_MODULE_ID, BUILTIN_ACP_SERVICE, BUILTIN_EXEC_SERVICE, BUILTIN_HTTP_SERVICE,
    BUILTIN_MCP_SERVICE, EXEC_MODULE_ID, HTTP_MODULE_ID, MCP_MODULE_ID, is_builtin_service,
    resolve_builtin_module,
};
pub use types::{
    ArtifactMode, FrozenStarlarkMachine, INVOKE_INPUT_SLOT_KEY, INVOKE_TOOL_SLOT_KEY,
    StarlarkAction, StarlarkAfter, StarlarkAgentTool, StarlarkGuard, StarlarkInvoke,
    StarlarkMachine, StarlarkOutcome, StarlarkStateId, StarlarkStateNode, StarlarkStateSpace,
    StarlarkTransition, SymbolOrEnum, starlark_input_slot_name, starlark_tool_slot_name,
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use starchart_ir::GuardRef;

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
        let guard = compiled.ir.states[&starchart_ir::StateId::from("locked")].on[0]
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
load("@starchart/http", "HttpMethod", "HttpRequest", "http_request", "invoke_http")
load("@starchart/exec", "invoke_cmd")
load("@starchart/acp", "acp_agent", "invoke_agent")

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
            .expect("built-in @starchart/* modules should typecheck and compile");
        assert_eq!(compiled.inputs.len(), 2);
        assert_eq!(
            compiled.ir.states[&starchart_ir::StateId::from("fetching")].invoke[0].src,
            BUILTIN_HTTP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starchart_ir::StateId::from("coding")].invoke[0].src,
            BUILTIN_ACP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starchart_ir::StateId::from("testing")].invoke[0].src,
            BUILTIN_EXEC_SERVICE
        );
    }

    #[test]
    fn builtin_http_module_rejects_invalid_parameter_types_statically() {
        let src = r#"
load("@starchart/http", "http_request")
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
load("@starchart/acp", "ArtifactMode", "acp_agent", "artifact_tool", "invoke_agent", "read_tool")

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
load("@starchart/acp", "acp_agent", "artifact_tool", "invoke_agent")

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
load("@starchart/mcp", "McpServer", "invoke_mcp", "mcp_http_server", "mcp_stdio_server")

Ctx = record(repo = str, pr_url = field(str, ""))
gh = mcp_stdio_server(id = "github", command = "npx", args = ["-y", "@modelcontextprotocol/server-github"])
linear = mcp_http_server(id = "linear", url = "http://127.0.0.1:9000/mcp")

S = state_space("opening_pr", "updating_ticket", "done", "failed")

chart = machine(
    id = "mcp_client_check",
    initial = S.opening_pr,
    context = Ctx(repo = "floodfx/starchart"),
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
            .expect("@starchart/mcp should typecheck and compile");
        assert_eq!(compiled.inputs.len(), 2);
        assert_eq!(
            compiled.ir.states[&starchart_ir::StateId::from("opening_pr")].invoke[0].src,
            BUILTIN_MCP_SERVICE
        );
        assert_eq!(
            compiled.ir.states[&starchart_ir::StateId::from("opening_pr")].invoke[0].id.as_deref(),
            Some("github_create_pull_request")
        );
    }
}

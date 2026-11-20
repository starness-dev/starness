use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_starchart")
}

fn workspace_file(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

#[test]
fn check_order_workflow_exits_zero() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(workspace_file("tests/fixtures/order_workflow.star"))
        .output()
        .expect("run starchart check");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"));
    assert!(stdout.contains("order_workflow"));
}

#[test]
fn check_broken_star_file_exits_nonzero_with_diagnostics() {
    let tmp_path = std::env::temp_dir().join("starchart_broken_test.star");
    std::fs::write(
        &tmp_path,
        r#"
idle = state_id("idle")
chart = machine(
    id = "broken",
    initial = idle,
    states = {
        idle: state(
            on = [transition(event = "GO", target = "raw_string_not_allowed")],
        ),
    },
)
"#,
    )
    .expect("write broken star file");

    let output = Command::new(bin_path())
        .arg("check")
        .arg(&tmp_path)
        .output()
        .expect("run starchart check on broken file");

    let _ = std::fs::remove_file(&tmp_path);

    assert!(!output.status.success(), "broken file must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("static typecheck failed"),
        "expected diagnostic on stderr, got:\n{stderr}"
    );
}

#[test]
fn diagram_outputs_mermaid_state_diagram_v2() {
    let output = Command::new(bin_path())
        .arg("diagram")
        .arg(workspace_file("tests/fixtures/order_workflow.star"))
        .output()
        .expect("run starchart diagram");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("stateDiagram-v2"),
        "expected Mermaid stateDiagram-v2 header, got:\n{stdout}"
    );
    assert!(stdout.contains("validating"));
    assert!(stdout.contains("charging"));
}

#[test]
fn run_executes_order_workflow_events_end_to_end() {
    let output = Command::new(bin_path())
        .arg("run")
        .arg(workspace_file("tests/fixtures/order_workflow.star"))
        .arg("--event")
        .arg(r#"{"type":"APPROVED","amount":250}"#)
        .arg("--event")
        .arg("CHARGE_OK")
        .output()
        .expect("run starchart run");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("action `CHARGE_CARD` invoked"));
    assert!(stdout.contains("action `NOTIFY_USER` invoked"));
    assert!(stdout.contains("done=true"));
}

#[test]
fn check_builtin_http_and_acp_examples_exits_zero() {
    let output = Command::new(bin_path())
        .arg("check")
        .arg(workspace_file("examples/checkout_http.star"))
        .arg(workspace_file("examples/ci_autofix_acp.star"))
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg(workspace_file("examples/multi_agent_acp.star"))
        .output()
        .expect("run starchart check on zero-compile examples");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("checkout_http"));
    assert!(stdout.contains("multi_agent_acp"));
}

#[test]
fn parent_workflow_invokes_child_starchart_over_stdio_acp_protocol() {
    let tmp_dir = std::env::temp_dir();
    let child_path = tmp_dir.join("starchart_child_acp_agent.star");
    let parent_path = tmp_dir.join("starchart_parent_acp_caller.star");

    // Child statechart served via `starchart acp <child_path>`:
    // Receives prompt in `ctx.task`, runs a shell transform via `@starchart/exec`, and finishes!
    std::fs::write(
        &child_path,
        r#"
load("@starchart/exec", "ExecSpec", "exec_spec", "invoke_cmd")

ChildCtx = record(task = str, result = str)
S = state_space("working", "done")

def build_cmd(ctx: ChildCtx) -> ExecSpec:
    return exec_spec(cmd = ["sh", "-c", "printf 'PLAN_FOR:'\"$TASK\"", ], env = {"TASK": ctx.task})

def save_res(ctx: ChildCtx, event: dict) -> ChildCtx:
    return ChildCtx(task = ctx.task, result = event["output"]["stdout"])

chart = machine(
    id = "child_planner_agent",
    initial = S.working,
    context = ChildCtx(task = "", result = ""),
    states = {
        S.working: state(
            invoke = invoke_cmd(
                cmd = build_cmd,
                on_done = outcome(target = S.done, assign = save_res),
            ),
        ),
        S.done: final_state(),
    },
)
"#,
    )
    .expect("write child acp star file");

    // Parent statechart executed via `starchart run <parent_path>`:
    // Uses `@starchart/acp` to spawn `starchart acp <child_path>` over stdio JSON-RPC!
    let parent_src = format!(
        r#"
load("@starchart/acp", "acp_agent", "invoke_agent")

ParentCtx = record(goal = str, agent_reply = str)
S = state_space("delegating", "completed", "failed")

sub_agent = acp_agent(
    id = "sub_planner",
    command = {bin:?},
    args = ["acp", {child:?}],
)

def make_prompt(ctx: ParentCtx) -> str:
    return ctx.goal

def capture_reply(ctx: ParentCtx, event: dict) -> ParentCtx:
    return ParentCtx(goal = ctx.goal, agent_reply = event["output"]["text"])

chart = machine(
    id = "parent_orchestrator",
    initial = S.delegating,
    context = ParentCtx(goal = "oauth2-flow", agent_reply = ""),
    states = {{
        S.delegating: state(
            invoke = invoke_agent(
                agent = sub_agent,
                prompt = make_prompt,
                on_done = outcome(target = S.completed, assign = capture_reply),
                on_error = S.failed,
            ),
        ),
        S.completed: final_state(output = {{"ok": True}}),
        S.failed: final_state(output = {{"ok": False}}),
    }},
)
"#,
        bin = bin_path(),
        child = child_path.to_string_lossy()
    );
    std::fs::write(&parent_path, parent_src).expect("write parent acp star file");

    let output = Command::new(bin_path())
        .arg("run")
        .arg(&parent_path)
        .output()
        .expect("run parent starchart invoking child acp agent");

    let _ = std::fs::remove_file(&child_path);
    let _ = std::fs::remove_file(&parent_path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "parent workflow failed!\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("PLAN_FOR:oauth2-flow"),
        "expected child ACP agent output in parent context, got:\n{stdout}"
    );
    assert!(stdout.contains(r#"active=["completed"]"#));
}


fn parse_jsonl(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSONL line {l:?}: {e}")))
        .collect()
}

#[test]
fn run_trace_to_stdout_emits_only_jsonl_and_moves_human_output_to_stderr() {
    let output = Command::new(bin_path())
        .arg("run")
        .arg(workspace_file("tests/fixtures/order_workflow.star"))
        .arg("--trace")
        .arg("-")
        .arg("--event")
        .arg(r#"{"type":"APPROVED","amount":250}"#)
        .arg("--event")
        .arg("CHARGE_OK")
        .output()
        .expect("run starchart run --trace -");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr:\n{stderr}");

    let records = parse_jsonl(&stdout);
    let kinds: Vec<&str> = records.iter().map(|r| r["kind"].as_str().unwrap_or("")).collect();
    assert_eq!(kinds, vec!["started", "transition", "transition"], "{stdout}");
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r["seq"], serde_json::json!(i + 1));
        assert_eq!(r["machine"], serde_json::json!("order_workflow"));
    }
    assert_eq!(records[2]["to"], serde_json::json!(["fulfilled"]));

    // Human-readable progress is still available, on stderr.
    assert!(stderr.contains("[init] machine=`order_workflow`"), "{stderr}");
    assert!(stderr.contains("action `NOTIFY_USER` invoked"), "{stderr}");
}

#[test]
fn run_trace_to_file_captures_service_progress_from_async_builtins() {
    let dir = std::env::temp_dir().join(format!(
        "starchart_trace_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let chart_path = dir.join("build.star");
    let trace_path = dir.join("trace.jsonl");
    std::fs::write(
        &chart_path,
        r#"
load("@starchart/exec", "invoke_cmd")

S = state_space("building", "ok", "failed")

chart = machine(
    id = "build",
    initial = S.building,
    states = {
        S.building: state(
            invoke = invoke_cmd(
                id = "compile",
                cmd = ["sh", "-c", "echo compiling; echo warning: unused >&2; echo finished"],
                on_done = S.ok,
                on_error = S.failed,
            ),
        ),
        S.ok: final_state(),
        S.failed: final_state(),
    },
)
"#,
    )
    .expect("write chart");

    let output = Command::new(bin_path())
        .arg("run")
        .arg(&chart_path)
        .arg("--trace")
        .arg(&trace_path)
        .output()
        .expect("run starchart run --trace <file>");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("[done] machine=`build`"), "human output stays on stdout: {stdout}");

    let trace = std::fs::read_to_string(&trace_path).expect("trace file");
    let _ = std::fs::remove_dir_all(&dir);
    let records = parse_jsonl(&trace);

    let progress: Vec<(String, String)> = records
        .iter()
        .filter(|r| r["kind"] == "service_progress")
        .map(|r| {
            assert_eq!(r["invoke_id"], "compile");
            assert_eq!(r["state"], "building");
            (
                r["stream"].as_str().unwrap_or("").to_owned(),
                r["chunk"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    assert!(progress.contains(&("stdout".into(), "compiling".into())), "{trace}");
    assert!(progress.contains(&("stdout".into(), "finished".into())), "{trace}");
    assert!(progress.contains(&("stderr".into(), "warning: unused".into())), "{trace}");

    let kinds: Vec<&str> = records.iter().map(|r| r["kind"].as_str().unwrap_or("")).collect();
    assert_eq!(kinds.first(), Some(&"started"));
    assert!(kinds.contains(&"service_started"));
    assert!(kinds.contains(&"service_finished"));
    assert_eq!(kinds.last(), Some(&"stopped"), "{trace}");
}

#[test]
fn run_acp_turn_uses_native_starchart_mcp_bridge_and_streams_artifact_traces() {
    let dir = std::env::temp_dir().join(format!(
        "starchart_cli_mcp_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let agent_py = dir.join("mock_agent.py");
    let chart_path = dir.join("workflow.star");

    std::fs::write(
        &agent_py,
        r#"import json, os, subprocess, sys

def send_acp(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def mcp_rpc(proc, req_id, method, params=None):
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode("utf-8"))
    proc.stdin.flush()
    return json.loads(proc.stdout.readline().decode("utf-8"))

mcp_proc = None
for raw in sys.stdin:
    req = json.loads(raw)
    m, rid = req.get("method"), req.get("id")
    if m == "initialize":
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif m == "session/new":
        srv = req["params"]["mcpServers"][0]
        # Verify the CLI passed its own native `starchart mcp-bridge` binary!
        assert os.path.basename(srv["command"]).startswith("starchart"), f"expected native starchart binary, got {srv}"
        assert srv["args"][0] == "mcp-bridge", f"unexpected args: {srv}"
        mcp_proc = subprocess.Popen([srv["command"]] + srv["args"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        mcp_rpc(mcp_proc, 1, "initialize", {"protocolVersion": "2024-11-05"})
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "cli-sess-1"}})
    elif m == "session/prompt":
        res = mcp_rpc(mcp_proc, 2, "tools/call", {
            "name": "submit_plan",
            "arguments": {"summary": "Native bridge plan", "steps": ["step A", "step B"]}
        })
        assert res["result"]["isError"] is False, res
        mcp_proc.stdin.close()
        mcp_proc.wait(timeout=5)
        send_acp({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        break
"#,
    )
    .expect("write mock_agent.py");

    let chart_src = format!(
        r#"
load("@starchart/acp", "acp_agent", "artifact_tool", "invoke_agent")

Ctx = record(summary = str)
Plan = record(summary = str, steps = list[str])

def save_plan(ctx: Ctx, event: dict) -> Ctx:
    plan = event["output"]["artifacts"]["submit_plan"]
    return Ctx(summary = plan["summary"])

planner = acp_agent(
    id = "planner",
    command = "python3",
    args = ["-u", {agent_path}],
)

S = state_space("planning", "done", "failed")

chart = machine(
    id = "cli_mcp_workflow",
    initial = S.planning,
    context = Ctx(summary = ""),
    states = {{
        S.planning: state(
            invoke = invoke_agent(
                agent = planner,
                prompt = "Make plan",
                tools = [
                    artifact_tool("submit_plan", schema = Plan, description = "Submit plan"),
                ],
                on_done = outcome(target = S.done, assign = save_plan),
                on_error = S.failed,
            ),
        ),
        S.done: final_state(),
        S.failed: final_state(),
    }},
)
"#,
        agent_path = serde_json::to_string(&agent_py.to_string_lossy()).unwrap(),
    );
    std::fs::write(&chart_path, chart_src).expect("write workflow.star");

    let output = Command::new(bin_path())
        .arg("run")
        .arg(&chart_path)
        .arg("--trace")
        .arg("-")
        .output()
        .expect("run starchart run --trace -");

    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let records = parse_jsonl(&stdout);
    let artifact_progress = records
        .iter()
        .find(|r| r["kind"] == "service_progress" && r["stream"] == "artifact")
        .expect("expected `artifact` service_progress record in JSONL trace");
    let chunk: serde_json::Value =
        serde_json::from_str(artifact_progress["chunk"].as_str().unwrap()).unwrap();
    assert_eq!(chunk["tool"], "submit_plan");
    assert_eq!(chunk["artifact"]["summary"], "Native bridge plan");

    let final_transition = records
        .iter()
        .rfind(|r| r["kind"] == "transition")
        .expect("final transition");
    assert_eq!(final_transition["to"], serde_json::json!(["done"]));
    assert_eq!(
        final_transition["context"]["summary"],
        serde_json::json!("Native bridge plan")
    );
}

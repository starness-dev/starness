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

fn spawn_mock_deploy_control_api() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock deploy API");
    let port = listener.local_addr().unwrap().port();
    let incidents = Arc::new(AtomicUsize::new(0));
    let incidents_clone = Arc::clone(&incidents);

    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0_u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let body = if first_line.contains("/v1/deploy-window") {
                r#"{"freeze":false}"#
            } else if first_line.contains("/v1/canary-metrics") {
                r#"{"error_rate":0.001}"#
            } else if first_line.contains("/v1/incidents") {
                incidents_clone.fetch_add(1, Ordering::SeqCst);
                r#"{"paged":true}"#
            } else {
                r#"{}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });

    (port, incidents)
}

#[test]
fn run_events_stdin_drives_deployment_pipeline_standalone_without_control_api() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(bin_path())
        .arg("run")
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg("--events-stdin")
        .arg("--trace")
        .arg("-")
        .arg("--context")
        .arg(r#"{"bake_interval_ms": 20}"#)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn standalone deployment_pipeline");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"type":"APPROVE","by":"alice"}}"#).unwrap();
        writeln!(stdin, r#"{{"type":"APPROVE","by":"joe"}}"#).unwrap();
        writeln!(stdin, "START_ROLLOUT").unwrap();
    }

    let output = child.wait_with_output().expect("wait standalone deployment_pipeline");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let records = parse_jsonl(&stdout);
    let final_transition = records
        .iter()
        .rfind(|r| r["kind"] == "transition")
        .expect("final transition");
    assert_eq!(final_transition["to"], serde_json::json!(["live"]), "trace:\n{stdout}");
    assert_eq!(final_transition["output"], serde_json::json!({"status": "live"}));
    assert_eq!(final_transition["context"]["bake_checks"], 2);
    assert_eq!(
        final_transition["context"]["approved_by"],
        serde_json::json!(["alice", "joe"])
    );
}

#[test]
fn run_events_stdin_drives_deployment_pipeline_with_pause_resume_to_live() {
    use std::io::Write;
    use std::process::Stdio;

    let (port, incidents) = spawn_mock_deploy_control_api();
    let ctx_patch = serde_json::json!({
        "control_api": format!("http://127.0.0.1:{port}"),
        "bake_interval_ms": 40,
    })
    .to_string();

    let mut child = Command::new(bin_path())
        .arg("run")
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg("--events-stdin")
        .arg("--trace")
        .arg("-")
        .arg("--context")
        .arg(&ctx_patch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn starchart run --events-stdin");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "# Approve rollout quorum from two engineers").unwrap();
        writeln!(stdin, r#"{{"type":"APPROVE","by":"alice"}}"#).unwrap();
        writeln!(stdin, r#"{{"type":"APPROVE","by":"bob"}}"#).unwrap();
        writeln!(stdin, "START_ROLLOUT").unwrap();
        writeln!(stdin, "sleep:25").unwrap();
        writeln!(stdin, "PAUSE").unwrap();
        writeln!(stdin, "sleep:20").unwrap();
        writeln!(stdin, "RESUME").unwrap();
    }

    let output = child.wait_with_output().expect("wait deployment_pipeline");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let records = parse_jsonl(&stdout);
    let final_transition = records
        .iter()
        .rfind(|r| r["kind"] == "transition")
        .expect("final transition");
    assert_eq!(final_transition["to"], serde_json::json!(["live"]), "trace:\n{stdout}");
    assert_eq!(final_transition["output"], serde_json::json!({"status": "live"}));
    assert_eq!(
        final_transition["context"]["approved_by"],
        serde_json::json!(["alice", "bob"])
    );
    assert_eq!(incidents.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn run_events_stdin_handles_live_rollback_signal_and_pages_oncall() {
    use std::io::Write;
    use std::process::Stdio;

    let (port, incidents) = spawn_mock_deploy_control_api();
    let ctx_patch = serde_json::json!({
        "control_api": format!("http://127.0.0.1:{port}"),
        "bake_interval_ms": 500,
    })
    .to_string();

    let mut child = Command::new(bin_path())
        .arg("run")
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg("--events-stdin")
        .arg("--trace")
        .arg("-")
        .arg("--context")
        .arg(&ctx_patch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn starchart run --events-stdin");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"type":"APPROVE","by":"alice"}}"#).unwrap();
        writeln!(stdin, r#"{{"type":"APPROVE","by":"bob"}}"#).unwrap();
        writeln!(stdin, "START_ROLLOUT").unwrap();
        // Wait until rollout enters `bake_wait`, then send emergency `ROLLBACK`!
        writeln!(stdin, "sleep:80").unwrap();
        writeln!(stdin, r#"{{"type":"ROLLBACK","reason":"sev1_p99_spike"}}"#).unwrap();
    }

    let output = child.wait_with_output().expect("wait rollback");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let records = parse_jsonl(&stdout);
    let final_transition = records
        .iter()
        .rfind(|r| r["kind"] == "transition")
        .expect("final transition");
    assert_eq!(
        final_transition["to"],
        serde_json::json!(["rolled_back"]),
        "trace:\n{stdout}"
    );
    assert_eq!(
        final_transition["context"]["reason"],
        serde_json::json!("sev1_p99_spike")
    );
    assert_eq!(incidents.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn http_call(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(addr).expect("connect to starchart serve");
    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes()).expect("write http request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read http response");
    let (head, resp_body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("missing http header separator for `{method} {path}`, raw={raw:?}"));
    let status_code: u16 = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status_code, resp_body.to_owned())
}

fn read_bound_addr(stdout: &mut std::io::BufReader<std::process::ChildStdout>) -> String {
    use std::io::BufRead;

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read listening banner");
    let idx = line
        .find("http://")
        .unwrap_or_else(|| panic!("expected `http://` in serve banner, got: {line:?}"));
    line[idx + "http://".len()..].trim().to_owned()
}

#[test]
fn serve_multi_machine_instances_in_memory_sqlite_sse_and_non_completable_actors() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::process::Stdio;

    // Create a temporary non-completable `.star` definition (zero top-level `final_state`s)
    // to host alongside `examples/` on the same server & port.
    let tmp_dir = std::env::temp_dir().join(format!(
        "starchart_serve_multi_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let flag_star = tmp_dir.join("feature_flag.star");
    std::fs::write(
        &flag_star,
        r#"
FlagCtx = record(flips = field(int, 0))
S = state_space("disabled", "enabled")

chart: StateMachine = machine(
    id = "feature_flag",
    description = "Long-lived non-completable toggle actor",
    initial = S.disabled,
    context = FlagCtx(),
    states = {
        S.disabled: state(
            on = [
                transition(
                    event = "TOGGLE",
                    target = S.enabled,
                    assign = lambda ctx, _ev: update(ctx, flips = ctx.flips + 1),
                ),
            ],
        ),
        S.enabled: state(
            on = [
                transition(
                    event = "TOGGLE",
                    target = S.disabled,
                    assign = lambda ctx, _ev: update(ctx, flips = ctx.flips + 1),
                ),
            ],
        ),
    },
)
"#,
    )
    .unwrap();

    // Default `--db` is `:memory:`. Pass both `examples/` directory and `feature_flag.star`!
    let mut child = Command::new(bin_path())
        .arg("serve")
        .arg(workspace_file("examples"))
        .arg(&flag_star)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn multi-machine starchart serve");

    let mut stdout_reader = BufReader::new(child.stdout.take().expect("stdout"));
    let addr = read_bound_addr(&mut stdout_reader);

    // 1. Check GET /health and GET /machines catalog (`completable: true` vs `false`)
    let (code, health_body) = http_call(&addr, "GET", "/health", None);
    assert_eq!(code, 200);
    let health: serde_json::Value = serde_json::from_str(&health_body).unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["db"], ":memory:");
    assert_eq!(health["live_instances"], 0);

    let (code, machines_body) = http_call(&addr, "GET", "/machines", None);
    assert_eq!(code, 200);
    let machines: serde_json::Value = serde_json::from_str(&machines_body).unwrap();
    let list = machines["machines"].as_array().unwrap();
    let deploy_meta = list
        .iter()
        .find(|m| m["id"] == "deployment_pipeline")
        .expect("deployment_pipeline registered");
    assert_eq!(deploy_meta["completable"], true);
    let flag_meta = list
        .iter()
        .find(|m| m["id"] == "feature_flag")
        .expect("feature_flag registered");
    assert_eq!(flag_meta["completable"], false);

    let (code, diagram) = http_call(&addr, "GET", "/machines/deployment_pipeline/diagram", None);
    assert_eq!(code, 200);
    assert!(diagram.starts_with("stateDiagram-v2"));

    // 2. Spawn two independent `deployment_pipeline` instances with caller-supplied IDs
    let (code, inst1_body) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances",
        Some(r#"{"id":"payments-v1","context":{"bake_interval_ms":25}}"#),
    );
    assert_eq!(code, 201, "body={inst1_body}");
    let inst1: serde_json::Value = serde_json::from_str(&inst1_body).unwrap();
    assert_eq!(inst1["id"], "payments-v1");
    assert_eq!(inst1["status"], "waiting");

    // Duplicate ID returns 409 Conflict by default, or 200 OK with `?idempotent=true`
    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances",
        Some(r#"{"id":"payments-v1"}"#),
    );
    assert_eq!(code, 409);
    let (code, idem_body) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances?idempotent=true",
        Some(r#"{"id":"payments-v1"}"#),
    );
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&idem_body).unwrap()["id"],
        "payments-v1"
    );

    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances",
        Some(r#"{"id":"auth-v2","context":{"service":"auth-api"}}"#),
    );
    assert_eq!(code, 201);

    // 3. Stream SSE trace on `payments-v1` while driving it to `live`
    let sse_addr = addr.clone();
    let sse_handle = std::thread::spawn(move || {
        let mut s = TcpStream::connect(&sse_addr).expect("connect SSE");
        write!(
            s,
            "GET /machines/deployment_pipeline/instances/payments-v1/trace HTTP/1.1\r\nHost: {sse_addr}\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let reader = BufReader::new(s);
        let mut data_lines = Vec::new();
        for line in reader.lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("data: ") {
                data_lines.push(rest.to_owned());
            }
        }
        data_lines
    });

    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances/payments-v1/events",
        Some(r#"{"type":"APPROVE","by":"alice"}"#),
    );
    assert_eq!(code, 200);
    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances/payments-v1/events/APPROVE",
        Some(r#"{"by":"bob"}"#),
    );
    assert_eq!(code, 200);
    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances/payments-v1/events/START_ROLLOUT",
        None,
    );
    assert_eq!(code, 200);

    // SSE stream closes automatically once `payments-v1` reaches `completed` and flushes trace!
    let sse_events = sse_handle.join().expect("join SSE");
    assert!(
        sse_events
            .iter()
            .any(|l| l.contains(r#""kind":"started""#) && l.contains(r#""instance_id":"payments-v1""#)),
        "expected started trace for payments-v1: {sse_events:?}"
    );
    assert!(
        sse_events.iter().any(|l| l.contains(r#""to":["live"]"#)),
        "expected transition to live: {sse_events:?}"
    );

    // `payments-v1` is now `completed`, evicted from `live_instances`, and rejects new events with 409,
    // while still serving snapshot + historical trace from `:memory:` SQLite!
    let (code, p1_done) = http_call(
        &addr,
        "GET",
        "/machines/deployment_pipeline/instances/payments-v1",
        None,
    );
    assert_eq!(code, 200);
    let p1_snap: serde_json::Value = serde_json::from_str(&p1_done).unwrap();
    assert_eq!(p1_snap["status"], "completed");
    assert_eq!(p1_snap["done"], true);
    assert_eq!(p1_snap["output"], serde_json::json!({"status": "live"}));

    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/deployment_pipeline/instances/payments-v1/events/APPROVE",
        None,
    );
    assert_eq!(code, 409);

    // Sibling instance `auth-v2` is untouched and still `waiting` (`live_instances == 1`)!
    let (_, health2_body) = http_call(&addr, "GET", "/health", None);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&health2_body).unwrap()["live_instances"],
        1
    );
    let (_, auth_body) = http_call(
        &addr,
        "GET",
        "/machines/deployment_pipeline/instances/auth-v2",
        None,
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&auth_body).unwrap()["status"],
        "waiting"
    );

    // 4. Drive non-completable `feature_flag` actor, then cancel (`DELETE`) and purge (`DELETE ?purge=true`)
    let (code, _) = http_call(
        &addr,
        "POST",
        "/machines/feature_flag/instances",
        Some(r#"{"id":"dark-mode"}"#),
    );
    assert_eq!(code, 201);
    let (code, toggled) = http_call(
        &addr,
        "POST",
        "/machines/feature_flag/instances/dark-mode/events/TOGGLE",
        None,
    );
    assert_eq!(code, 200);
    let toggled_json: serde_json::Value = serde_json::from_str(&toggled).unwrap();
    assert_eq!(toggled_json["status"], "waiting");
    assert_eq!(toggled_json["active_states"], serde_json::json!(["enabled"]));
    assert_eq!(toggled_json["context"]["flips"], 1);

    let (code, cancelled) = http_call(
        &addr,
        "DELETE",
        "/machines/feature_flag/instances/dark-mode",
        None,
    );
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&cancelled).unwrap()["status"],
        "cancelled"
    );

    let (code, _) = http_call(
        &addr,
        "DELETE",
        "/machines/feature_flag/instances/dark-mode?purge=true",
        None,
    );
    assert_eq!(code, 200);
    let (code, _) = http_call(
        &addr,
        "GET",
        "/machines/feature_flag/instances/dark-mode",
        None,
    );
    assert_eq!(code, 404);

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn serve_rehydrates_in_flight_instances_from_sqlite_db_file_across_restart() {
    use std::io::BufReader;
    use std::process::Stdio;

    let (api_port, _incidents) = spawn_mock_deploy_control_api();
    let db_dir = std::env::temp_dir().join(format!(
        "starchart_serve_sqlite_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("starchart.db");

    // --- Process 1: start server with `--db <path>/starchart.db` ---
    let mut server1 = Command::new(bin_path())
        .arg("serve")
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--db")
        .arg(&db_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server1");

    let mut r1 = BufReader::new(server1.stdout.take().unwrap());
    let addr1 = read_bound_addr(&mut r1);

    // Instance A (`fast-done`): completes before crash — should stay `completed` on disk and NOT spawn a live actor on restart.
    let fast_create = serde_json::json!({
        "id": "fast-done",
        "context": { "bake_interval_ms": 15 }
    })
    .to_string();
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances",
        Some(&fast_create),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/fast-done/events/APPROVE",
        Some(r#"{"by":"alice"}"#),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/fast-done/events/APPROVE",
        Some(r#"{"by":"bob"}"#),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/fast-done/events/START_ROLLOUT",
        None,
    );
    for _ in 0..50 {
        let (_, body) = http_call(
            &addr1,
            "GET",
            "/machines/deployment_pipeline/instances/fast-done",
            None,
        );
        if serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"] == "completed" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Instance B (`paused-rollout`): approve, start rollout, pause during `bake_wait` (`shallow_history`), then crash server1!
    let paused_create = serde_json::json!({
        "id": "paused-rollout",
        "context": {
            "control_api": format!("http://127.0.0.1:{api_port}"),
            "bake_interval_ms": 120,
        }
    })
    .to_string();
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances",
        Some(&paused_create),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/paused-rollout/events/APPROVE",
        Some(r#"{"by":"alice"}"#),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/paused-rollout/events/APPROVE",
        Some(r#"{"by":"bob"}"#),
    );
    http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/paused-rollout/events/START_ROLLOUT",
        None,
    );

    for _ in 0..40 {
        let (_, body) = http_call(
            &addr1,
            "GET",
            "/machines/deployment_pipeline/instances/paused-rollout",
            None,
        );
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        if st["active_states"] == serde_json::json!(["rolling_out.bake_wait"]) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let (code, paused_body) = http_call(
        &addr1,
        "POST",
        "/machines/deployment_pipeline/instances/paused-rollout/events/PAUSE",
        None,
    );
    assert_eq!(code, 200);
    let paused_json: serde_json::Value = serde_json::from_str(&paused_body).unwrap();
    assert_eq!(paused_json["status"], "waiting");
    assert_eq!(paused_json["active_states"], serde_json::json!(["paused"]));
    assert_eq!(paused_json["history"]["rolling_out"], "bake_wait");

    let _ = server1.kill();
    let _ = server1.wait();

    // --- Process 2: restart against the same `--db` SQLite file ---
    let mut server2 = Command::new(bin_path())
        .arg("serve")
        .arg(workspace_file("examples/deployment_pipeline.star"))
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--db")
        .arg(&db_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server2");

    let mut r2 = BufReader::new(server2.stdout.take().unwrap());
    let addr2 = read_bound_addr(&mut r2);

    // Only `paused-rollout` was rehydrated into RAM (`live_instances == 1`), while `fast-done` stays queryable from SQLite!
    let (_, h2_body) = http_call(&addr2, "GET", "/health", None);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&h2_body).unwrap()["live_instances"],
        1
    );

    let (code, fast_snap) = http_call(
        &addr2,
        "GET",
        "/machines/deployment_pipeline/instances/fast-done",
        None,
    );
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fast_snap).unwrap()["status"],
        "completed"
    );

    let (code, resumed_initial) = http_call(
        &addr2,
        "GET",
        "/machines/deployment_pipeline/instances/paused-rollout",
        None,
    );
    assert_eq!(code, 200);
    let init2: serde_json::Value = serde_json::from_str(&resumed_initial).unwrap();
    assert_eq!(init2["active_states"], serde_json::json!(["paused"]));
    assert_eq!(
        init2["context"]["approved_by"],
        serde_json::json!(["alice", "bob"])
    );

    // Send `RESUME` and verify `shallow_history` returns to `rolling_out.bake_wait` and completes to `live`!
    let (code, after_resume) = http_call(
        &addr2,
        "POST",
        "/machines/deployment_pipeline/instances/paused-rollout/events/RESUME",
        None,
    );
    assert_eq!(code, 200);
    let res_json: serde_json::Value = serde_json::from_str(&after_resume).unwrap();
    assert_eq!(
        res_json["active_states"],
        serde_json::json!(["rolling_out.bake_wait"])
    );

    let mut reached_live = false;
    for _ in 0..50 {
        let (_, body) = http_call(
            &addr2,
            "GET",
            "/machines/deployment_pipeline/instances/paused-rollout",
            None,
        );
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        if st["status"] == "completed" && st["active_states"] == serde_json::json!(["live"]) {
            reached_live = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(reached_live, "expected paused-rollout to reach `live` after RESUME");

    // Verify trace `seq` remained strictly monotonic across the SQLite restart!
    let (code, trace_jsonl) = http_call(
        &addr2,
        "GET",
        "/machines/deployment_pipeline/instances/paused-rollout/trace?format=jsonl",
        None,
    );
    assert_eq!(code, 200);
    let trace_records = parse_jsonl(&trace_jsonl);
    assert!(trace_records.len() >= 4);
    for pair in trace_records.windows(2) {
        assert!(
            pair[1]["seq"].as_u64().unwrap() > pair[0]["seq"].as_u64().unwrap(),
            "expected strictly monotonic trace seq across restart: {pair:?}"
        );
    }

    let _ = server2.kill();
    let _ = server2.wait();
    let _ = std::fs::remove_dir_all(&db_dir);
}

//! Built-in `@starchart/exec` async subprocess command runner.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Executes an `ExecSpec` (or `list[str]`) command specification from `@starchart/exec`.
/// Uses `kill_on_drop(true)` so exiting the owning state immediately terminates the child process.
pub async fn execute_command(_ctx: Value, input: Value) -> anyhow::Result<Value> {
    let (cmd_vec, cwd, env_map, stdin_str, timeout_ms, check) = if let Some(arr) = input.as_array() {
        let args = parse_string_list(arr)?;
        (args, None, None, None, 60_000_u64, true)
    } else if let Some(obj) = input.as_object() {
        let arr = obj
            .get("cmd")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("ExecSpec missing `cmd` string list"))?;
        let args = parse_string_list(arr)?;
        let cwd = obj.get("cwd").and_then(Value::as_str).map(str::to_owned);
        let env_map = obj.get("env").and_then(Value::as_object).cloned();
        let stdin_str = obj.get("stdin").and_then(Value::as_str).map(str::to_owned);
        let timeout_ms = obj
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000);
        let check = obj.get("check").and_then(Value::as_bool).unwrap_or(true);
        (args, cwd, env_map, stdin_str, timeout_ms, check)
    } else {
        bail!("`@starchart/exec` input must be an ExecSpec object or string list, got `{input}`");
    };

    let Some((program, args)) = cmd_vec.split_first() else {
        bail!("`@starchart/exec` `cmd` must contain at least a program name");
    };

    let mut command = Command::new(program);
    command
        .args(args)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_str.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    if let Some(dir) = &cwd {
        command.current_dir(dir);
    }

    if let Some(envs) = &env_map {
        for (k, v) in envs {
            if let Some(val_str) = v.as_str() {
                command.env(k, val_str);
            }
        }
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn command `{program}`"))?;

    if let Some(stdin_payload) = stdin_str
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin
            .write_all(stdin_payload.as_bytes())
            .await
            .context("failed writing stdin to child process")?;
        drop(child_stdin);
    }

    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("command `{program}` timed out after {timeout_ms}ms"))?
        .with_context(|| format!("failed waiting on command `{program}`"))?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if check && !output.status.success() {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        bail!("command `{program}` exited with code {code}: {detail}");
    }

    Ok(json!({
        "code": code,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

fn parse_string_list(arr: &[Value]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("command element must be a string, got `{item}`"))?;
        out.push(s.to_owned());
    }
    Ok(out)
}

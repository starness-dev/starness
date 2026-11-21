//! Built-in `@starness/exec` async subprocess command runner.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::builtins::secrets::{SecretScrubber, resolve_secret_string_map};
use crate::trace::{ProgressKind, progress_enabled, report_progress};

/// Executes an `ExecSpec` (or `list[str]`) command specification from `@starness/exec`.
/// Uses `kill_on_drop(true)` so exiting the owning state immediately terminates the child process.
pub async fn execute_command(_ctx: Value, input: Value) -> anyhow::Result<Value> {
    let mut scrubber = SecretScrubber::new();
    let res = execute_command_inner(&input, &mut scrubber).await;
    scrubber.scrub_result(res)
}

async fn execute_command_inner(
    input: &Value,
    scrubber: &mut SecretScrubber,
) -> anyhow::Result<Value> {
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
        bail!("`@starness/exec` input must be an ExecSpec object or string list");
    };

    let resolved_envs = resolve_secret_string_map(env_map.as_ref(), scrubber)?;

    let Some((program, args)) = cmd_vec.split_first() else {
        bail!("`@starness/exec` `cmd` must contain at least a program name");
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

    for (k, val_str) in &resolved_envs {
        command.env(k, val_str.expose_secret());
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

    let stdout_pipe = child.stdout.take().context("child stdout not piped")?;
    let stderr_pipe = child.stderr.take().context("child stderr not piped")?;

    // Read both pipes line by line (forwarding each line as trace progress) while waiting
    // for exit. Everything runs in this future, so cancelling the invoke drops the child
    // (`kill_on_drop`) and the readers together.
    let run = async {
        let (stdout_bytes, stderr_bytes, status) = tokio::try_join!(
            read_streamed(stdout_pipe, ProgressKind::Stdout),
            read_streamed(stderr_pipe, ProgressKind::Stderr),
            child.wait(),
        )?;
        Ok::<_, std::io::Error>((stdout_bytes, stderr_bytes, status))
    };

    let (stdout_bytes, stderr_bytes, status) =
        tokio::time::timeout(Duration::from_millis(timeout_ms), run)
            .await
            .map_err(|_| anyhow::anyhow!("command `{program}` timed out after {timeout_ms}ms"))?
            .with_context(|| format!("failed waiting on command `{program}`"))?;

    let code = status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if check && !status.success() {
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

/// Reads `pipe` to EOF, returning all bytes and reporting each line as `stream` progress.
async fn read_streamed<R: AsyncRead + Unpin>(
    pipe: R,
    stream: ProgressKind,
) -> std::io::Result<Vec<u8>> {
    let mut reader = BufReader::new(pipe);
    let mut all = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            break;
        }
        all.extend_from_slice(&line);
        if progress_enabled() {
            let text = String::from_utf8_lossy(&line);
            report_progress(stream, text.trim_end_matches(['\n', '\r']));
        }
    }
    Ok(all)
}

//! `execute_command` — mirrors Go main.go (~1392 handler + `runCommand`
//! ~788). Output framing is the parity-critical piece and MUST stay
//! byte-identical to the Go server:
//!
//! ```text
//! cwd: <abs cwd>
//! command: <command>
//! exit_code: N
//! stdout:
//! <stdout>
//! [stderr:
//! <stderr>]
//! ```
//!
//! and on timeout (with partial captured output):
//! ```text
//! timeout framing: "timed_out: true\nstdout:\n…\nstderr:\n…"
//! ```

use std::process::Stdio;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Ctx, DEFAULT_COMMAND_TIMEOUT_MS, clip, default_string, errno_display, safe_path};
use crate::auth::child_env;
use crate::safety::GateResult;
use crate::shell::{normalize_lf, shell_command};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ExecuteInput {
    /// command to execute
    pub command: String,
    /// working directory relative to workspace
    #[serde(default)]
    pub cwd: Option<String>,
    /// timeout in milliseconds
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

/// Runs the tool and always produces the final text (Go mirrors this by
/// never returning Go-errors from the handler — errors become
/// `command_error: …` text payloads).
pub async fn run(ctx: &Ctx, input: ExecuteInput) -> String {
    if input.command.is_empty() {
        return "command_error: command is required".to_string();
    }
    let cwd = match safe_path(ctx, &default_string(input.cwd.as_deref(), ".")) {
        Ok(p) => p,
        Err(e) => return format!("command_error: {e}"),
    };
    // Phase 3 gate: sidecar + audit (enforce/observe/off all audit).
    if let GateResult::Blocked(text) =
        crate::safety::gate_shell(ctx, "execute_command", &input.command, &cwd).await
    {
        return text;
    }
    let timeout = match input.timeout_ms {
        Some(ms) if ms > 0 => Duration::from_millis(ms as u64),
        _ => Duration::from_millis(DEFAULT_COMMAND_TIMEOUT_MS),
    };
    match run_command(&input.command, &cwd, timeout).await {
        Ok(out) => format!("cwd: {cwd}\ncommand: {}\n{out}", input.command),
        Err(spawn_err) => format!("command_error: {spawn_err}"),
    }
}

/// Mirrors Go `runCommand`: run via `shellCommand`, `cmd.Env = childEnv()`,
/// timeout kills the child and yields partial output. Spawn failures are
/// the only error propagation (mirroring Go's non-ExitError branch).
async fn run_command(command: &str, cwd: &str, timeout: Duration) -> Result<String, String> {
    let (name, args) = shell_command(command);
    let mut cmd = tokio::process::Command::new(&name);
    cmd.args(&args)
        .current_dir(cwd)
        .env_clear()
        .envs(child_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start {name} in {cwd}: {}", errno_display(&e)))?;

    // Drain pipes concurrently in tasks so a big output can never block the
    // wait, and whatever was flushed before a timeout kill is preserved
    // (same observable behavior as Go's CommandContext kill + buffers).
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = vec![];
        if let Some(mut p) = stdout_pipe {
            use tokio::io::AsyncReadExt;
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = vec![];
        if let Some(mut p) = stderr_pipe {
            use tokio::io::AsyncReadExt;
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    });

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            // Go's ExitCode() reports -1 when the process was killed by a
            // signal; status.code() is None in exactly that case (unix).
            let code = status.code().unwrap_or(-1);
            let mut out = format!(
                "exit_code: {code}\nstdout:\n{}",
                clip(&normalize_lf(&lossy(&stdout)))
            );
            if !stderr.is_empty() {
                out += &format!("\nstderr:\n{}", clip(&normalize_lf(&lossy(&stderr))));
            }
            Ok(out)
        }
        Ok(Err(wait_err)) => {
            // Wait failed after a successful spawn — Go's non-ExitError
            // branch propagates the raw error; surface its real text.
            Err(format!("wait for command: {}", errno_display(&wait_err)))
        }
        Err(_elapsed) => {
            // Timeout: kill like CommandContext, then drain what was flushed.
            // (Kill errors are ignored, like Go's Kill.)
            let _ = child.start_kill();
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            Ok(format!(
                "timed_out: true\nstdout:\n{}\nstderr:\n{}",
                clip(&normalize_lf(&lossy(&stdout))),
                clip(&normalize_lf(&lossy(&stderr)))
            ))
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx::plain("/tmp")
    }

    #[tokio::test]
    async fn requires_command() {
        let out = run(
            &ctx(),
            ExecuteInput {
                command: String::new(),
                cwd: None,
                timeout_ms: None,
            },
        )
        .await;
        assert_eq!(out, "command_error: command is required");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn success_framing_matches_go() {
        let out = run(
            &ctx(),
            ExecuteInput {
                command: "echo hello".to_string(),
                cwd: None,
                timeout_ms: None,
            },
        )
        .await;
        // The cwd line contains the process cwd — framing is what matters.
        let expected_tail = "command: echo hello\nexit_code: 0\nstdout:\nhello\n".to_string();
        assert!(out.ends_with(&expected_tail), "got: {out:?}");
        assert!(out.starts_with("cwd: /"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_only_when_present() {
        let out = run(
            &ctx(),
            ExecuteInput {
                command: "echo err-text 1>&2; echo ok".to_string(),
                cwd: None,
                timeout_ms: None,
            },
        )
        .await;
        assert!(
            out.ends_with("exit_code: 0\nstdout:\nok\n\nstderr:\nerr-text\n"),
            "got: {out:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_framing() {
        let out = run(
            &ctx(),
            ExecuteInput {
                command: "echo partial; sleep 5".to_string(),
                cwd: None,
                timeout_ms: Some(300),
            },
        )
        .await;
        assert!(
            out.contains("timed_out: true\nstdout:\npartial\n\nstderr:\n"),
            "got: {out:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_token_never_leaks_to_children() {
        // Simulate an inherited token and confirm a child cannot see it.
        let vars = vec![
            (
                std::ffi::OsString::from("HOME"),
                std::ffi::OsString::from("/x"),
            ),
            (
                std::ffi::OsString::from("ryns_auth_token"),
                std::ffi::OsString::from("leak"),
            ),
        ];
        let filtered = crate::auth::child_env_filter(vars);
        assert!(
            filtered
                .iter()
                .all(|(k, _)| k.to_string_lossy() != "ryns_auth_token")
        );
    }
}

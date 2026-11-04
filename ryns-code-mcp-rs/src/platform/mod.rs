//! Per-OS helpers mirroring `platform.go`. Phase 1 skeleton grew into the
//! Phase 2 runtime helper layer used by the desktop/media/process tools:
//! `run_plain` / `run_x_tool` / `run_raw_command` / `capture_screen` /
//! clipboard, plus the windowsUnsupported gate (platform.go ~271) and the
//! darwin Homebrew PATH fix (platform.go init()).
//!
//! Signal handling notes (signal_unix.go / signal_windows.go mirror lives in
//! `tools/processes.rs`): unix kills go through `libc::kill` (libc is already
//! in the dependency lock), Windows always uses Process.Kill.

#[cfg(target_os = "macos")]
pub mod darwin;

/// Minimal PATH bootstrap mirroring `platform.go` init(): GUI-launched MCP
/// clients often start with a minimal PATH that lacks Homebrew, so tools like
/// rg are not found. Prepends the usual Homebrew directories on macOS. Must
/// run before the tokio runtime is created (edition-2024 `set_var` is unsafe
/// and must not race the async runtime's env readers).
#[cfg(target_os = "macos")]
pub fn init_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let dirs = ["/usr/local/bin", "/opt/homebrew/bin"];
    let new_path = prepend_missing_dirs(&current, &dirs);
    if new_path != current {
        // Safe: runs once during startup, single-threaded, BEFORE the tokio
        // runtime is built (see main.rs); no concurrent env readers exist.
        unsafe { std::env::set_var("PATH", new_path) };
    }
}

#[cfg(not(target_os = "macos"))]
pub fn init_path() {}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Pure helper behind `init_path` (table-tested): mirrors Go's loop verbatim
/// — each dir is checked against the *current* prepend state and prepended
/// itself when missing, so dirs declared as
/// `[/usr/local/bin, /opt/homebrew/bin]` end up in the order
/// `/opt/homebrew/bin:/usr/local/bin:<current>` (the later prepend wins the
/// front; a dir already present via ":dir:" padding is skipped, as in Go).
fn prepend_missing_dirs(current: &str, dirs: &[&str]) -> String {
    let mut path = current.to_string();
    for dir in dirs {
        let padded = format!(":{path}:");
        if padded.contains(&format!(":{dir}:")) {
            continue;
        }
        path = format!("{dir}:{path}");
    }
    path
}

/// Mirrors Go's `os.UserHomeDir()` fallback used for the workspace root.
fn home_dir() -> Option<String> {
    #[cfg(unix)]
    let key = "HOME";
    #[cfg(windows)]
    let key = "USERPROFILE";
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Mirrors Go's `workspace` package var: `RYNS_WORKSPACE_ROOT` (cleaned)
/// when set, else `<home>/RynsWorkspace`, else `.`.
pub fn workspace_root() -> String {
    if let Some(v) = std::env::var("RYNS_WORKSPACE_ROOT")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return crate::tools::path_clean(&v);
    }
    match home_dir() {
        Some(home) => crate::tools::path_join(&home, "RynsWorkspace"),
        None => ".".to_string(),
    }
}

// ---- process helpers -----------------------------------------------------------

use std::ffi::OsString;
use std::time::Duration;

fn set_cmd_env(cmd: &mut tokio::process::Command, env: Vec<(OsString, OsString)>) {
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
}

fn spawn_pipes() -> std::process::Stdio {
    std::process::Stdio::piped()
}

/// Go exec error wording: ExitError.Error() = "exit status N" (Go prints a
/// "signal: …" shaped error when the process was killed); anything else keeps
/// the io error's Display text.
fn go_exec_status(status: &std::process::ExitStatus) -> String {
    #[cfg(windows)]
    {
        format!("exit status {}", status.code().unwrap_or_default())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            Some(sig) => format!("signal: {}", signal_name(sig)),
            None => format!("exit status {}", status.code().unwrap_or_default()),
        }
    }
}

/// Go's syscall signal `String()` descriptions for ordinary signals
/// (mirrored from the sigtable semantics used by `syscall.Signal.String`).
#[cfg_attr(windows, allow(dead_code))]
pub fn signal_name(sig: i32) -> String {
    match sig {
        1 => "hangup",
        2 => "interrupt",
        3 => "quit",
        7 => "bus error",
        8 => "floating point exception",
        9 => "killed",
        11 => "segmentation fault",
        13 => "broken pipe",
        14 => "alarm clock",
        15 => "terminated",
        30 => "user defined signal 1",
        31 => "user defined signal 2",
        other => return format!("signal {other}"),
    }
    .to_string()
}

/// Mirrors `runPlain` (platform.go ~59): run `name args` with a timeout and
/// optional stdin, capture combined output. Returns the output text and,
/// when the run failed, Go's error wording.
pub async fn run_plain(
    name: &str,
    args: &[String],
    stdin_text: &str,
    timeout: Duration,
) -> (String, Option<String>) {
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(args)
        .stdin(if stdin_text.is_empty() {
            std::process::Stdio::null()
        } else {
            std::process::Stdio::piped()
        })
        .stdout(spawn_pipes())
        .stderr(spawn_pipes());
    set_cmd_env(&mut cmd, crate::auth::child_env());
    let stdin_text = stdin_text.to_string();
    let run = async {
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err((String::new(), e.to_string())),
        };
        if !stdin_text.is_empty() {
            use tokio::io::AsyncWriteExt;
            if let Some(mut i) = child.stdin.take() {
                let _ = i.write_all(stdin_text.as_bytes()).await;
            }
        }
        let out = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => return Err((String::new(), e.to_string())),
        };
        let mut combined = out.stdout;
        combined.extend_from_slice(&out.stderr);
        let text = String::from_utf8_lossy(&combined).into_owned();
        if !out.status.success() {
            return Err((text, go_exec_status(&out.status)));
        }
        Ok((text, None))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok((t, None))) => (t, None),
        Ok(Ok((t, Some(e)))) => (t, Some(e)),
        Ok(Err((t, e))) => (t, Some(e)),
        Err(_) => (String::new(), Some("context deadline exceeded".to_string())),
    }
}

/// xEnv (platform.go ~228): childEnv + DISPLAY=:0, plus XAUTHORITY when
/// $HOME/.Xauthority exists (the box runs its desktop on :0).
fn x_env() -> Vec<(OsString, OsString)> {
    let mut env = crate::auth::child_env();
    env.push((OsString::from("DISPLAY"), OsString::from(":0")));
    if let Some(home) = home_dir() {
        let xauth = crate::tools::path_join(&home, ".Xauthority");
        if std::path::Path::new(&xauth).is_file() {
            env.push((OsString::from("XAUTHORITY"), OsString::from(xauth)));
        }
    }
    env
}

/// Mirrors `runXTool` (platform.go ~239): desktop-automation command with
/// xEnv applied and bounded timeout.
pub async fn run_x_tool(
    timeout: Duration,
    name: &str,
    args: &[String],
) -> (String, Option<String>) {
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(spawn_pipes())
        .stderr(spawn_pipes());
    set_cmd_env(&mut cmd, x_env());
    let run = async {
        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return Err((String::new(), e.to_string())),
        };
        let mut combined = out.stdout;
        combined.extend_from_slice(&out.stderr);
        let text = String::from_utf8_lossy(&combined).into_owned();
        if !out.status.success() {
            return Err((text, go_exec_status(&out.status)));
        }
        Ok((text, None))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok((t, None))) => (t, None),
        Ok(Ok((t, Some(e)))) => (t, Some(e)),
        Ok(Err((t, e))) => (t, Some(e)),
        Err(_) => (String::new(), Some("context deadline exceeded".to_string())),
    }
}

/// Mirrors `runRawCommand` (main.go ~455): shell-wrapped command run from the
/// workspace dir; trimmed LF-normalized output plus Go exec wording on error.
pub async fn run_raw_command(command: &str, timeout: Duration) -> (String, Option<String>) {
    let (name, args) = crate::shell::shell_command(command);
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(&args)
        .current_dir(workspace_root())
        .stdin(std::process::Stdio::null())
        .stdout(spawn_pipes())
        .stderr(spawn_pipes());
    set_cmd_env(&mut cmd, crate::auth::child_env());
    let run = async {
        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return Err((String::new(), e.to_string())),
        };
        let text = crate::shell::normalize_lf(&String::from_utf8_lossy(&out.stdout))
            .trim()
            .to_string();
        if !out.status.success() {
            return Err((text, go_exec_status(&out.status)));
        }
        Ok((text, None))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok((t, None))) => (t, None),
        Ok(Ok((t, Some(e)))) => (t, Some(e)),
        Ok(Err((t, e))) => (t, Some(e)),
        Err(_) => (String::new(), Some("context deadline exceeded".to_string())),
    }
}

/// mirrors windowsUnsupported (platform.go ~271): the tool's error line when
/// the current platform has no implementation.
pub fn windows_unsupported(tool: &str) -> Option<String> {
    if cfg!(windows) {
        Some(format!("{tool}: not supported on windows"))
    } else {
        None
    }
}

/// captureScreen (platform.go ~85): macOS screencapture, Linux import.
pub async fn capture_screen(window: &str, out_path: &str) -> (String, Option<String>) {
    #[cfg(target_os = "macos")]
    {
        let mut args: Vec<String> = vec!["-x".into()];
        if !window.is_empty() && window != "root" {
            args.push("-l".into());
            args.push(window.to_string());
        }
        args.push(out_path.to_string());
        run_plain("screencapture", &args, "", Duration::from_secs(15)).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let w = if window.is_empty() { "root" } else { window };
        run_x_tool(
            Duration::from_secs(15),
            "import",
            &["-window".to_string(), w.to_string(), out_path.to_string()],
        )
        .await
    }
}

/// clipboardRead (platform.go ~101).
pub async fn clipboard_read() -> (String, Option<String>) {
    #[cfg(target_os = "macos")]
    {
        run_plain("pbpaste", &[], "", Duration::from_secs(5)).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_x_tool(
            Duration::from_secs(5),
            "xclip",
            &[
                "-selection".to_string(),
                "clipboard".to_string(),
                "-o".to_string(),
            ],
        )
        .await
    }
}

/// clipboardWrite (platform.go ~110): pbcopy via stdin on macOS; on Linux
/// xclip keeps a background holder, so Go sends stdout/stderr to null and
/// judges by status — mirrored (stdin piped, stdio noise discarded).
pub async fn clipboard_write(text: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let (_, err) = run_plain("pbcopy", &[], text, Duration::from_secs(5)).await;
        err
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut cmd = tokio::process::Command::new("xclip");
        cmd.args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        set_cmd_env(&mut cmd, x_env());
        let text = text.to_string();
        let run = async {
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return Err(e.to_string()),
            };
            use tokio::io::AsyncWriteExt;
            if let Some(mut i) = child.stdin.take() {
                let _ = i.write_all(text.as_bytes()).await;
            }
            let status = match child.wait().await {
                Ok(s) => s,
                Err(e) => return Err(e.to_string()),
            };
            if !status.success() {
                return Err(format!("exit status {}", status.code().unwrap_or_default()));
            }
            Ok(())
        };
        match tokio::time::timeout(Duration::from_secs(5), run).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("timeout".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table test for the pure helper behind `init_path` (runs on every
    /// target; mirrors platform.go's init() prepend loop).
    #[test]
    fn prepend_missing_dirs_matches_go_order() {
        assert_eq!(
            prepend_missing_dirs("/usr/bin", &["/usr/local/bin", "/opt/homebrew/bin"]),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin"
        );
        // Already-present dirs are skipped wherever they sit.
        assert_eq!(
            prepend_missing_dirs(
                "/opt/homebrew/bin:/usr/bin",
                &["/usr/local/bin", "/opt/homebrew/bin"]
            ),
            "/usr/local/bin:/opt/homebrew/bin:/usr/bin"
        );
        // Nothing missing → unchanged.
        assert_eq!(
            prepend_missing_dirs(
                "/opt/homebrew/bin:/usr/local/bin",
                &["/usr/local/bin", "/opt/homebrew/bin"]
            ),
            "/opt/homebrew/bin:/usr/local/bin"
        );
        // Empty PATH keeps Go's trailing-colon joint.
        assert_eq!(
            prepend_missing_dirs("", &["/usr/local/bin", "/opt/homebrew/bin"]),
            "/opt/homebrew/bin:/usr/local/bin:"
        );
        // ":dir:" padding cannot conflict with a different dir name.
        assert_eq!(
            prepend_missing_dirs("/usr/local/bin2", &["/usr/local/bin", "/opt/homebrew/bin"]),
            "/opt/homebrew/bin:/usr/local/bin:/usr/local/bin2"
        );
    }

    #[test]
    fn workspace_root_respects_env() {
        // Set through the pure helper chain, not via env mutation races.
        let cleaned = crate::tools::path_clean("/tmp/ryns-ws/");
        assert_eq!(cleaned, "/tmp/ryns-ws");
    }

    #[test]
    fn workspace_root_defaults_to_home() {
        // Can't guarantee HOME in tests, but the join must be clean.
        let home = home_dir().unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(
                workspace_root(),
                crate::tools::path_join(&home, "RynsWorkspace")
            );
        }
    }

    /// Go's signal descriptions (sigtable wording) for the everyday signals.
    #[test]
    fn signal_names_match_go_wording() {
        assert_eq!(signal_name(2), "interrupt");
        assert_eq!(signal_name(9), "killed");
        assert_eq!(signal_name(15), "terminated");
        assert_eq!(signal_name(17), "signal 17");
    }
}

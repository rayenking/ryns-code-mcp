//! Process tools mirroring Go's start_process / process_output / send_input /
//! kill_process / list_processes (main.go ~1411-1516, ~818-918) and the
//! signal split (signal_unix.go / signal_windows.go).
//!
//! PTY: interactive start uses Go's `script(1)` trick (ptyCommand, platform.go
//! ~280) — no extra platform APIs, same behavior as Go. Windows: Go drops the
//! PTY (`interactive=false` + note) and kill_process always uses Process.Kill.
//!
//! Output buffers keep the LAST 2×MAX_OUTPUT bytes (Go copyOutput cap).

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Map;

use super::{Ctx, MAX_OUTPUT, default_string, safe_path};
use crate::auth::child_env;
use crate::safety::GateResult;
use crate::shell::{normalize_lf, shell_command};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartInput {
    pub command: String,
    #[serde(default, rename = "cwd")]
    pub cwd: Option<String>,
    #[serde(default, rename = "interactive")]
    pub interactive: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessInput {
    pub process_id: String,
    #[serde(default)]
    pub clear: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendInput {
    pub process_id: String,
    pub input: String,
    #[serde(default)]
    pub newline: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KillInput {
    pub process_id: String,
    #[serde(default)]
    pub signal: Option<String>,
}

const BUF_CAP: usize = MAX_OUTPUT * 2;

pub struct Proc {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub started_at: String,
    stdout: Mutex<String>,
    stderr: Mutex<String>,
    exit_code: Mutex<Option<i64>>,
    sig_name: Mutex<String>,
    done: AtomicBool,
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Proc {
    pub fn done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

fn registry() -> &'static Mutex<BTreeMap<String, Arc<Proc>>> {
    static MAP: Mutex<BTreeMap<String, Arc<Proc>>> = Mutex::new(BTreeMap::new());
    &MAP
}

/// Live process ids (registration order) for workspace_info.
pub fn active_ids() -> Vec<String> {
    registry().lock().unwrap().keys().cloned().collect()
}

/// Proc row JSON shared by process_output (started_at is list-only; Go adds
/// it in the list_processes loop, but NOT in process_output's map).
fn proc_row(p: &Proc) -> Map<String, serde_json::Value> {
    let mut m = Map::new();
    m.insert("process_id".into(), p.id.clone().into());
    m.insert("command".into(), p.command.clone().into());
    m.insert("cwd".into(), p.cwd.clone().into());
    m.insert("done".into(), p.done().into());
    m.insert("exit_code".into(), (*p.exit_code.lock().unwrap()).into());
    m.insert("signal".into(), p.sig_name.lock().unwrap().clone().into());
    m
}

/// Mirror of Go `startProcess` (main.go ~839): spawn under shell/PTY,
/// register with a sequential id, drain pipes into capped tail buffers, and
/// reap exit status like Go's wait goroutine.
async fn spawn_registered(
    command: &str,
    cwd: &str,
    interactive: bool,
) -> Result<Arc<Proc>, String> {
    let mut cmd = build_cmd(command, interactive);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env_clear();
    for (k, v) in child_env() {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start {command} in {cwd}: {e}"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let id = NEXT_PROC_ID.fetch_add(1, Ordering::SeqCst).to_string();
    let p = Arc::new(Proc {
        id: id.clone(),
        command: command.to_string(),
        cwd: cwd.to_string(),
        started_at: super::rfc3339_nano(std::time::SystemTime::now()),
        stdout: Mutex::new(String::new()),
        stderr: Mutex::new(String::new()),
        exit_code: Mutex::new(None),
        sig_name: Mutex::new(String::new()),
        done: AtomicBool::new(false),
        stdin: Mutex::new(stdin),
        child: Mutex::new(Some(child)),
    });
    registry().lock().unwrap().insert(id.clone(), p.clone());

    // Drain tasks (Go copyOutput): capped tail; EOF or read error ends them.
    {
        use tokio::io::AsyncReadExt;
        async fn drain<R: tokio::io::AsyncRead + Unpin>(
            pipe: Option<R>,
            slot: usize,
            p: Arc<Proc>,
        ) {
            let Some(pipe) = pipe else { return };
            let mut pipe = pipe;
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        let lock = if slot == 0 { &p.stdout } else { &p.stderr };
                        let mut text = lock.lock().unwrap();
                        text.push_str(&chunk);
                        if text.len() > BUF_CAP {
                            let cut = text.len() - BUF_CAP;
                            *text = String::from_utf8_lossy(&text.as_bytes()[cut..]).into_owned();
                        }
                    }
                }
            }
        }
        tokio::spawn(drain(stdout, 0, p.clone()));
        tokio::spawn(drain(stderr, 1, p.clone()));
    }

    // Reap task (Go's wait goroutine): Done flag, exit code, signal name.
    // The child handle is taken out of the std::Mutex while awaited so the
    // future stays Send (guards may never cross an await point).
    {
        let p = p.clone();
        tokio::spawn(async move {
            let child = p.child.lock().unwrap().take();
            let Some(mut child) = child else {
                p.done.store(true, Ordering::SeqCst);
                return;
            };
            let status = child.wait().await;
            p.done.store(true, Ordering::SeqCst);
            if let Ok(status) = status {
                *p.exit_code.lock().unwrap() = Some(status.code().unwrap_or(-1) as i64);
                *p.sig_name.lock().unwrap() = exit_signal(&status);
            }
            *p.child.lock().unwrap() = Some(child);
        });
    }
    Ok(p)
}

static NEXT_PROC_ID: AtomicU64 = AtomicU64::new(1);

/// Go `startProcess` command choice: interactive → ptyCommand (`script`),
/// else shellCommand. Windows forces non-interactive at the run_start layer.
fn build_cmd(command: &str, interactive: bool) -> tokio::process::Command {
    let mut cmd;
    if interactive {
        #[cfg(target_os = "macos")]
        {
            // script -q /dev/null <shell args>  (BSD script syntax)
            let (_, shell_args) = shell_command(command);
            cmd = tokio::process::Command::new("script");
            let mut all = vec!["-q".to_string(), "/dev/null".to_string()];
            all.extend(shell_args.iter().cloned());
            cmd.args(&all);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // script -qefc <command> /dev/null  (util-linux syntax)
            cmd = tokio::process::Command::new("script");
            cmd.args(["-qefc", command, "/dev/null"]);
        }
        #[cfg(windows)]
        {
            // Unreachable (interactive forced false on windows); sane fallback.
            let (name, args) = shell_command(command);
            cmd = tokio::process::Command::new(name);
            cmd.args(&args);
        }
    } else {
        let (name, args) = shell_command(command);
        cmd = tokio::process::Command::new(name);
        cmd.args(&args);
    }
    cmd
}

/// Mirror of signal_unix.exitSignal: the killing signal's Go name
/// (syscall.Signal.String()), "" on Windows (signal_windows.go), "" when the
/// process exited normally.
fn exit_signal(status: &std::process::ExitStatus) -> String {
    #[cfg(windows)]
    {
        let _ = status;
        String::new()
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .signal()
            .map(crate::platform::signal_name)
            .unwrap_or_default()
    }
}

/// start_process (main.go ~1411).
pub async fn run_start(ctx: &Ctx, input: StartInput) -> String {
    let cwd = match safe_path(ctx, &default_string(input.cwd.as_deref(), ".")) {
        Ok(c) => c,
        Err(e) => return format!("start_error: {e}"),
    };
    // Phase 3 gate: sidecar check on process starts (all modes audit).
    if let GateResult::Blocked(text) =
        crate::safety::gate_shell(ctx, "start_process", &input.command, &cwd).await
    {
        return text;
    }
    let mut interactive = input.interactive.unwrap_or(true);
    let note = if cfg!(windows) && interactive {
        interactive = false;
        "windows: no PTY support, process started non-interactively via PowerShell"
    } else {
        ""
    };
    let p = match spawn_registered(&input.command, &cwd, interactive).await {
        Ok(p) => p,
        Err(e) => return format!("start_error: {e}"),
    };
    let mut row = Map::new();
    row.insert("process_id".into(), p.id.clone().into());
    row.insert("command".into(), p.command.clone().into());
    row.insert("cwd".into(), p.cwd.clone().into());
    row.insert("started_at".into(), p.started_at.clone().into());
    row.insert("interactive".into(), interactive.into());
    if !note.is_empty() {
        row.insert("note".into(), note.to_string().into());
    }
    super::must_json(&row)
}

/// process_output (main.go ~1438): read the capped tail buffers; CRLF is
/// normalized at read time only (like Go), and `clear` empties the buffers.
pub async fn run_output(input: ProcessInput) -> String {
    let p = match registry().lock().unwrap().get(&input.process_id) {
        Some(p) => p.clone(),
        None => return format!("process_not_found: {}", input.process_id),
    };
    let value = {
        let mut row = proc_row(&p);
        let stdout = p.stdout.lock().unwrap();
        let stderr = p.stderr.lock().unwrap();
        row.insert("stdout".into(), normalize_lf(&stdout).into());
        row.insert("stderr".into(), normalize_lf(&stderr).into());
        if input.clear {
            // Zeroed only after the row is built (Go: build map from current
            // content, then reset).
        }
        row
    };
    if input.clear {
        p.stdout.lock().unwrap().clear();
        p.stderr.lock().unwrap().clear();
    }
    super::must_json(&value)
}

/// send_input (main.go ~1455).
pub async fn run_send(ctx: &Ctx, input: SendInput) -> String {
    let p = match registry().lock().unwrap().get(&input.process_id) {
        Some(p) => p.clone(),
        None => return format!("process_not_found: {}", input.process_id),
    };
    if p.done() {
        return format!("process_not_running: {}", input.process_id);
    }
    // Phase 3 gate: sidecar check using the spawned process's original
    // command and cwd (frozen: send_input is always sidecar-checked).
    // Session-key note: the map keys on the full (tool, command, cwd)
    // triple, so an `allow_session` granted to a `start_process` ask does
    // NOT auto-allow `send_input` (different tool = different key); only a
    // session decision made for send_input's own ask does.
    if let GateResult::Blocked(text) =
        crate::safety::gate_shell(ctx, "send_input", &p.command, &p.cwd).await
    {
        return text;
    }
    let mut data = input.input;
    if input.newline {
        data.push('\n');
    }
    use tokio::io::AsyncWriteExt;
    // The stdin slot is a std::Mutex; holding its guard across an await
    // would make this future !Send. Take the handle out for the write and
    // put it back (Go keeps the same WriteCloser across calls).
    let taken = p.stdin.lock().unwrap().take();
    let Some(mut stdin) = taken else {
        return format!("process_not_running: {}", input.process_id);
    };
    let res = async {
        stdin.write_all(data.as_bytes()).await?;
        stdin.flush().await
    };
    let out = res.await;
    *p.stdin.lock().unwrap() = Some(stdin);
    match out {
        Err(e) => format!("send_input_error: {e}"),
        Ok(()) => format!("sent {} bytes to process {}", data.len(), input.process_id),
    }
}

/// list_processes (main.go ~1473). Go iterates a Go map (random order);
/// this port keeps registration order (a BTreeMap) — recorded divergence.
pub async fn run_list() -> String {
    let reg = registry().lock().unwrap();
    // Go's list rows add started_at (process_output's map does not).
    let rows: Vec<Map<String, serde_json::Value>> = reg
        .values()
        .map(|p| {
            let mut row = proc_row(p);
            row.insert("started_at".into(), p.started_at.clone().into());
            row
        })
        .collect();
    super::must_json(&rows)
}

/// kill_process (main.go ~1485). Unix uses signal_unix.go's whitelist
/// (SIGTERM default). Windows mirrors Go: any requested signal is ignored and
/// the process is terminated.
pub async fn run_kill(input: KillInput) -> String {
    let p = match registry().lock().unwrap().get(&input.process_id) {
        Some(p) => p.clone(),
        None => return format!("process_not_found: {}", input.process_id),
    };
    if p.done() {
        return format!("process_already_done: {}", input.process_id);
    }
    let sig = input
        .signal
        .clone()
        .unwrap_or_else(|| "SIGTERM".to_string());
    if cfg!(windows) {
        // Windows branch: Process.Kill(), signal name reported but ignored.
        let mut child = p.child.lock().unwrap();
        if let Some(c) = child.as_mut() {
            c.start_kill().ok();
        }
        return format!(
            "killed process {} (windows: signal {} is ignored, process terminated)",
            input.process_id, sig
        );
    }
    let sig_num: i32 = match sig.as_str() {
        "SIGTERM" => 15,
        "SIGKILL" => 9,
        "SIGINT" => 2,
        _ => return "kill_error: invalid signal".to_string(),
    };
    #[cfg_attr(windows, allow(unused_variables))]
    let pid = match p.child.lock().unwrap().as_mut() {
        Some(c) => match c.id() {
            Some(pid) => pid as i32,
            None => return format!("kill_error: process already reaped: {}", input.process_id),
        },
        None => return format!("kill_error: process already reaped: {}", input.process_id),
    };
    #[cfg(unix)]
    {
        // libc kill (nix is not in the dependency allow-list; equivalent
        // semantic, same EINVAL → invalid-signal wording comes from the
        // whitelist above).
        let target = unsafe { libc::kill(pid, sig_num) };
        if target != 0 {
            let e = std::io::Error::last_os_error();
            return format!("kill_error: {}", super::errno_display(&e));
        }
        format!("sent {sig} to process {}", input.process_id)
    }
    #[cfg(not(unix))]
    {
        let _ = sig_num;
        unreachable!("windows branch returns above")
    }
}

#[cfg(test)]
mod shaping_tests {
    use super::*;

    /// Go's list row adds started_at even though process_output's map does
    /// not — table over both JSON shapes.
    #[tokio::test]
    async fn list_rows_carry_started_at_output_rows_do_not() {
        let text = run_start(
            &Ctx::plain(std::env::temp_dir().to_string_lossy()),
            StartInput {
                command: "sleep 0.15".into(),
                cwd: None,
                interactive: None,
            },
        )
        .await;
        assert!(text.contains("\"process_id\""), "start json: {text}");
        let start: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let id = start["process_id"].as_str().unwrap_or_default().to_string();
        // list row
        let rows = super::run_list().await;
        let list: serde_json::Value = serde_json::from_str(&rows).unwrap_or_default();
        let row = list
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|r| r["process_id"].as_str() == Some(id.as_str()))
                    .cloned()
            })
            .expect("row present");
        assert!(row.get("started_at").is_some(), "list row: {row}");
        // process_output row
        let out = run_output(ProcessInput {
            process_id: id,
            clear: false,
        })
        .await;
        let ov: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        assert!(ov.get("started_at").is_none(), "output row: {ov}");
        assert!(ov.get("stdout").is_some(), "output row: {ov}");
    }
}

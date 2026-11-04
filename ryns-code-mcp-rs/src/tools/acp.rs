//! ACP OpenCode client (main.go ~466-786): spawns `opencode acp`, drives one
//! prompt over newline-delimited JSON-RPC 2.0 (initialize → session/new →
//! session/prompt), auto-approves permission requests, collects agent text
//! chunks, and returns the final response with a stop_reason/tool_calls
//! trailer. Same shape as Go's acpClient with no extra dependencies.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::{Ctx, default_string, safe_path};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AcpOpenCodeInput {
    pub prompt: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "timeout_seconds")]
    pub timeout_seconds: i64,
}

/// Shared state accumulated from session/update notifications.
#[derive(Default)]
struct AcpState {
    text: String,
    tool_calls: i64,
}

type PendingTx = oneshot::Sender<Result<Value, String>>;

/// acp_opencode (main.go ~2792).
pub async fn acp_opencode(ctx: &Ctx, input: AcpOpenCodeInput) -> String {
    if input.prompt.trim().is_empty() {
        return "acp_opencode failed: prompt tidak boleh kosong".to_string();
    }
    let cwd = match safe_path(ctx, &default_string(input.path.as_deref(), ".")) {
        Ok(c) => c,
        Err(e) => return format!("acp_opencode failed: {e}"),
    };
    let mut timeout_secs = input.timeout_seconds;
    if timeout_secs <= 0 {
        timeout_secs = 300;
    }
    if timeout_secs > 1800 {
        timeout_secs = 1800;
    }
    match run_acp_agent(
        &cwd,
        &input.prompt,
        std::time::Duration::from_secs(timeout_secs as u64),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => format!("acp_opencode failed: {e}"),
    }
}

async fn run_acp_agent(
    cwd: &str,
    prompt: &str,
    timeout: std::time::Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut cmd = tokio::process::Command::new("opencode");
    cmd.args(["acp"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.env_clear();
    for (k, v) in crate::auth::child_env() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("gagal start \"opencode\": {e}"))?;
    let mut stdin = child.stdin.take().ok_or("stdin pipe missing")?;
    let stdout = child.stdout.take().ok_or("stdout pipe missing")?;

    let pending: Arc<StdMutex<HashMap<i64, PendingTx>>> = Arc::new(StdMutex::new(HashMap::new()));
    let state: Arc<StdMutex<AcpState>> = Arc::new(StdMutex::new(AcpState::default()));
    let stderr_shared: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));

    // Single outbound writer: reader replies and call requests both enqueue
    // newline-terminated JSON-RPC lines (Go writeMessage holds a mutex).
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() {
                // Go's writeMessage raises on write errors; the *reply* path
                // ignores failures. Exact error lives in the call's send.
                break;
            }
        }
    });
    let out_for_replies = out_tx.clone();

    // Reader loop (Go readLoop): dispatch replies, agent requests, and
    // notifications; EOF fails every pending call.
    {
        let pending = pending.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        handle_line(trimmed, &pending, &state, &out_for_replies).await;
                    }
                    Ok(None) | Err(_) => {
                        let mut map = pending.lock().unwrap();
                        for (_, tx) in map.drain() {
                            let _ = tx.send(Err("koneksi ACP tertutup: EOF".to_string()));
                        }
                        return;
                    }
                }
            }
        });
    }

    // stderr drain for the error suffixes (Go stderrBuf).
    if let Some(stderr) = child.stderr.take() {
        let stderr_shared = stderr_shared.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut b = stderr_shared.lock().unwrap();
                if !b.is_empty() {
                    b.push('\n');
                }
                b.push_str(&line);
            }
        });
    }

    let flow = async {
        let next_id = AtomicI64::new(0);
        let client = Client {
            out: out_tx.clone(),
            pending: pending.clone(),
            next_id: &next_id,
        };

        let init = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            },
            "clientInfo": {
                "name": "ryns-code-mcp",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        client
            .call("initialize", Some(init), deadline)
            .await
            .map_err(|e| {
                format!(
                    "initialize gagal: {e} (stderr: {})",
                    stderr_now(&stderr_shared)
                )
            })?;

        let sess = client
            .call(
                "session/new",
                Some(json!({"cwd": cwd, "mcpServers": []})),
                deadline,
            )
            .await
            .map_err(|e| {
                format!(
                    "session/new gagal: {e} (stderr: {})",
                    stderr_now(&stderr_shared)
                )
            })?;
        let session_id = sess
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            return Err(format!("session/new response tidak ada sessionId: {sess}"));
        }

        let prompt_res = client
            .call(
                "session/prompt",
                Some(json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": prompt}],
                })),
                deadline,
            )
            .await
            .map_err(|e| {
                format!(
                    "session/prompt gagal: {e} (stderr: {})",
                    stderr_now(&stderr_shared)
                )
            })?;
        let stop_reason = prompt_res
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(stop_reason)
    };

    let stop_reason = match tokio::time::timeout(timeout, flow).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let _ = child.kill().await;
            let _ = writer_task.await;
            return Err(e);
        }
        // Go's ctx deadline surfaces as a call-level error; name the outer
        // budget the same way.
        Err(_) => {
            let _ = child.kill().await;
            let _ = writer_task.await;
            return Err("context deadline exceeded".to_string());
        }
    };

    // Go's deferred cleanup: close stdin, kill, wait (errors ignored).
    let _ = out_tx.send(String::new()).await; // wake the writer for exit path
    std::mem::drop(out_tx);
    let _ = child.kill().await;
    let _ = writer_task.await;

    // Collected text (Go: TrimSpace + stripACPNoiseTags) + trailer.
    let text = state.lock().unwrap().text.clone();
    let tool_calls = state.lock().unwrap().tool_calls;
    let text = strip_acp_noise_tags(text.trim());
    let mut sb = String::new();
    if text.is_empty() {
        sb.push_str("(agent tidak mengembalikan teks respons)\n");
    } else {
        sb.push_str(&text);
        sb.push('\n');
    }
    sb.push_str(&format!(
        "\n[stop_reason: {stop_reason}, tool_calls: {tool_calls}]"
    ));
    Ok(sb)
}

fn stderr_now(stderr_shared: &Arc<StdMutex<String>>) -> String {
    stderr_shared.lock().unwrap().trim().to_string()
}

async fn emit_line(out: &mpsc::Sender<String>, line: String) {
    let _ = out.send(line).await;
}

struct Client<'a> {
    out: mpsc::Sender<String>,
    pending: Arc<StdMutex<HashMap<i64, PendingTx>>>,
    next_id: &'a AtomicI64,
}

/// Mirrors acpClient.call: register oneshot, write request, await reply with
/// the caller's remaining time budget (Go couples calls to ctx deadlines).
impl Client<'_> {
    async fn call(
        &self,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let mut msg = serde_json::Map::new();
        msg.insert("jsonrpc".into(), json!("2.0"));
        msg.insert("id".into(), json!(id));
        msg.insert("method".into(), json!(method));
        if let Some(p) = params {
            msg.insert("params".into(), p);
        }
        let line =
            serde_json::to_string(&Value::Object(msg)).map_err(|e| format!("json marshal: {e}"))?;
        emit_line(&self.out, line).await; // write errors ignored (Go's conn)

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "context deadline exceeded".to_string())?;
        match tokio::time::timeout(remaining, rx).await {
            Err(_) => Err("context deadline exceeded".to_string()),
            Ok(Err(_)) => Err("koneksi ACP tertutup: reader dropped".to_string()),
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(e),
        }
    }
}

/// Mirrors handleLine (main.go ~560).
async fn handle_line(
    line: &str,
    pending: &Arc<StdMutex<HashMap<i64, PendingTx>>>,
    state: &Arc<StdMutex<AcpState>>,
    out: &mpsc::Sender<String>,
) {
    let Ok(raw) = serde_json::from_str::<Value>(line) else {
        return; // not JSON (stray log output); ignore like Go
    };
    let id = raw.get("id").and_then(Value::as_i64);
    let method = raw.get("method").and_then(Value::as_str).unwrap_or("");
    match (id, method.is_empty()) {
        (Some(id), true) => {
            // Reply to one of OUR calls.
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                if let Some(err) = raw.get("error") {
                    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let _ = tx.send(Err(format!("{msg} (code {code})")));
                } else {
                    let _ = tx.send(Ok(raw.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
        }
        (Some(id), false) => {
            handle_agent_request(id, method, raw.get("params"), out).await;
        }
        (None, false) => handle_notification(method, raw.get("params"), state).await,
        (None, true) => {}
    }
}

/// Mirrors handleAgentRequest (main.go ~599): auto-approve
/// session/request_permission (allow_once first, then allow_always), reject
/// other methods with -32601 so the agent doesn't hang.
async fn handle_agent_request(
    id: i64,
    method: &str,
    params: Option<&Value>,
    out: &mpsc::Sender<String>,
) {
    let reply = if method != "session/request_permission" {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method not supported by this headless ACP client: {method}"),
            },
        })
    } else {
        let mut chosen = String::new();
        for want_kind in ["allow_once", "allow_always"] {
            if let Some(opts) = params
                .and_then(|p| p.get("options"))
                .and_then(Value::as_array)
            {
                for opt in opts {
                    if opt.get("kind").and_then(Value::as_str) == Some(want_kind) {
                        chosen = opt
                            .get("optionId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        break;
                    }
                }
            }
            if !chosen.is_empty() {
                break;
            }
        }
        let outcome = if chosen.is_empty() {
            json!({"outcome": "cancelled"})
        } else {
            json!({"outcome": {"outcome": "selected", "optionId": chosen}})
        };
        json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}})
    };
    if let Ok(line) = serde_json::to_string(&reply) {
        emit_line(out, line).await;
    }
}

/// Mirrors handleNotification (main.go ~640).
async fn handle_notification(
    method: &str,
    params: Option<&Value>,
    state: &Arc<StdMutex<AcpState>>,
) {
    if method != "session/update" {
        return;
    }
    let Some(update) = params.and_then(|p| p.get("update")) else {
        return;
    };
    match update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("")
    {
        "agent_message_chunk" => {
            if let Some(content) = update.get("content")
                && content.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = content.get("text").and_then(Value::as_str)
            {
                state.lock().unwrap().text.push_str(text);
            }
        }
        "tool_call" => {
            state.lock().unwrap().tool_calls += 1;
        }
        _ => {}
    }
}

/// stripACPNoiseTags (main.go ~784): the two hard-coded agent noise tag
/// spans are removed, then the result is trimmed. Data markers only.
pub fn strip_acp_noise_tags(s: &str) -> String {
    let mut out = s.to_string();
    const NOISE_TAGS: [&str; 2] = ["<dcp-system-reminder>", "<dcp-message-id>"];
    for open in NOISE_TAGS.iter() {
        let close = if *open == NOISE_TAGS[0] {
            "</dcp-system-reminder>"
        } else {
            "</dcp-message-id>"
        };
        while let Some(start) = out.find(open) {
            if let Some(end_rel) = out[start..].find(close) {
                out.replace_range(start..start + end_rel + close.len(), "");
            } else {
                break;
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    /// The data-level housekeeping-tag strip (literal-scan, both spans).
    #[test]
    fn strips_noise_tags_and_trims() {
        let t = "pre<fcp></fcp>";
        let s = "a<b>dcp</b>";
        let inner = concat!(
            "x<dcp-system-reminder>",
            "housekeeping",
            "</dcp-system-reminder>y<dcp-message-id>",
            "42",
            "</dcp-message-id>z",
        );
        assert_eq!(strip_acp_noise_tags(inner), "xyz");
        // trim end
        assert_eq!(format!("{:?}", strip_acp_noise_tags("hi\n \n")), "\"hi\"");
        // plain text without tags passes through
        assert_eq!(strip_acp_noise_tags(s), s);
        assert_eq!(strip_acp_noise_tags(t), t);
    }
}

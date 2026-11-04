//! Safety / policy engine — Phase 3 per docs/PLAN_RUST_PORT.md §3 + §6.
//!
//! Components:
//! - `Engine`: config from `RYNS_SAFETY_MODE` / `RYNS_ASK_TIMEOUT_MS` /
//!   `RYNS_NODE_BIN` / `RYNS_SIDECAR_SCRIPT`; persistent lazy sidecar child
//!   (newline-JSON protocol), ASK queue for deny→human review, allow_session
//!   map (in-memory, session-scoped), append-only audit JSONL.
//! - Built-in secret-path ruleset (`secret_rule`): own minimal rules on file
//!   tools — NOT upstream cc-safety-net parity (plan §1).
//!
//! Frozen decision mapping (plan §3/§6):
//! - engine `allow` → run;
//! - engine/user-rule `deny` (enforce) → block + pending ASK entry; the human
//!   decision (allow/deny/allow_session via `/api/safety/decide/{id}`) or the
//!   `RYNS_ASK_TIMEOUT_MS` expiry resolves it; timeout ⇒ deny reason
//!   "timeout", late decisions ⇒ NotFound;
//! - sidecar error/absent in enforce ⇒ deny fail-closed (audit
//!   `sidecar_error`, NO ask); in observe ⇒ run + audit `sidecar_error`;
//! - `off` mode ⇒ zero checks, still audited (decision "allow");
//! - `allow_session` map applies BEFORE any re-check on identical
//!   (tool, command, cwd) triples; session-scoped memory only;
//! - sidecar local THROW maps in the SCRIPT to `kind:"deny"` with
//!   ruleId `custom.sidecar-error` (plan §3, fail closed).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::io::AsyncBufReadExt;
use tokio::sync::{broadcast, oneshot};

use crate::auth::child_env;
use crate::tools::rfc3339_nano;

/// Per-call sidecar budget (frozen: 5 s). Transport failure/timeout after
/// that maps per-mode (enforce: deny, observe: allow+audit).
pub const SIDECAR_CALL_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Observe,
    Off,
}

impl Mode {
    pub fn parse(raw: &str) -> Result<Mode, String> {
        match raw {
            "enforce" => Ok(Mode::Enforce),
            "observe" => Ok(Mode::Observe),
            "off" => Ok(Mode::Off),
            other => Err(format!(
                "RYNS_SAFETY_MODE must be one of enforce, observe, off (got {other:?})"
            )),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Observe => "observe",
            Mode::Off => "off",
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub mode: Mode,
    pub ask_timeout_ms: u64,
    pub node_bin: String,
    pub sidecar_script: String,
}

pub fn config_from_env() -> Result<Config, String> {
    let mode = match std::env::var("RYNS_SAFETY_MODE") {
        Ok(raw) if !raw.is_empty() => Mode::parse(&raw)?,
        _ => Mode::Enforce,
    };
    let ask_timeout_ms = match std::env::var("RYNS_ASK_TIMEOUT_MS") {
        Ok(raw) if !raw.is_empty() => raw.parse::<u64>().map_err(|_| {
            format!("RYNS_ASK_TIMEOUT_MS must be an integer in milliseconds (got {raw:?})")
        })?,
        _ => 120_000,
    };
    let node_bin = match std::env::var("RYNS_NODE_BIN") {
        Ok(raw) if !raw.is_empty() => raw,
        _ => "node".to_string(),
    };
    let sidecar_script = match std::env::var("RYNS_SIDECAR_SCRIPT") {
        Ok(raw) if !raw.is_empty() => raw,
        // Relative default: anchored by the launcher CWD (the `run` script /
        // MCP client startup directory); documented, same relative shape the
        // plan describes (`<repo>/scripts/safety-sidecar/index.mjs`).
        _ => "scripts/safety-sidecar/index.mjs".to_string(),
    };
    Ok(Config {
        mode,
        ask_timeout_ms,
        node_bin,
        sidecar_script,
    })
}

// ---- gate results & ask machinery -------------------------------------------

#[derive(Debug)]
pub enum GateResult {
    /// Checked: run the tool.
    Run,
    /// Checked: do NOT run; the text is the tool's final textResult body
    /// (safety_error convention).
    Blocked(String),
}

/// Human decision for a pending ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionChoice {
    Allow,
    Deny,
    AllowSession,
}

impl DecisionChoice {
    pub fn parse(raw: &str) -> Option<DecisionChoice> {
        match raw {
            "allow" => Some(DecisionChoice::Allow),
            "deny" => Some(DecisionChoice::Deny),
            "allow_session" => Some(DecisionChoice::AllowSession),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            DecisionChoice::Allow => "allow",
            DecisionChoice::Deny => "deny",
            DecisionChoice::AllowSession => "allow_session",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AskView {
    pub id: String,
    pub tool: String,
    pub command: String,
    pub cwd: String,
    pub reason: String,
    pub rule_id: String,
    pub created_at: String,
    pub expires_at: String,
}

/// SSE payload: pending (new ask) / resolved (decision or timeout).
#[derive(Debug, Clone)]
pub enum Event {
    Pending(AskView),
    Resolved { id: String, decision: &'static str },
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecideError {
    NotFound,
    AlreadyDecided,
}

enum AskState {
    Waiting(oneshot::Sender<AskOutcome>),
    Decided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskOutcome {
    Allowed,
    AllowedSession,
    Denied,
}

struct AskRecord {
    view: AskView,
    expires: SystemTime,
    state: AskState,
}

/// A sidecar verdict mapped through the frozen protocol semantics.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny {
        reason: String,
        rule_id: String,
    },
    /// Sidecar transport/protocol failure (spawn, death, timeout, malformed).
    SidecarError(String),
}

// ---- built-in secret-path rules (own ruleset; not upstream parity) ----------

/// Identifies the minimal secret-path ruleset for a file-tool path argument.
/// Matching is path-component based so relative, `~/…`, and absolute forms
/// behave the same; comparisons are ASCII-case-insensitive so Windows-style
/// paths are covered too. Returns (ruleId, reason) used by the audit and the
/// ASK view.
pub fn secret_rule(path: &str) -> Option<(&'static str, &'static str)> {
    let lower = path.to_ascii_lowercase();
    let comps: Vec<&str> = lower.split(['/', '\\']).filter(|c| !c.is_empty()).collect();
    let file = comps.last().copied().unwrap_or("");
    if file.starts_with(".env") {
        return Some(("secret.env", "path matches the .env* secret-file rule"));
    }
    // Directory membership beats file-name prefixes: a key under an .ssh
    // directory reports the .ssh rule.
    if comps.contains(&".ssh") {
        return Some(("secret.ssh", "path lives under an .ssh directory"));
    }
    if comps.contains(&".aws") {
        return Some(("secret.aws", "path lives under an .aws directory"));
    }
    if file.starts_with("id_rsa") || file.starts_with("id_ed25519") {
        return Some((
            "secret.ssh-key",
            "path matches the id_rsa/id_ed25519 private-key rule",
        ));
    }
    if comps
        .windows(2)
        .any(|w| w[0] == "library" && w[1] == "keychains")
    {
        return Some((
            "secret.keychain",
            "path lives under a credential store (Library/Keychains)",
        ));
    }
    None
}

// ---- sidecar client ---------------------------------------------------------

#[derive(Debug)]
enum ClientErr {
    Spawn(String),
    Closed,
    Timeout,
}

impl ClientErr {
    fn text(&self) -> String {
        match self {
            ClientErr::Spawn(e) => format!("sidecar spawn failed: {e}"),
            ClientErr::Closed => "sidecar closed the stream".to_string(),
            ClientErr::Timeout => "sidecar call budget exceeded (5s)".to_string(),
        }
    }
}

type Waiters =
    Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, ClientErr>>>>>;

#[derive(Debug)]
struct SidecarHandle {
    node_bin: String,
    script: String,
    cwd: String,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    waiters: Waiters,
    next_id: AtomicU64,
}

impl SidecarHandle {
    fn new(node_bin: String, script: String, cwd: String) -> Self {
        Self {
            node_bin,
            script,
            cwd,
            child: tokio::sync::Mutex::new(None),
            stdin: tokio::sync::Mutex::new(None),
            waiters: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawns the sidecar and arms the stdout reader task. Only called while
    /// the child lock is held.
    async fn spawn_locked(
        &self,
        child: &mut tokio::sync::MutexGuard<'_, Option<tokio::process::Child>>,
    ) -> Result<tokio::process::ChildStdin, String> {
        let mut cmd = tokio::process::Command::new(&self.node_bin);
        cmd.arg(&self.script)
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            // Persistent; dies with the server process (kill on handle drop).
            .kill_on_drop(true);
        cmd.env_clear();
        for (k, v) in child_env() {
            cmd.env(k, v);
        }
        let mut spawned = cmd
            .spawn()
            .map_err(|e| format!("{} {}: {e}", self.node_bin, self.script))?;
        let stdout = spawned
            .stdout
            .take()
            .ok_or_else(|| "sidecar stdout unavailable".to_string())?;
        let stdin = spawned
            .stdin
            .take()
            .ok_or_else(|| "sidecar stdin unavailable".to_string())?;
        **child = Some(spawned);
        let waiters = self.waiters.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        if let Some(v) = serde_json::from_str::<serde_json::Value>(line.trim())
                            .ok()
                            .filter(|v| v.get("id").and_then(serde_json::Value::as_u64).is_some())
                        {
                            let id = v["id"].as_u64().unwrap();
                            let slot = waiters.lock().await.remove(&id);
                            if let Some(tx) = slot {
                                let _ = tx.send(Ok(v));
                            }
                        }
                    }
                }
            }
            // EOF / reader error: fail every pending waiter (fail-closed).
            let mut map = waiters.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(ClientErr::Closed));
            }
        });
        Ok(stdin)
    }

    async fn ensure_spawned(&self) -> Result<(), String> {
        let mut child = self.child.lock().await;
        let alive = match child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        };
        if alive {
            return Ok(());
        }
        let stdin = self.spawn_locked(&mut child).await?;
        *self.stdin.lock().await = Some(stdin);
        Ok(())
    }

    /// Safety net: force a fresh spawn on the next attempt.
    async fn mark_broken(&self) {
        *self.child.lock().await = None;
        *self.stdin.lock().await = None;
    }

    /// One checkCommand round-trip: fresh spawn + one respawn budget, 5 s
    /// per call. Every transport/protocol failure maps to a `ClientErr`.
    async fn call(&self, command: &str, cwd: &str) -> Result<serde_json::Value, ClientErr> {
        let mut last_err: Option<ClientErr> = None;
        for attempt in 0..2 {
            if attempt > 0 {
                self.mark_broken().await;
            }
            if let Err(e) = self.ensure_spawned().await {
                last_err = Some(ClientErr::Spawn(e));
                continue;
            }
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let req = serde_json::json!({
                "v": 1,
                "id": id,
                "op": "checkCommand",
                "command": command,
                "cwd": cwd,
            });
            let mut line = serde_json::to_string(&req).unwrap_or_default();
            line.push('\n');
            let (tx, rx) = oneshot::channel();
            self.waiters.lock().await.insert(id, tx);
            let mut send_ok = false;
            {
                let mut stdin_guard = self.stdin.lock().await;
                if let Some(w) = stdin_guard.as_mut() {
                    use tokio::io::AsyncWriteExt;
                    send_ok = w.write_all(line.as_bytes()).await.is_ok() && w.flush().await.is_ok();
                }
                if !send_ok {
                    // Broken pipe / no stdin: drop the waiter and retry once
                    // (this counts as the respawn).
                    self.waiters.lock().await.remove(&id);
                    self.mark_broken().await;
                    last_err = Some(ClientErr::Closed);
                    continue;
                }
            }
            match tokio::time::timeout(SIDECAR_CALL_BUDGET, rx).await {
                Ok(Ok(Ok(v))) => return Ok(v),
                Ok(Ok(Err(e))) => return Err(e),
                Ok(Err(_recv)) => {
                    // Sender dropped without a reply — fail this call.
                    self.waiters.lock().await.remove(&id);
                    return Err(ClientErr::Closed);
                }
                Err(_elapsed) => {
                    self.waiters.lock().await.remove(&id);
                    return Err(ClientErr::Timeout);
                }
            }
        }
        Err(last_err.unwrap_or(ClientErr::Spawn("sidecar could not be started".to_string())))
    }
}

// ---- protocol parsing -------------------------------------------------------

/// Maps a raw sidecar reply onto the frozen protocol semantics. Lenient:
/// accepts `{"kind":"error","message":…}` AND the `{"error":…}` shorthand;
/// anything unparseable becomes a sidecar error (never a silent allow).
pub fn parse_reply(v: &serde_json::Value) -> Verdict {
    let Some(obj) = v.as_object() else {
        return Verdict::SidecarError("malformed reply (non-object)".to_string());
    };
    match obj.get("kind").and_then(serde_json::Value::as_str) {
        Some("allow") => Verdict::Allow,
        Some("deny") => Verdict::Deny {
            reason: str_field(v, "reason").unwrap_or_else(|| "denied by policy".into()),
            rule_id: str_field(v, "ruleId")
                .or_else(|| str_field(v, "rule_id"))
                .unwrap_or_else(|| "custom.rule".into()),
        },
        Some("error") => {
            Verdict::SidecarError(str_field(v, "message").unwrap_or_else(|| "sidecar error".into()))
        }
        _ => match str_field(v, "error") {
            Some(message) => Verdict::SidecarError(message),
            None => Verdict::SidecarError("malformed reply (missing kind)".to_string()),
        },
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// JSON string escaping (quote-delimited) for audit lines, written without
/// serde_json to guarantee the frozen field ORDER
/// (ts, mode, tool, command, cwd, decision, ruleId, reason).
pub fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- engine -----------------------------------------------------------------

pub struct Engine {
    mode: Mode,
    workspace: String,
    ask_timeout: Duration,
    sidecar: SidecarHandle,
    pending: Mutex<HashMap<String, AskRecord>>,
    session_ok: Mutex<HashSet<(String, String, String)>>,
    events: broadcast::Sender<Event>,
    audit_lock: Mutex<()>,
    seq: AtomicU64,
    sweeper_started: AtomicBool,
}

impl Engine {
    pub fn new(config: &Config, workspace: &str) -> Engine {
        // Best effort: the audit dir may not exist yet (workspace-relative).
        let _ = std::fs::create_dir_all(format!("{workspace}/.state"));
        let (events, _drained) = broadcast::channel(256);
        Engine {
            mode: config.mode,
            workspace: workspace.to_string(),
            ask_timeout: Duration::from_millis(config.ask_timeout_ms),
            sidecar: SidecarHandle::new(
                config.node_bin.clone(),
                config.sidecar_script.clone(),
                workspace.to_string(),
            ),
            pending: Mutex::new(HashMap::new()),
            session_ok: Mutex::new(HashSet::new()),
            events,
            audit_lock: Mutex::new(()),
            seq: AtomicU64::new(1),
            sweeper_started: AtomicBool::new(false),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn audit_path(&self) -> String {
        format!("{}/.state/safety-audit.jsonl", self.workspace)
    }

    fn next_id(&self) -> String {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("ask-{nanos:016x}-{seq}")
    }

    /// Appends one audit line. Every gated call writes at least one line;
    /// the ask lifecycle adds ask_allowed/ask_denied/ask_timeout. Appends are
    /// O_APPEND (atomic line writes) under one mutex for ordering.
    #[allow(clippy::too_many_arguments)]
    pub fn audit(
        &self,
        tool: &str,
        command: &str,
        cwd: &str,
        decision: &str,
        rule_id: &str,
        reason: &str,
    ) {
        let line = format!(
            "{{\"ts\":{},\"mode\":\"{}\",\"tool\":{},\"command\":{},\"cwd\":{},\"decision\":\"{}\",\"ruleId\":{},\"reason\":{}}}\n",
            json_quote(&rfc3339_nano(SystemTime::now())),
            self.mode.label(),
            json_quote(tool),
            json_quote(command),
            json_quote(cwd),
            decision,
            json_quote(rule_id),
            json_quote(reason),
        );
        let _guard = self.audit_lock.lock().unwrap();
        let open_file = || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(self.audit_path())
            }
            #[cfg(not(unix))]
            {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.audit_path())
            }
        };
        let Ok(mut f) = open_file() else {
            // Audit is append-best-effort; never fail a tool call over it.
            return;
        };
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }

    /// Drops expired entries so late decisions map to NotFound (frozen).
    fn prune(&self) {
        let now = SystemTime::now();
        self.pending.lock().unwrap().retain(|_, r| r.expires > now);
    }

    /// Starts (once) a periodic sweeper that expires timed-out pendings even
    /// when the requesting tool future is gone (cancelled/disconnected):
    /// expiry then still produces the `ask_timeout` audit line and the
    /// `resolved`/timeout SSE frame. STOPGAP, documented: a still-alive gate
    /// future racing the sweeper surfaces the expiry as a closed decision
    /// channel (fail-closed block + `ask_denied` line) — the tool still never
    /// runs; cleanup (deduped expiry ownership) is a later-phase item.
    pub fn start_sweeper(self: &Arc<Self>) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick is immediate — skip it
            loop {
                tick.tick().await;
                engine.expire_pending_now(SystemTime::now());
            }
        });
    }

    /// Expires every pending whose deadline passed: removes it (late API
    /// decisions → NotFound), audits `ask_timeout`, broadcasts resolved.
    fn expire_pending_now(&self, now: SystemTime) {
        let expired: Vec<String> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let view;
            {
                let mut map = self.pending.lock().unwrap();
                let Some(mut rec) = map.remove(&id) else {
                    continue;
                };
                rec.state = AskState::Decided; // waiting sender drops: gate (if alive) sees a closed channel
                view = rec.view;
            }
            self.audit(
                &view.tool,
                &view.command,
                &view.cwd,
                "ask_timeout",
                &view.rule_id,
                "timeout",
            );
            let _ = self.events.send(Event::Resolved {
                id,
                decision: "timeout",
            });
        }
    }

    pub fn pending_snapshot(&self) -> Vec<AskView> {
        self.prune();
        let map = self.pending.lock().unwrap();
        let mut out: Vec<AskView> = map
            .values()
            .filter(|r| matches!(r.state, AskState::Waiting(_)))
            .map(|r| r.view.clone())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        out
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Resolves a pending ask (HTTP decide endpoint). Late duplicate decisions
    /// map to AlreadyDecided until the entry expires; after expiry NotFound.
    pub fn decide(&self, id: &str, decision: DecisionChoice) -> Result<AskView, DecideError> {
        self.prune();
        let label = decision.label();
        let view = {
            let mut map = self.pending.lock().unwrap();
            let Some(record) = map.get_mut(id) else {
                return Err(DecideError::NotFound);
            };
            if matches!(record.state, AskState::Decided) {
                return Err(DecideError::AlreadyDecided);
            }
            let mut taken = match std::mem::replace(&mut record.state, AskState::Decided) {
                AskState::Waiting(tx) => Some(tx),
                AskState::Decided => None,
            };
            if let Some(tx) = taken.take() {
                let _ = tx.send(match decision {
                    DecisionChoice::Allow => AskOutcome::Allowed,
                    DecisionChoice::Deny => AskOutcome::Denied,
                    DecisionChoice::AllowSession => AskOutcome::AllowedSession,
                });
            }
            if decision == DecisionChoice::AllowSession {
                let key = (
                    record.view.tool.clone(),
                    record.view.command.clone(),
                    record.view.cwd.clone(),
                );
                self.session_ok.lock().unwrap().insert(key);
            }
            record.view.clone()
        };
        let _ = self.events.send(Event::Resolved {
            id: view.id.clone(),
            decision: label,
        });
        Ok(view)
    }

    /// Guard for tools with policy `access: "ask"` (§A2): MANDATORY ASK —
    /// always routed to the pending queue regardless of any engine verdict
    /// (session-allow map honored first; timeout ⇒ deny "timeout").
    pub async fn forced_ask(self: &Arc<Self>, tool: &str, command: &str, cwd: &str) -> GateResult {
        self.start_sweeper();
        let key = (tool.to_string(), command.to_string(), cwd.to_string());
        if self.session_ok.lock().unwrap().contains(&key) {
            self.audit(tool, command, cwd, "allow", "session.allow", "");
            return GateResult::Run;
        }
        self.begin_ask(
            tool,
            command,
            cwd,
            "ops.ask",
            "mandatory review (access=ask)",
        )
        .await
    }

    /// Main gate for always-sidecar-checked tools.
    pub async fn gate(&self, tool: &str, command: &str, cwd: &str) -> GateResult {
        if self.mode == Mode::Off {
            self.audit(tool, command, cwd, "allow", "", "");
            return GateResult::Run;
        }
        // allow_session applies BEFORE any re-check (frozen).
        let key = (tool.to_string(), command.to_string(), cwd.to_string());
        if self.session_ok.lock().unwrap().contains(&key) {
            self.audit(tool, command, cwd, "allow", "session.allow", "");
            return GateResult::Run;
        }
        let raw = self.sidecar.call(command, cwd).await;
        let verdict = match raw {
            Ok(v) => parse_reply(&v),
            Err(e) => Verdict::SidecarError(e.text()),
        };
        match verdict {
            Verdict::Allow => {
                self.audit(tool, command, cwd, "allow", "", "");
                GateResult::Run
            }
            Verdict::Deny { reason, rule_id } => {
                if self.mode == Mode::Observe {
                    self.audit(tool, command, cwd, "deny", &rule_id, &reason);
                    GateResult::Run
                } else {
                    self.begin_ask(tool, command, cwd, &rule_id, &reason).await
                }
            }
            Verdict::SidecarError(message) => {
                self.audit(
                    tool,
                    command,
                    cwd,
                    "sidecar_error",
                    "sidecar_error",
                    &message,
                );
                match self.mode {
                    Mode::Observe => GateResult::Run,
                    _ => GateResult::Blocked(format!(
                        "safety_error: denied (sidecar_error): {message}"
                    )),
                }
            }
        }
    }

    /// Gate for file tools' path arguments (own secret-path ruleset only —
    /// no sidecar consult; a rule hit feeds the same ASK flow as an engine
    /// deny, frozen). The session map applies first, as with shell tools.
    pub async fn gate_path(&self, tool: &str, path: &str) -> GateResult {
        if self.mode == Mode::Off {
            let cwd = dir_of(path);
            self.audit(tool, path, &cwd, "allow", "", "");
            return GateResult::Run;
        }
        let key = (tool.to_string(), path.to_string(), dir_of(path).to_string());
        if self.session_ok.lock().unwrap().contains(&key) {
            let cwd = dir_of(path);
            self.audit(tool, path, &cwd, "allow", "session.allow", "");
            return GateResult::Run;
        }
        match secret_rule(path) {
            None => {
                let cwd = dir_of(path);
                self.audit(tool, path, &cwd, "allow", "", "");
                GateResult::Run
            }
            Some((rule_id, reason)) => {
                if self.mode == Mode::Observe {
                    let cwd = dir_of(path);
                    self.audit(tool, path, &cwd, "deny", rule_id, reason);
                    GateResult::Run
                } else {
                    self.begin_ask(tool, path, &dir_of(path), rule_id, reason)
                        .await
                }
            }
        }
    }

    /// deny → pending ASK → wait for the decision (timeout ⇒ deny reason
    /// "timeout"; late decision ⇒ the entry is gone, NotFound for the API).
    /// Shared by sidecar denies and built-in rule denies. `command` holds the
    /// shell command for shell tools, the vetted path for path tools; `cwd`
    /// is the working directory for shell tools and the parent dir for path
    /// tools.
    async fn begin_ask(
        &self,
        tool: &str,
        command: &str,
        cwd: &str,
        rule_id: &str,
        reason: &str,
    ) -> GateResult {
        self.prune();
        let id = self.next_id();
        let created = SystemTime::now();
        let expires = created + self.ask_timeout;
        let view = AskView {
            id: id.clone(),
            tool: tool.to_string(),
            command: command.to_string(),
            cwd: cwd.to_string(),
            reason: reason.to_string(),
            rule_id: rule_id.to_string(),
            created_at: rfc3339_nano(created),
            expires_at: rfc3339_nano(expires),
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id.clone(),
            AskRecord {
                view: view.clone(),
                expires,
                state: AskState::Waiting(tx),
            },
        );
        self.audit(tool, command, cwd, "ask", rule_id, reason);
        let _ = self.events.send(Event::Pending(view.clone()));

        match tokio::time::timeout(self.ask_timeout, rx).await {
            Err(_elapsed) => {
                // Removes the entry so late decisions map to NotFound.
                self.pending.lock().unwrap().remove(&id);
                self.audit(tool, command, cwd, "ask_timeout", rule_id, "timeout");
                let _ = self.events.send(Event::Resolved {
                    id,
                    decision: "timeout",
                });
                GateResult::Blocked(format!("safety_error: denied ({rule_id}): timeout"))
            }
            Ok(Ok(AskOutcome::Allowed)) => {
                if let Some(rec) = self.pending.lock().unwrap().get_mut(&id) {
                    rec.state = AskState::Decided;
                }
                self.audit(tool, command, cwd, "ask_allowed", rule_id, reason);
                let _ = self.events.send(Event::Resolved {
                    id,
                    decision: "allow",
                });
                GateResult::Run
            }
            Ok(Ok(AskOutcome::AllowedSession)) => {
                self.pending.lock().unwrap().remove(&id);
                self.session_ok.lock().unwrap().insert((
                    tool.to_string(),
                    command.to_string(),
                    cwd.to_string(),
                ));
                self.audit(
                    tool,
                    command,
                    cwd,
                    "ask_allowed",
                    &format!("{rule_id}+session"),
                    reason,
                );
                let _ = self.events.send(Event::Resolved {
                    id,
                    decision: "allow_session",
                });
                GateResult::Run
            }
            Ok(Ok(AskOutcome::Denied)) => {
                if let Some(rec) = self.pending.lock().unwrap().get_mut(&id) {
                    rec.state = AskState::Decided;
                }
                self.audit(tool, command, cwd, "ask_denied", rule_id, reason);
                let _ = self.events.send(Event::Resolved {
                    id,
                    decision: "deny",
                });
                GateResult::Blocked(format!("safety_error: denied ({rule_id}): {reason}"))
            }
            Ok(Err(_dropped)) => {
                // Decision channel dropped without a verdict — fail closed.
                // (Record kept as Decided until expiry; duplicates → 409.)
                if let Some(rec) = self.pending.lock().unwrap().get_mut(&id) {
                    rec.state = AskState::Decided;
                }
                self.audit(
                    tool,
                    command,
                    cwd,
                    "ask_denied",
                    rule_id,
                    "decision channel closed",
                );
                let _ = self.events.send(Event::Resolved {
                    id,
                    decision: "deny",
                });
                GateResult::Blocked(format!(
                    "safety_error: denied ({rule_id}): decision channel closed"
                ))
            }
        }
    }
}

/// Parent directory of a path (cwd column for path-tool gates/audits).
pub(crate) fn dir_of(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(i) if i > 0 => path[..i].to_string(),
        Some(_) => "/".to_string(),
        None => ".".to_string(),
    }
}

// ---- tool-call entry points ---------------------------------------------------

/// Gate for a shell-side tool (execute_command/start_process/send_input/
/// apply_patch): sidecar check + audit; no safety engine present (tests)
/// means always-run. Modes: off → zero checks + plain `allow` audit (exact
/// pre-§A2 behavior); enforce/observe with per-tool policy safety=false →
/// the whole gate is skipped and the bypass is audited (§A2).
pub async fn gate_shell(
    ctx: &crate::tools::Ctx,
    tool: &str,
    command: &str,
    cwd: &str,
) -> GateResult {
    let Some(engine) = &ctx.safety else {
        return GateResult::Run;
    };
    if engine.mode() == Mode::Off {
        return engine.gate(tool, command, cwd).await;
    }
    // §A2 single-ASK marker: an approved forced ask covers the WHOLE
    // invocation (apply_patch prompts exactly once) — audit + run, no re-ask.
    if let Some(approved) = &ctx.approved_ask
        && approved.as_ref() == tool
    {
        engine.audit(
            tool,
            command,
            cwd,
            "allow",
            "ops.ask+invocation",
            "mandatory-review decision already granted for this call",
        );
        return GateResult::Run;
    }
    engine.start_sweeper();
    if !crate::policy::load(&ctx.workspace).setting(tool).safety {
        engine.audit(
            tool,
            command,
            cwd,
            "allow",
            "policy.safety_off",
            "per-tool safety disabled",
        );
        return GateResult::Run;
    }
    engine.gate(tool, command, cwd).await
}

/// Gate for file tools' path arguments (own secret-path ruleset): one check
/// per path, first deny wins (single pending ask). No engine → always-run.
/// Off keeps pre-§A2 behavior; policy safety=false skips the rules entirely.
pub async fn gate_paths(ctx: &crate::tools::Ctx, tool: &str, paths: &[&str]) -> GateResult {
    let Some(engine) = &ctx.safety else {
        return GateResult::Run;
    };
    if engine.mode() == Mode::Off {
        return engine
            .gate_path(tool, paths.first().copied().unwrap_or("."))
            .await;
    }
    engine.start_sweeper();
    if !crate::policy::load(&ctx.workspace).setting(tool).safety {
        let cwd = dir_of(paths.first().copied().unwrap_or("."));
        engine.audit(
            tool,
            &paths.join(" "),
            &cwd,
            "allow",
            "policy.safety_off",
            "per-tool safety disabled",
        );
        return GateResult::Run;
    }
    for path in paths {
        match engine.gate_path(tool, path).await {
            GateResult::Run => {}
            blocked => return blocked,
        }
    }
    GateResult::Run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn engine_for_test(mode: Mode, timeout_ms: u64) -> Engine {
        let tag = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let ws = std::env::temp_dir().join(format!("ryns-safety-{tag}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        Engine::new(
            &Config {
                mode,
                ask_timeout_ms: timeout_ms,
                node_bin: "node".to_string(),
                // Unreachable script: sidecar_error path; ask-flow tests use
                // gate_path (rules deny → ASK, no sidecar consult) so no
                // node binary is required for these unit tests.
                sidecar_script: "/nonexistent/index.mjs".to_string(),
            },
            &ws.to_string_lossy(),
        )
    }

    // ---- secret rules table -------------------------------------------------

    #[test]
    fn secret_path_rules_table() {
        for (p, rule) in [
            ("/home/u/.env", Some("secret.env")),
            ("/home/u/.env.local", Some("secret.env")),
            ("C:/proj/.envrc", Some("secret.env")),
            ("/home/u/.ssh/id_rsa", Some("secret.ssh")),
            ("/home/u/.aws/config", Some("secret.aws")),
            ("/home/u/.ssh/id_ed25519_sk", Some("secret.ssh")),
            (
                "/Users/u/Library/Keychains/login.keychain-db",
                Some("secret.keychain"),
            ),
            ("id_rsa", Some("secret.ssh-key")),
        ] {
            assert_eq!(secret_rule(p).map(|(r, _)| r), rule, "case {p}");
        }
        // allow sides, including the reviewer edge: ~/.ssh-config must NOT
        // match (only exact ".ssh" directory components; "ssh-config" is a
        // file name that starts with neither ".env" nor "id_rsa").
        for p in [
            "/home/u/.ssh-config",
            "/home/u/notes.md",
            "/etc/hosts",
            "/Users/u/sources/main.rs",
            "/home/u/env",
        ] {
            assert!(secret_rule(p).is_none(), "expected allow for {p}");
        }
    }

    #[test]
    fn env_prefix_rule_is_a_file_component_rule() {
        assert_eq!(secret_rule("/x/.envrc").map(|(r, _)| r), Some("secret.env"));
        // "env" without the leading dot is not a secret file.
        assert!(secret_rule("/x/env").is_none());
    }

    #[test]
    fn secret_rules_case_insensitive() {
        assert!(secret_rule("/USERS/u/Library/KEYCHAINS/a").is_some());
        assert!(secret_rule("/home/u/.SSH/ID_RSA").is_some());
    }

    // ---- protocol parser table ----------------------------------------------

    #[test]
    fn parse_reply_table() {
        assert_eq!(
            parse_reply(&serde_json::json!({"v":1,"id":1,"kind":"allow"})),
            Verdict::Allow
        );
        assert_eq!(
            parse_reply(&serde_json::json!({"kind":"deny","reason":"r2","ruleId":"ru"})),
            Verdict::Deny {
                reason: "r2".into(),
                rule_id: "ru".into()
            }
        );
        assert_eq!(
            parse_reply(&serde_json::json!({"kind":"deny","reason":"r"})),
            Verdict::Deny {
                reason: "r".into(),
                rule_id: "custom.rule".into()
            }
        );
        assert_eq!(
            parse_reply(&serde_json::json!({"kind":"error","message":"boom"})),
            Verdict::SidecarError("boom".into())
        );
        assert_eq!(
            parse_reply(&serde_json::json!({"error":"broken"})),
            Verdict::SidecarError("broken".into())
        );
        assert!(matches!(
            parse_reply(&serde_json::json!("text")),
            Verdict::SidecarError(_)
        ));
        assert!(matches!(
            parse_reply(&serde_json::json!({"kind":"wat"})),
            Verdict::SidecarError(_)
        ));
        assert!(matches!(
            parse_reply(&serde_json::Value::Null),
            Verdict::SidecarError(_)
        ));
    }

    // ---- audit JSONL shape ---------------------------------------------------

    #[test]
    fn audit_line_json_shape() {
        let engine = engine_for_test(Mode::Enforce, 120_000);
        engine.audit("execute_command", "echo hi", "/w", "allow", "", "");
        engine.audit(
            "read_file",
            "/x/.env",
            "/x",
            "ask",
            "secret.env",
            "path with \"quotes\"\nand newline",
        );
        let body = std::fs::read_to_string(engine.audit_path()).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        for key in [
            "ts", "mode", "tool", "command", "cwd", "decision", "ruleId", "reason",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(v["mode"], "enforce");
        assert_eq!(v["tool"], "execute_command");
        assert_eq!(v["decision"], "allow");
        // Frozen field order in the raw line.
        let mut last = 0usize;
        for k in [
            "\"ts\"",
            "\"mode\"",
            "\"tool\"",
            "\"command\"",
            "\"cwd\"",
            "\"decision\"",
            "\"ruleId\"",
            "\"reason\"",
        ] {
            let at = lines[0].find(k).unwrap();
            assert!(at >= last, "field order violated at {k}");
            last = at;
        }
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["decision"], "ask");
        assert_eq!(v2["ruleId"], "secret.env");
        assert!(v2["reason"].as_str().unwrap().contains("quotes"));
    }

    // ---- queue/timeout/session semantics (engine core) -----------------------

    #[tokio::test]
    async fn rule_deny_then_allow_runs() {
        let engine = Arc::new(engine_for_test(Mode::Enforce, 120_000));
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate_path("read_file", "/x/.env").await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let views = engine.pending_snapshot();
        assert_eq!(views.len(), 1, "expected one pending ask");
        assert_eq!(views[0].tool, "read_file");
        assert_eq!(views[0].rule_id, "secret.env");
        assert_eq!(views[0].cwd, "/x");
        assert!(engine.decide(&views[0].id, DecisionChoice::Allow).is_ok());
        assert!(matches!(g.await.unwrap(), GateResult::Run));
        // Late duplicate decision → AlreadyDecided until the entry expires.
        assert_eq!(
            engine
                .decide(&views[0].id, DecisionChoice::Allow)
                .unwrap_err(),
            DecideError::AlreadyDecided
        );
        assert!(engine.pending_snapshot().is_empty());
    }

    #[tokio::test]
    async fn timeout_denies_via_timeout_path() {
        let engine = Arc::new(engine_for_test(Mode::Enforce, 200));
        let mut rx = engine.subscribe();
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate_path("write_file", "/x/.env").await }
        });
        let first = rx.recv().await;
        assert!(matches!(first, Ok(Event::Pending(_))));
        match tokio::time::timeout(Duration::from_secs(5), g).await {
            Ok(Ok(GateResult::Blocked(text))) => {
                assert!(text.contains("timeout"), "text: {text}");
                assert!(text.contains("secret.env"), "text: {text}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        // Late decision AFTER the timeout ⇒ not found (frozen).
        assert!(engine.pending_snapshot().is_empty());
        // The resolved/timeout frame was broadcast (respects the pending).
        let mut got_resolved = false;
        while let Ok(ev) = rx.try_recv() {
            if let Event::Resolved { decision, .. } = ev
                && decision == "timeout"
            {
                got_resolved = true;
            }
        }
        assert!(got_resolved, "expected a resolved/timeout frame");
    }

    #[tokio::test]
    async fn deny_decision_blocks_tool() {
        let engine = Arc::new(engine_for_test(Mode::Enforce, 120_000));
        let mut rx = engine.subscribe();
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate_path("delete_file", "/x/.env").await }
        });
        let _ = rx.recv().await; // pending frame
        let views = engine.pending_snapshot();
        assert!(engine.decide(&views[0].id, DecisionChoice::Deny).is_ok());
        assert!(matches!(g.await.unwrap(), GateResult::Blocked(_)));
    }

    #[tokio::test]
    async fn session_map_skips_recheck_on_identical_triple() {
        let engine = Arc::new(engine_for_test(Mode::Enforce, 200));
        let mut rx = engine.subscribe();
        // First call: rule deny → ask → allow_session.
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate_path("read_file", "/w/.env").await }
        });
        let _ = rx.recv().await;
        let views = engine.pending_snapshot();
        assert!(
            engine
                .decide(&views[0].id, DecisionChoice::AllowSession)
                .is_ok()
        );
        assert!(matches!(g.await.unwrap(), GateResult::Run));
        // Second identical call short-circuits on the session map BEFORE any
        // re-check (would otherwise hit the rules' deny → ask again).
        assert!(matches!(
            engine.gate_path("read_file", "/w/.env").await,
            GateResult::Run
        ));
        // The shell-tool gate shares the same session map semantics.
        engine.audit("execute_command", "echo hi", "/w", "allow", "", "");
        assert!(matches!(
            engine.gate("execute_command", "echo hi", "/w").await,
            GateResult::Blocked(_) // unreachable sidecar ⇒ deny (no session entry)
        ));
    }

    #[tokio::test]
    async fn observe_mode_runs_everything_and_audits_deny() {
        let engine = Arc::new(engine_for_test(Mode::Observe, 200));
        assert!(matches!(
            engine.gate_path("read_file", "/x/.env").await,
            GateResult::Run
        ));
        let body = std::fs::read_to_string(engine.audit_path()).unwrap();
        assert!(body.contains("\"decision\":\"deny\""), "body: {body}");
        assert!(body.contains("secret.env"));
    }

    #[tokio::test]
    async fn off_mode_zero_checks_but_audits() {
        let engine = Arc::new(engine_for_test(Mode::Off, 200));
        assert!(matches!(
            engine.gate_path("read_file", "/x/.env").await,
            GateResult::Run
        ));
        assert!(matches!(
            engine.gate("execute_command", "rm -rf /", "/w").await,
            GateResult::Run
        ));
        let body = std::fs::read_to_string(engine.audit_path()).unwrap();
        assert!(body.contains("\"mode\":\"off\""));
        assert!(body.contains("\"decision\":\"allow\""));
    }

    #[test]
    fn json_quote_escapes() {
        assert_eq!(json_quote("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(json_quote("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn mode_parse_table() {
        assert_eq!(Mode::parse("enforce"), Ok(Mode::Enforce));
        assert_eq!(Mode::parse("observe"), Ok(Mode::Observe));
        assert_eq!(Mode::parse("off"), Ok(Mode::Off));
        assert!(Mode::parse("wat").is_err());
    }

    #[test]
    fn dir_of_shapes() {
        assert_eq!(dir_of("/x/.env"), "/x");
        assert_eq!(dir_of("/x"), "/");
        assert_eq!(dir_of(".env"), ".");
    }

    #[test]
    fn decision_choice_parse_table() {
        assert_eq!(DecisionChoice::parse("allow"), Some(DecisionChoice::Allow));
        assert_eq!(DecisionChoice::parse("deny"), Some(DecisionChoice::Deny));
        assert_eq!(
            DecisionChoice::parse("allow_session"),
            Some(DecisionChoice::AllowSession)
        );
        assert!(DecisionChoice::parse("wat").is_none());
    }
    // ---- integration: a real node sidecar (stub script) ----------------------
    // These tests exercise the FULL loop: spawn node → newline-JSON protocol
    // → engine mapping → pending ask → decision/timeout → SSE frames.

    /// Needs `node` on PATH (the same binary the engine would use).
    fn node_available() -> bool {
        match std::process::Command::new("node").arg("--version").output() {
            Ok(o) => o.status.success(),
            Err(_) => false,
        }
    }

    /// Writes a stub sidecar script that denies (or allows, per `kind`)
    /// every request; the reply's reason includes whether
    /// `RYNS_AUTH_TOKEN` reached the child env (must be unset — frozen).
    fn write_stub_script(tag: &str, kind: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ryns-stub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("stub.mjs");
        std::fs::write(
            &script,
            format!(
                r#"
import {{ createInterface }} from 'node:readline';
const rl = createInterface({{ input: process.stdin }});
const tokenSeen = process.env.RYNS_AUTH_TOKEN === undefined ? 'unset' : 'present';
rl.on('line', (line) => {{
  if (!line.trim()) return;
  try {{
    const req = JSON.parse(line);
    const out = req.op === 'checkCommand' && tokenSeen === 'unset' && '{}' === 'deny'
      ? {{ v: 1, id: req.id, kind: 'deny', reason: 'stub denial (' + tokenSeen + ')', ruleId: 'stub.rule' }}
      : {{ v: 1, id: req.id, kind: 'allow' }};
    process.stdout.write(JSON.stringify(out) + '\n');
  }} catch {{}}
}});
rl.on('close', () => process.exit(0));
"#,
                kind
            ),
        )
        .unwrap();
        script
    }

    fn engine_with_stub(mode: Mode, timeout_ms: u64, script: &std::path::Path) -> Engine {
        let ws = std::env::temp_dir().join(format!(
            "ryns-safety-{}-{}-{:x}",
            mode.label(),
            script.file_name().unwrap().to_string_lossy(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        Engine::new(
            &Config {
                mode,
                ask_timeout_ms: timeout_ms,
                node_bin: "node".to_string(),
                sidecar_script: script.to_string_lossy().into_owned(),
            },
            &ws.to_string_lossy(),
        )
    }

    #[tokio::test]
    async fn sidecar_deny_creates_pending_allow_runs() {
        if !node_available() {
            return;
        }
        let script = write_stub_script("deny", "deny");
        let engine = Arc::new(engine_with_stub(Mode::Enforce, 120_000, &script));
        let mut rx = engine.subscribe();

        // engine deny → pending ask (frozen mapping).
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate("execute_command", "echo hi", "/w").await }
        });
        let first = rx.recv().await;
        match first {
            Ok(Event::Pending(ref v)) => {
                assert_eq!(v.tool, "execute_command");
                assert_eq!(v.rule_id, "stub.rule");
                // Token MUST NOT leak into the sidecar child env (frozen).
                assert!(
                    v.reason.contains("unset"),
                    "stub saw the token: {}",
                    v.reason
                );
            }
            other => panic!("expected pending event, got {other:?}"),
        }
        let views = engine.pending_snapshot();
        assert_eq!(views.len(), 1);
        assert!(engine.decide(&views[0].id, DecisionChoice::Allow).is_ok());
        assert!(matches!(g.await.unwrap(), GateResult::Run));

        // Second round: deny decision → blocked.
        let g2 = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate("execute_command", "rm -rf /", "/w").await }
        });
        // Drain buffered rounds until the fresh pending shows up.
        loop {
            let ev = rx.recv().await;
            let Ok(Event::Pending(_)) = ev else { continue };
            break;
        }
        let views = engine.pending_snapshot();
        assert!(engine.decide(&views[0].id, DecisionChoice::Deny).is_ok());
        match g2.await.unwrap() {
            GateResult::Blocked(text) => assert!(text.contains("stub denial"), "text: {text}"),
            other => panic!("expected block, got {other:?}"),
        }
        let mut resolutions = 0;
        while let Ok(ev) = rx.try_recv() {
            if let Event::Resolved { .. } = ev {
                resolutions += 1;
            }
        }
        assert!(
            resolutions >= 2,
            "expected resolved frames, got {resolutions}"
        );
    }

    #[tokio::test]
    async fn sidecar_allow_runs_immediately() {
        if !node_available() {
            return;
        }
        let script = write_stub_script("allow", "allow");
        let engine = Arc::new(engine_with_stub(Mode::Enforce, 120_000, &script));
        assert!(matches!(
            engine.gate("execute_command", "echo hi", "/w").await,
            GateResult::Run
        ));
        let body = std::fs::read_to_string(engine.audit_path()).unwrap();
        assert!(body.contains("\"decision\":\"allow\""), "body: {body}");
    }

    #[tokio::test]
    async fn sidecar_deny_timeout_yields_timeout_block() {
        if !node_available() {
            return;
        }
        let script = write_stub_script("deny", "deny");
        let engine = Arc::new(engine_with_stub(Mode::Enforce, 150, &script));
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.gate("execute_command", "echo hi", "/w").await }
        });
        match tokio::time::timeout(Duration::from_secs(5), g).await {
            Ok(Ok(GateResult::Blocked(text))) => {
                assert!(text.contains("timeout"), "text: {text}");
                assert!(text.contains("stub.rule"), "text: {text}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Late decision after the timeout → NotFound (frozen).
        let views = engine.pending_snapshot();
        assert!(views.is_empty());
    }

    #[tokio::test]
    async fn apply_patch_env_target_creates_pending_and_block() {
        if !node_available() {
            return;
        }
        // Sidecar stub ALLOWS (so the sidecar gate passes); the secret-path
        // rules on the patch TARGETS must still create the pending review.
        let script = write_stub_script("patch-allow", "allow");
        let engine = Arc::new(engine_with_stub(Mode::Enforce, 120_000, &script));
        let ws = std::fs::canonicalize(
            std::path::Path::new(&engine.audit_path())
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap()
        .to_path_buf();
        let ctx = crate::tools::Ctx {
            workspace: Arc::from(ws.to_string_lossy().to_string()),
            safety: Some(engine.clone()),
            approved_ask: None,
        };
        engine.start_sweeper();
        // create .env so the patch is a modification of a secret path
        let env_path = ws.join(".env");
        std::fs::write(&env_path, "A=1\n").unwrap();
        let patch_input = crate::tools::files::PatchInput {
            patch: "--- a/.env\n+++ b/.env\n@@ -1,1 +1,1 @@\n-A=1\n+A=SUCCESS_PATCH\n".into(),
            cwd: Some(".".into()),
            dry_run: false,
        };
        let g = tokio::spawn({
            let ctx = ctx.clone();
            let engine = engine.clone();
            async move {
                let blocked = crate::tools::files::apply_patch(&ctx, patch_input.clone()).await;
                (blocked, engine)
            }
        });
        // Drain events until the pending shows up in the queue.
        let mut rx = engine.subscribe();
        loop {
            let Ok(Event::Pending(_)) = rx.recv().await else {
                continue;
            };
            break;
        }
        let views = engine.pending_snapshot();
        assert_eq!(views.len(), 1, "expected one pending from the .env patch");
        assert_eq!(views[0].rule_id, "secret.env");
        assert!(engine.decide(&views[0].id, DecisionChoice::Deny).is_ok());
        let (blocked_text, _) = g.await.unwrap();
        assert!(
            blocked_text.starts_with("safety_error:"),
            "unexpected: {blocked_text}"
        );
        // The file must be UNTOUCHED (deny before run).
        assert_eq!(std::fs::read_to_string(&env_path).unwrap(), "A=1\n");

        // Harmless patch: no secret targets → applies normally.
        let safe_input = crate::tools::files::PatchInput {
            patch: "--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1 @@\n+hello\n".into(),
            cwd: Some(".".into()),
            dry_run: false,
        };
        let text = crate::tools::files::apply_patch(&ctx, safe_input).await;
        assert!(text.starts_with("exit_code: 0"), "text: {text}");
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "hello\n"
        );
    }

    // ---- §A5: forced ask (policy access=ask) and safety bypass -------------

    /// edison major #2 [blocker follow-up]: with access="ask" and per-tool
    /// safety=true, the invocation must produce EXACTLY ONE pending — the
    /// forced ask. After the allow decision, gate_shell (deny-sidecar) and
    /// gate_paths (secret target) must NOT re-ask: the task-local invocation
    /// marker covers the whole call and the patch applies.
    #[tokio::test]
    async fn apply_patch_access_ask_prompts_exactly_once() {
        if !node_available() {
            return;
        }
        // Deny stub: WITHOUT the marker the gate_shell sidecar check would
        // queue a second pending after the (already-approved) forced ask.
        let script = write_stub_script("single-ask", "deny");
        let ws = temp_ws_for("single-ask");
        let engine = Arc::new(Engine::new(
            &Config {
                mode: Mode::Enforce,
                ask_timeout_ms: 120_000,
                node_bin: "node".into(),
                sidecar_script: script.to_string_lossy().into_owned(),
            },
            &ws,
        ));
        crate::policy::ensure_defaults(&ws);
        crate::policy::update_entry(&ws, "apply_patch", crate::policy::Access::Ask, true).unwrap();
        let ctx = crate::tools::Ctx {
            workspace: ws.clone().into(),
            safety: Some(engine.clone()),
            approved_ask: None,
        };
        // The dispatch pre-check (call_tool's Ask branch): forced ask #1.
        let patch: String = "--- a/secret.txt\n+++ b/secret.txt\n@@ -0,0 +1 @@\n+patched\n".into();
        {
            let engine2 = engine.clone();
            let g = tokio::spawn({
                let patch = patch.clone();
                let ws = ws.clone();
                async move { engine2.forced_ask("apply_patch", &patch, &ws).await }
            });
            // Wait for the pending, then approve.
            tokio::time::sleep(Duration::from_millis(80)).await;
            let views = engine.pending_snapshot();
            assert_eq!(views.len(), 1, "forced ask creates exactly one pending");
            assert!(engine.decide(&views[0].id, DecisionChoice::Allow).is_ok());
            assert!(matches!(g.await.unwrap(), GateResult::Run));
        }
        let before = engine.pending_snapshot().len();
        assert_eq!(before, 0);
        // Simulate call_tool building the ctx WITH the invocation marker.
        let mut gctx = ctx.clone();
        gctx.approved_ask = Some("apply_patch".into());
        let out = crate::tools::files::apply_patch(
            &gctx,
            crate::tools::files::PatchInput {
                patch,
                cwd: Some(".".into()),
                dry_run: false,
            },
        )
        .await;
        assert!(
            out.starts_with("exit_code: 0"),
            "patch must run after the single ask: {out}"
        );
        assert_eq!(
            std::fs::read_to_string(format!("{ws}/secret.txt")).unwrap(),
            "patched\n"
        );
        // No second pending was ever created by the inner gates.
        assert_eq!(
            engine.pending_snapshot().len(),
            0,
            "no re-asks after the marker"
        );
        let body = std::fs::read_to_string(format!("{ws}/.state/safety-audit.jsonl")).unwrap();
        assert!(
            body.contains("ops.ask+invocation"),
            "gates must audit the marker: {body}"
        );
        assert_eq!(
            body.matches("\"decision\":\"ask\"").count(),
            1,
            "exactly one ask line"
        );
    }

    #[tokio::test]
    async fn forced_ask_creates_pending_when_engine_would_allow() {
        if !node_available() {
            return;
        }
        // Sidecar stub ALLOWS: without the access=ask routing this would run
        // immediately. The forced ask must still queue it.
        let script = write_stub_script("force-allow", "allow");
        let ws = temp_ws_for("force-ask");
        let engine = Arc::new(Engine::new(
            &Config {
                mode: Mode::Enforce,
                ask_timeout_ms: 120_000,
                node_bin: "node".into(),
                sidecar_script: script.to_string_lossy().into_owned(),
            },
            &ws,
        ));
        // policy access=ask for execute_command (file read decides behavior).
        crate::policy::ensure_defaults(&ws);
        crate::policy::update_entry(&ws, "execute_command", crate::policy::Access::Ask, true)
            .unwrap();
        // The pending opens on the first call…
        let mut rx = engine.subscribe();
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.forced_ask("execute_command", "echo hi", "/w").await }
        });
        loop {
            let Ok(Event::Pending(_)) = rx.recv().await else {
                continue;
            };
            break;
        }
        let views = engine.pending_snapshot();
        assert_eq!(views.len(), 1, "forced ask must create a pending");
        assert_eq!(views[0].rule_id, "ops.ask");
        // …and the decision resolves the gated future with Run.
        assert!(engine.decide(&views[0].id, DecisionChoice::Allow).is_ok());
        assert!(matches!(g.await.unwrap(), GateResult::Run));
    }

    #[tokio::test]
    async fn forced_ask_session_shortcut_and_timeout_deny() {
        let ws = temp_ws_for("force-session");
        let engine = Arc::new(Engine::new(
            &Config {
                mode: Mode::Enforce,
                ask_timeout_ms: 150,
                node_bin: "node".into(),
                // Unreachable script is fine: forced_ask never consults the
                // sidecar (mandatory queue, independent of engine verdict).
                sidecar_script: "/nonexistent/index.mjs".into(),
            },
            &ws,
        ));
        // timeout path: no decision → deny with reason timeout.
        let g = tokio::spawn({
            let eng = engine.clone();
            async move { eng.forced_ask("execute_command", "echo hi", "/w").await }
        });
        match tokio::time::timeout(Duration::from_secs(5), g).await {
            Ok(Ok(GateResult::Blocked(text))) => {
                assert!(
                    text.contains("timeout") && text.contains("ops.ask"),
                    "got: {text}"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        // session shortcut: allow_session then the SAME triple skips the queue.
        let first = tokio::spawn({
            let eng = engine.clone();
            async move { eng.forced_ask("execute_command", "echo second", "/w").await }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        let views = engine.pending_snapshot();
        assert!(
            engine
                .decide(&views[0].id, DecisionChoice::AllowSession)
                .is_ok()
        );
        assert!(matches!(first.await.unwrap(), GateResult::Run));
        let direct = engine
            .forced_ask("execute_command", "echo second", "/w")
            .await;
        assert!(
            matches!(direct, GateResult::Run),
            "session shortcut must skip the queue"
        );
    }

    /// §A5: policy safety=false bypasses the gate on a gated tool even in
    /// enforce mode with a deny-stub sidecar → command RUNS; safety=true
    /// blocks (deny → pending → timeout).
    #[tokio::test]
    async fn safety_false_bypass_in_enforce() {
        if !node_available() {
            return;
        }
        let script = write_stub_script("bypass", "deny");
        let ws = temp_ws_for("bypass");
        let engine = Arc::new(Engine::new(
            &Config {
                mode: Mode::Enforce,
                ask_timeout_ms: 150,
                node_bin: "node".into(),
                sidecar_script: script.to_string_lossy().into_owned(),
            },
            &ws,
        ));
        crate::policy::ensure_defaults(&ws);
        let ctx = crate::tools::Ctx {
            workspace: ws.clone().into(),
            safety: Some(engine.clone()),
            approved_ask: None,
        };
        // safety=false → gate skipped → command runs, output intact.
        crate::policy::update_entry(
            &ws,
            "execute_command",
            crate::policy::Access::Enabled,
            false,
        )
        .unwrap();
        let out = crate::tools::execute_command::run(
            &ctx,
            crate::tools::execute_command::ExecuteInput {
                command: "echo bypass-exit".into(),
                ..Default::default()
            },
        )
        .await;
        assert!(
            out.ends_with("exit_code: 0\nstdout:\nbypass-exit\n"),
            "bypass run text: {out}"
        );
        let body = std::fs::read_to_string(format!("{ws}/.state/safety-audit.jsonl")).unwrap();
        assert!(body.contains("policy.safety_off"), "audit: {body}");

        // safety=true (default) → the deny sidecar wins → blocked (timeout).
        let _ = crate::policy::update_entry(
            &ws,
            "execute_command",
            crate::policy::Access::Enabled,
            true,
        );
        let blocked = crate::tools::execute_command::run(
            &ctx,
            crate::tools::execute_command::ExecuteInput {
                command: "echo must-not-run".into(),
                ..Default::default()
            },
        )
        .await;
        assert!(
            blocked.contains("safety_error: denied (stub.rule): timeout"),
            "blocked text: {blocked}"
        );
    }

    fn temp_ws_for(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ryns-safety-ws-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn observe_mode_runs_engine_denies_and_audits() {
        if !node_available() {
            return;
        }
        let script = write_stub_script("deny", "deny");
        let engine = Arc::new(engine_with_stub(Mode::Observe, 120_000, &script));
        assert!(matches!(
            engine.gate("execute_command", "echo hi", "/w").await,
            GateResult::Run
        ));
        let body = std::fs::read_to_string(engine.audit_path()).unwrap();
        assert!(body.contains("\"decision\":\"deny\""), "body: {body}");
        assert!(body.contains("stub.rule"));
    }
}

# PLAN: Go → Rust port + safety layer + Tauri GUI

Decision record (user, 2026-11-03): full Rust rewrite (option B), cc-safety-net via **persistent Node sidecar** (`checkCommand` library API), GUI = **Tauri desktop**, ASK timeout default = **deny (fail-closed)**.

## 0. Principles & relationship to the Go server

- The Go server (`main.go`/`platform.go`, frozen at 0.8.0) is the **behavioral reference**. Rust must match its tool behavior; diffs found during parity count as Rust bugs unless the design doc says otherwise.
- The legacy TS variant (`src/index.ts`) is out of scope.
- Not a sandbox (cc-safety-net upstream says so too): safety net is pre-execution veto + audit, not isolation.
- All Go-era verification lessons apply to Rust (workspace handling, CRLF normalization, fail-closed auth, no token inheritance to children).

## 1. Verified facts driving this design

- `rmcp` (official Rust MCP SDK) supports stdio + streamable-HTTP servers; implementers must verify the exact API of the pinned version via context7 (`/websites/rs_rmcp_rmcp`) before coding.
- `cc-safety-net` (MIT) ships `import { checkCommand } from 'cc-safety-net/api'` (ESM-only, Node ≥22, synchronous). Input `{command: string, cwd: string}` — cwd absolute, must exist and be readable, no fallback default. Result: `{kind:'allow'}` | `{kind:'deny', reason, ruleId}` (binary; no "ask"). Throws `TypeError` on bad input; throw ⇒ treat as deny (fail-closed). Reads user policy + `<cwd>/.cc-safety-net/` project policy (no parent-directory search — pass the tool call's actual cwd). No audit records written; no network.
- No upstream API for file-tool checks ⇒ our own minimal secret-path ruleset on file tools (must not be presented as upstream parity).

## 2. Brownfield contract (must not break)

| Item | Value |
| --- | --- |
| Env: unchanged | `RYNS_WORKSPACE_ROOT`, `RYNS_PUBLIC_BASE_URL`, `RYNS_AUTH_TOKEN` (req. HTTP mode), plus existing tool envs |
| Routes | `/mcp`, `/assets/`, `/api/save/` — all behind bearer auth (`Authorization: Bearer` ci-scheme OR `X-RYNS-Auth`), constant-time, fail-closed, 401 `{"error":"unauthorized"}` |
| Data files | sheets & todos JSON layout identical → drop-in over an existing workspace |
| Child env | never inherit `RYNS_AUTH_TOKEN` (exact-key, case-insensitive filter) |
| CLI | `-port N` = HTTP mode (auth required), else stdio |
| New env | `RYNS_SAFETY_MODE` = `enforce` (default) / `observe` (log, allow) / `off`; `RYNS_ASK_TIMEOUT_MS` (default 120000); `RYNS_NODE_BIN` (default `node`); `RYNS_SIDECAR_SCRIPT` (default `<repo>/scripts/safety-sidecar/index.mjs`) |

## 3. Architecture (Rust workspace: single crate at first)

```
ryns-code-mcp-rs/            # new workspace root at repo top level
  crate: ryns-code-mcp (src/)
    main.rs        # cli: -port, env bootstrap, server bootstrap
    mcp/           # rmcp wiring: stdio + streamable http, auth layer
    tools/         # one module per Go tool group; same tool names & textResult formats
    auth.rs        # bearer middleware (mirrors Go authMiddleware semantics, constant-time)
    shell.rs       # shellCommand(): bash -lc | powershell -NoProfile…; LF normalization
    platform/      # per-OS cfg'ted helpers (mirror platform.go + signal split)
    safety/        # policy layer: sidecar client (tokio child, persistent, restart-on-exit),
                   #   decision mapping, ASK queue (tokio watch/bus + timeout), audit JSONL
    sheets.rs, todos.rs, acp.rs   # later phases
scripts/safety-sidecar/index.mjs   # Node sidecar: newline-JSON stdin/stdout around checkCommand
src-tauri/                    # Phase 4 Tauri app (separate folder, talks HTTP to server)
.run parity harness (scripts/parity/)  # drive Go & Rust via JSON-RPC, diff tool outputs
```

Sidecar protocol (newline-delimited JSON over stdin/stdout, one line per msg):
- Request: `{"v":1,"id":<u64>,"op":"checkCommand","command":<str>,"cwd":<abs str>}`
- Response: `{"v":1,"id":<u64>,"kind":"allow"}` or `{"v":1,"id":<u64>,"kind":"deny","reason":<str>,"ruleId":<str>}` or `{"v":1,"id":<u64>,"kind":"error","message":<str>}`
- Sidecar maps local throw → `kind:"deny", ruleId:"custom.sidecar-error"`; transport failure/timeout (sidecar call budget 5s) → Rust treats as deny (enforce) / allow+warn (observe); sidecar process death → respawn once per call, then fail per mode.

Decision mapping (v1, frozen):
- engine `allow` → run.
- engine `deny` → **ASK**: push pending request {id, tool, command, cwd, reason, ruleId, ts} to GUI; wait ≤ `RYNS_ASK_TIMEOUT_MS`; user Allow/Deny/Allow-same-command(session) decide; timeout ⇒ deny; result logged to audit JSONL (`.state/safety-audit.jsonl`).
- sidecar `error`/absent in `enforce` ⇒ deny with actionable message; in `observe` ⇒ allow + log.
- Gated tools v1: `execute_command`, `start_process` (+`send_input` restarts), `apply_patch`. Plus own secret-path rules (exact list in Phase 3 spec: `.env*`, `id_rsa`/`id_ed25519`, `~/.aws`, `~/.ssh`, credential stores) applied to `read_file`, `write_file`, `edit_file`, `multi_edit_file`, `delete_file`, `move_file`, `grep`/`search_files` (path-based, minimal). Not upstream parity; documented as such.

## 4. Phases & acceptance gates

- **P1 (alpha skeleton)**: workspace + rmcp stdio+HTTP + auth + `execute_command`,`list_directory`,`read_file`,`write_file` + shell.rs + platform skeleton. Gate: `cargo fmt --check`? (n/a yet) → `cargo build`, `cargo test`, `cargo clippy -- -D warnings`, `cargo check --target x86_64-pc-windows-msvc --target aarch64-apple-darwin` (no link), JSON-RPC smoke (initialize/tools/list/version string `0.9.0-rust.1`) + execute_command output parity vs Go (same input → same shape text).
- **P2 (parity)**: all 55 tools modulo conscious divergence list (sheets/todos/gold/ACP/lint/web…). Parity harness diff on scripted call set. Windows: `cargo check` cross targets; runtime Windows needs user-provided host.
- **P3 (safety)**: sidecar script + runner + policy layer + ASK queue + audit log + README section + tests (unit for mapping, integration for queue timeout).
- **P4 (GUI)**: Tauri desktop: approvals view, activity feed, settings (mode, timeout, presets passthrough to `npx cc-safety-net gui` for policy tuning); talks to server localhost API (+ optional tunnel). Review with lilith after design-taste-frontend skill load.
- **P5 (cutover)**: `run` script delegates to Rust binary; Go code archived to `legacy-go/` (kept for parity reference); packaging (cargo-bundle/tauri bundle); README rewritten; version → `1.0.0`.

Known open decisions (bring back to user when reached): ACP port keep/drop; PTY interactive parity approach in Rust (portable-pty crate?); Node dependency acceptance for Windows machines (or bundle node).

## 5. Accepted divergences (frozen ledger — parity harness asserts against this)

P1 (skeleton, verified 2026-11-03):
- /mcp POST content-type: Go `text/event-stream`, Rust `application/json` (both spec-compliant).
- `tools/list` omits Go's `outputSchema` advertisement (rmcp macro return-type limitation); `tools/call` result shape carries `{"text": …}` as in Go. Source-visible only.
- HTTP header NAMES: hyper lowercases (Go canonicalizes) — normalize header case in any strict byte-diff harness.
- `parse_flag_int` accepts signed decimal only (Go base-0 also parses 0x/0b/0o forms — refused as parse error in Rust).
- `path_clean_windows`: `\\?\` verbatim paths returned as-is; `..` above a UNC share purposefully untested.
- `list_directory` reports `readdirent dir: …` on broken directory iterations (Go may stop silently — Rust is stricter-by-error, intentional).
- Pending (P2): read_file image branch (png/jpg → text + inline image); `/assets/` + `/api/save/` routes; PTY/start_process; remaining 51 tools; annotations titles now done, but `outputSchema` stays omitted.

P3 reminder: P1 alpha has NO safety layer — do not operate as a public machine-control server until the Phase-3 gate ships.
## 6. GUI safety API contract (frozen 2026-11-03, user approved full rollout)

All `/api/safety/*` routes live behind the SAME bearer middleware as `/mcp`,`/assets/`,`/api/safety/` units (never widened). Header auth only (no query-param tokens). The Tauri shell does HTTP from its Rust core (headers attached); the webview JS talks to the shell over Tauri IPC, never browser fetch.

- `GET /api/safety/pending` → `200 {"pending":[{"id","tool","command","cwd","reason","ruleId","created_at","expires_at"}]}` (expires_at = created_at + RYNS_ASK_TIMEOUT_MS; server enforces timeout⇒deny regardless).
- `POST /api/safety/decide/{id}` body `{"decision":"allow"|"deny"|"allow_session"}` → `200 {"ok":true}` | `404 {"error":"not_found"}` (expired/unknown id) | `409 {"error":"already_decided"}`.
- `allow_session` applies to identical (tool,command,cwd) triples only, session-scoped, in-memory, cleared on restart; NOT persisted.
- `SSE GET /api/safety/events` → `event: pending` / `event: resolved` frames `{"id",...}`. Keepalive comment every 15s. Requires auth like every other route.
- Decision semantics frozen: policy engine `deny` (either sidecar or built-in secret-path rules) ⇒ tool BLOCKED and a pending ASK entry is created; user may Allow (once) / Allow-session / Deny; no decision by timeout ⇒ deny with `timeout` reason; `allow` from engine ⇒ run immediately, no GUI involvement (unless RYNS_SAFETY_MODE=observe).
- Sidecar failure in enforce mode ⇒ deny (fail-closed) and audit entry; in observe mode ⇒ run + audit `sidecar_error`.

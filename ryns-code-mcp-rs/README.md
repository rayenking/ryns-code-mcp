# ryns-code-mcp (Rust port)

Rust rewrite of the `ryns-code-mcp` MCP server (Phase 2 per
`../docs/PLAN_RUST_PORT.md`). The frozen Go server (`../legacy-go/`, 0.8.0) is the
behavioral reference; tool outputs must match it byte-for-byte unless a
divergence is documented below.

## What works (P2)

- **Transports:** MCP over **stdio** (default) and **Streamable HTTP** via
  `-port N` (`127.0.0.1:N`, stateless, `Host`-header validation disabled to
  match the Go tunnel layout).
- **Auth (HTTP mode):** bearer tokens from `RYNS_AUTH_TOKEN` (comma-separated,
  trimmed, empties dropped) via `Authorization: Bearer` (case-insensitive
  scheme) or `X-RYNS-Auth`. Constant-time over every candidate × token, no
  early exit, fail-closed; 401 body `{"error":"unauthorized"}`. HTTP mode
  refuses to start without a token; stdio never checks auth. Children never
  inherit `RYNS_AUTH_TOKEN` (exact key, case-insensitive filter).
- **HTTP routes (Go parity):** `/mcp` (Streamable HTTP), `/assets/` static
  files, `/api/save/` JSON writer (segment guard + method check + 2 MB cap),
  plus the Go-shaped 404 fallback — all behind the auth middleware.
- **Tools (55):** the full Go `setupTools` set, byte-identical on the parity
  spot-check list (files, edits, patch, tree, ripgrep search/grep, git, processes
  incl. PTY via `script`, workspace_info, local sheets + todos drop-in JSON,
  desktop automation with the Go `windowsUnsupported` gates, lint_format,
  gold price, ACP delegation, system_stats). `tools/list` returns exactly
  Go's 55 names in Go's registration order.

- **Safety layer (Phase 3):** every gated call is checked pre-execution and
  written to the append-only audit log `<workspace>/.state/safety-audit.jsonl`.
  - Mode via `RYNS_SAFETY_MODE`: `enforce` (default; policy violations block
    and open a pending human review — deny → ASK, timeout ⇒ deny),
    `observe` (log only, everything runs), `off` (zero checks, still audited).
  - `RYNS_ASK_TIMEOUT_MS` (default 120000): pending expires at
    `created_at + timeout` ⇒ deny with reason `timeout`.
  - Shell tools (`execute_command`, `start_process`, `send_input`,
    `apply_patch`) are checked through the CC safety-net sidecar (Node;
    `RYNS_NODE_BIN` default `node`, `RYNS_SIDECAR_SCRIPT` default
    `scripts/safety-sidecar/index.mjs` relative to the launcher CWD). File
    tools get the built-in secret-path rules (`.env*`, `id_rsa`/`id_ed25519`,
    `~/.ssh`, `~/.aws`, macOS `Library/Keychains`) on their path arguments.
  - GUI/API: `GET /api/safety/pending`, `POST /api/safety/decide/{id}`
    (`{"decision":"allow"|"deny"|"allow_session"}`; `allow_session` skips
    re-checks for identical tool+command+cwd in this process), SSE on
    `GET /api/safety/events` (`event: pending`/`resolved`, `: ping` every
    15 s). `expire`d ids answer 404; already-decided duplicates answer 409
    until expiry.
  - Audit line: appended atomically (O_APPEND, mode 0600 on unix) to
    `<workspace>/.state/safety-audit.jsonl`; no rotation yet — rotate or
    truncate the file manually (it grows one line per gated call).
  - Deploy step: run `npm install` in `scripts/safety-sidecar/` (pins
    `cc-safety-net ^2.7.2`; Node ≥ 22). Without a working sidecar in enforce
    mode every shell check fails closed (`sidecar_error` → deny).
  - Blocked calls answer the tool text `safety_error: denied ({ruleId}): {reason}`.

## Build / test / run

```sh
cargo build                      # debug build
cargo test                       # full test suite
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run                        # stdio mode
RYNS_AUTH_TOKEN=t ./target/release/ryns-code-mcp -port 8799   # HTTP mode
```

Cross-platform gates (no linking, so `cargo check` suffices):
`cargo check --target x86_64-pc-windows-msvc`, `--target
x86_64-unknown-linux-musl`, `--target aarch64-pc-windows-msvc`. Windows shell
commands run via `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy
Bypass -Command`; unix via `/bin/bash -lc`; outputs are CRLF-normalized.

## Environment

| Var | Meaning | Default |
| --- | --- | --- |
| `RYNS_WORKSPACE_ROOT` | workspace base for relative paths | `<home>/RynsWorkspace` |
| `RYNS_AUTH_TOKEN` | comma-separated bearer tokens (HTTP mode required) | — |
| `RYNS_PUBLIC_BASE_URL` | base for asset links (record_video, sheet_url) | `http://127.0.0.1:8799` |

## Known divergences from Go (tracked in the port plan)

- `GET /mcp` (and other wrong-method hits) reply with a bare `405 Method
  Not Allowed` status; Go also carries a `text/plain` `Method Not Allowed
`
  body. Same status, no body framing (differs from Go's `http.Error` shape).
- `/mcp` response **encoding** differs: the Go SDK replies
  `text/event-stream`, rmcp replies `application/json`. Both are compliant
  MCP Streamable HTTP responses; the response `Content-Type` on `/mcp` is a
  documented difference, not a parity claim.
- HTTP tools schema details may differ slightly (`type:["string","null"]` for
  optional fields, `additionalProperties`); the tool **text outputs** are the
  parity contract.
- `list_processes` rows come in registration order (Go iterates a Go map):
  same set, deterministic order.
- `search_files`/`grep` output keeps ripgrep's fs-walk order — nondeterministic
  between workspaces (same file set, same as running rg twice on two copies of
  a tree).
- `/assets/` static handler has no directory listing HTML and no `index.html`
  fallback (Go's `http.FileServer` lists dirs); missing files/dirs return the
  Go 404 shape.
- `/api/save` without the trailing slash returns the Go-shaped 404 (Go's
  ServeMux would redirect to `/api/save/`).
- `workspace_info` reports `node: "not used (Rust)"` / `go: "n/a (Rust port)"`
  (honest labels; Go reports runtime versions).
- `lint_format` uses the same tool matrix but Python formatting is reported as
  not installed (matching the frozen Go box).
- `check_gold_price` shells out to the system `curl` (Go uses the stdlib TLS
  stack); error wording preserved.
- ACP (`acp_opencode`) auto-approves permission requests like the headless Go
  client; underlying opencode binary must exist in PATH.
- Windows error wording relies on Rust `io::Error` Display (no BSD errno
  table); unix paths map errors onto Go's `os.PathError` wording.
- Go's signal-on-exit naming maps to `platform::signal_name` for common
  signals; unknown signals fall back to `signal N`.

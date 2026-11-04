# PLAN: Ryns Control (MCP control panel)

User-corrected product spec (2026-11-04). This SUPERSEDES the GUI parts of
PLAN_RUST_PORT.md §6 for the new app, but keeps the server-side safety
engine. `PLAN_RUST_PORT.md` §2 brownfield invariants mostly stand, with two
user-driven changes listed in §A below.

## 0. Product (what the user wants)

Desktop app "Ryns Control" (`com.ryns.control`, bundle dir
`src-tauri/`, productName in tauri.conf `ryns-mcp-control`):

1. **Server card.** Start / Stop / Restart buttons, status dot, and the MCP
   server runs AS A CHILD OF THE APP ("jalan bareng appnya"). App spawns the
   bundled server binary on launch; on Quit the server child is stopped
   (then user choice preserved for "keep running"? NO — v1: stop on quit,
   app relaunches it on next open). Auto-start on boot = tauri plugin
   `tauri-plugin-autostart` (Launch at login toggle in the app; app opens
   hidden in tray and spawns the server).
2. **Tools tab.** List ALL 55 tools. Clicking a tool opens per-tool controls:
   - **Access**: `enabled | disabled | ask` (segmented control)
   - **Safety**: on/off switch (per-tool)
3. **Safety tab.** Existing pending ask/allow/deny console (SSE live),
   unchanged feature set, kept as one tab.
4. **No token.** Auth is OFF by default; `.env` removed by orchestrator.

Server binary + safety sidecar are bundled INSIDE the `.app` under
`Contents/Resources/server/` (binary `ryns-code-mcp`, sidecar
`safety-sidecar/index.mjs` + its `node_modules`). GUI spawns its own copy —
the installed app must NOT depend on the repo checkout. Server spawn env:

- `RYNS_WORKSPACE_ROOT` (default `~/RynsWorkspace`; server default applies)
- `RYNS_SIDECAR_SCRIPT=<Resources>/server/safety-sidecar/index.mjs`
- `RYNS_SAFETY_MODE=enforce` (default; observe|off supported)
- NO `RYNS_AUTH_TOKEN` (default install); if GUI setting sets one, pass it
  (server then engages middleware — see §A1).
- Port default 8799 (GUI setting; `--port` arg).

GUI talks to `http://127.0.0.1:<port>`; it MUST handle "server stopped"
states gracefully (all cards degrade, Start button primary).

## A. Server-side contract changes (shaka)

### A1. Auth becomes OPTIONAL

- `RYNS_AUTH_TOKEN` unset/empty → NO middleware, localhost-only bind (port
  stays `127.0.0.1`). `-port` no longer fatally requires it. Stdio unchanged.
- Set → middleware arms EXACTLY as today (all routes incl. `/api/*`,
  constant-time, fail-closed).
- `.env` example wording updated; README updated (orchestrator ships).

### A2. Per-tool policy (the core feature)

File: `<repo-or-workspace?>/.state/tool-policy.json` — anchored NEXT TO the
server binary's own state: reuse existing `.state/` semantics (workspace
root `.state/`, same dir the server already uses for pid/audit). Created ON
FIRST RUN with all 55 tools at defaults:

```json
{
  "version": 1,
  "tools": {
    "execute_command": { "access": "enabled", "safety": true },
    "screenshot":      { "access": "enabled", "safety": false },
    "...":             { "access": "enabled", "safety": false }
  }
}
```

Defaults: `access: "enabled"` for all; `safety: true` for the 12 currently
gated tools (execute_command, start_process, send_input, apply_patch,
read_file, write_file, edit_file, multi_edit_file, delete_file, move_file,
grep, search_files), `false` for the rest.

Read policy ON EVERY tools/list and EVERY tools/call (simplest fresh-read;
file is tiny). Malformed file → fall back to DEFAULTS + log warning
(fail-open-to-defaults for semantics is safe: enabled+standard-safety); do
NOT crash.

Semantics (the frozen behavior):

| access | tools/list | tools/call |
| --- | --- | --- |
| `enabled` | visible | normal pipeline |
| `disabled` | HIDDEN (not listed) | rejected BEFORE any work: tool text `"<tool>_error: disabled by policy"` |
| `ask` | visible | MANDATORY ASK: always routed to the existing Engine pending queue (session allow map honored; timeout ⇒ deny "timeout"; user decision final) — independent of engine verdict |

`safety` switch: when `false`, skip the engine/secret-path gates for that
tool even in `RYNS_SAFETY_MODE=enforce`; when `true`, run current gate
behavior (shell tools → sidecar checkCommand; file tools → secret-path;
tools without extractable command/path simply pass). Effective safety =
`policy.safety && globalMode != off`. Observe mode: engine audits but does
not block (unchanged), regardless of per-tool switch (still respects the
ask/access dimension).

Implementation strategy: centralize in the tool dispatch layer where
possible (access precheck + ask routing), keep the 12 existing call-site
`gate_*` calls but gate them behind `policy.safety`. Refactor minimally —
NO re-architecture of the tool modules; a `policy guard` helper called at
each tool entry point is acceptable if the router cannot intercept.

### A3. Control API (no auth in default mode, behind middleware when token set)

All under existing axum server (same mux + auth layer as `/mcp`):

- `GET /api/config` →
  `{"version":"<crate>","port":8799,"mode":"enforce","auth":false,"tool_counts":{"enabled":N,"ask":N,"disabled":N},"workspace":"<root>","server_pid":<pid>}`
- `GET /api/tools` → `{"tools":[{"name":…,"title":…,"description":…,"access":"enabled|disabled|ask","safety":true|false,"safety_capable":true|false},…]}` all 55
  with `safety_capable=true` where the engine has real checks (the 12 gateway
  tools + any future engine hook) — GUI uses it to render the Safety switch
  meaningfully (tooltip "no checks available for this tool" when false).
- `POST /api/tools/policy` body `{"tool":"edit_file","access":"ask","safety":true}`
  → validates (known tool, known access values), writes file (O_TRUNC, 0600),
  returns the updated policy object; unknown tool → 404
  `{"error":"unknown_tool"}`; bad value → 400 `{"error":"invalid_policy"}`.
- Existing `GET /api/safety/pending | POST /api/safety/decide/{id} | GET
  /api/safety/events` UNCHANGED (the GUI Safety tab keeps working).

### A4. Parity gates that MUST stay green

- `tools/list` with DEFAULT policy = exactly today's 55 tools (order
  preserved). Existing lock test extended to build a default policy and
  assert equality.
- Off-mode outputs byte-identical (existing test) — policy defaults must not
  change their result.
- Existing auth matrix (when token set) unchanged.
- Sidecar protocol, audit JSONL shape, `/api/safety/*` shapes unchanged.

### A5. Tests to add (server)

- Policy file parse: valid/malformed/missing; default fallback; unknown tool
  ignored-with-warning.
- Disabled: hidden from tools/list; call rejected with exact text; no side
  effects (tool handler never runs).
- Ask: forced pending on "enabled engine would allow"; session-allow
  shortcut honored; timeout ⇒ deny.
- Safety:false bypasses gate on a gated tool (enforce mode, sidecar deny
  stub → command RUNS); safety:true blocks.
- API: policy POST validation (valid/404/400), /api/config + /api/tools
  shapes, no-auth 200 default, with-token 401.
- Mutated-policy-per-call: POST policy then tools/call reflects new state
  without restart.

## B. GUI contract changes (lilith)

- Keep `src-tauri/`; rebrand to `ryns-mcp-control` (`tauri.conf.json`
  productName + identifier `com.ryns.control`; window title "Ryns Control").
- Three tabs/cards: Server (start/stop/restart, status polling `/api/config`
  every 2s, autostart toggle via `tauri-plugin-autostart`), Tools (list +
  per-tool detail with Access segmented + Safety switch; optimistic update
  via `POST /api/tools/policy`; search filter; shows `safety_capable`
  tooltip), Safety (existing console).
- Server child management in Rust core: spawn bundled binary
  (`Contents/Resources/server/ryns-code-mcp`) — dev fallback: resolve
  `../ryns-code-mcp-rs/target/release/ryns-code-mcp` when the bundled copy is
  absent. Stop on app exit. Restart = kill + respawn with policy intact.
- NO token in settings by default; if `/api/config.auth==true` show a token
  field (optional future-proofing).
- The old safety-console window identity is REPLACED — the app becomes the
  control panel; ask windows still surface from the Safety tab and via SSE
  when hidden in tray (existing worker).
- tests: ≥ previous count (14+); add policy-toggle integration tests against
  a mock API (like the existing roundtrip tests) for tools list + policy
  post.

### B2. Auth token settable from the GUI (v1.1)

- The Settings modal exposes the existing `auth_token` field (password input
  + visibility toggle). Empty = auth off (bare loopback mode); non-empty =
  token armed on next server spawn (value passed verbatim to
  `RYNS_AUTH_TOKEN`, so comma-separated multi-token strings work).
- **Apply semantics (the v1.1 addition):** on Save, if `auth_token` changed
  and the managed server is running, the GUI automatically restarts the
  server child (existing `restart_server` command / `transition_lock`) so the
  new token takes effect immediately; toast reports the restart. If the
  server is stopped, the token applies on next Start.
- Token is persisted only in the GUI settings file (0600, existing
  `save_settings`); it is never logged, never returned by any API, and the
  server child receives it only via `configure_server_command` (parent env
  still stripped).
- GUI HTTP clients (`client.rs`, `sse.rs`, `preflight_probe`) authenticate
  with the **first non-empty comma entry** of the stored token (mirroring
  `config.rs` load semantics); the full verbatim value still goes to the
  child env so the server accepts any listed token.
- If the port is owned by an **external** server (`is_external`), the GUI
  cannot restart it: Save still persists the token, but the GUI warns that
  the external server must be restarted manually and never claims the
  change is applied.
- No server-side changes: the server keeps reading `RYNS_AUTH_TOKEN` at
  startup; `/api/config` keeps exposing only the boolean.

## C. Frozen cross-cutting decisions

- Per-request policy read (no watcher); JSON 0600; atomic write via temp
  file + rename.
- GUI never shells out policy changes; only the HTTP API.
- App bundle name `ryns-mcp-control`, display "Ryns Control", identifier
  `com.ryns.control`; the OLD `/Applications/ryns-safety-gui.app` is
  deleted at cutover by the orchestrator.
- The bundled sidecar folder includes `node_modules` (copied from repo
  after npm install; it exists). Engine default for an app-spawned server
  comes from `RYNS_SIDECAR_SCRIPT` env (GUI sets it) — repo-relative default
  only as fallback.
- Node ≥22 requirement applies to the machine for sidecar; if node missing,
  GUI shows a warning card; engine still fail-closed.

## D. Out of scope (v1)

- Global hotkey, notification taps, DMG/notarized installer, other-platform
  .msi/.deb packaging.
- Per-tool ARGUMENT-level rules (e.g., allow edit_file only under
  workspace) — record as follow-up.
- Multi-server (only the one bundled MCP server).

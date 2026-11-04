# ryns-code-mcp

An [MCP](https://modelcontextprotocol.io) server that lets an AI assistant work
on your Linux, macOS or Windows machine: files, search, shell commands,
long-running processes, git, screenshots, simple local spreadsheets, and a
pre-execution **safety layer** (policy engine + human approval queue).

The primary implementation is the **Rust server** (`ryns-code-mcp-rs/`,
cutover complete). The original Go server is kept as a frozen reference under
`legacy-go/` (launched with `./run -go`); see `docs/PLAN_RUST_PORT.md` for the
port plan.

> ## ⚠️ Security warning
>
> This server gives whoever can talk to it **full control of the machine it runs
> on**: arbitrary shell commands (`execute_command`), reading/writing/deleting
> files, screenshots, clipboard, keyboard and mouse control, camera and screen
> recording.
>
> HTTP mode is localhost-only and bearer-OPTIONAL: with `RYNS_AUTH_TOKEN`
> unset the server serves loopback traffic without auth and hard-rejects
> `Host` spoofing and any `Origin` header (browser pages cannot touch it);
> with the variable set to a comma-separated list of tokens, `Authorization:
> Bearer <token>` (scheme match case-insensitive) or `X-RYNS-Auth`
> authenticates every HTTP route (`/mcp`, `/assets/`, `/api/save/`,
> `/api/*`). Stdio mode never checks auth. In the desktop app the token is
> set/cleared from Settings (the bundled server restarts automatically to
> apply it). Even with a token set, prefer
> putting real authentication (Cloudflare Access or an authenticating
> proxy) in front of the tunnel as well; a single shared token is a low
> bar. Never expose the server directly to the internet. Use at your own
> risk.

## Tools

| Area | Tools |
| --- | --- |
| Shell & processes | `execute_command`, `start_process`, `process_output`, `send_input`, `list_processes`, `kill_process` |
| Files | `read_file`, `write_file`, `edit_file`, `multi_edit_file`, `apply_patch`, `list_directory`, `directory_tree`, `search_files`, `grep`, `create_directory`, `delete_file`, `move_file`, `get_file_info` |
| Desktop | `screenshot`, `ocr_screenshot`, `clipboard_get`, `clipboard_set`, `mouse_click`, `keyboard_type`, `key_press`, `get_active_window`, `list_windows`, `notify_send`, `camera_capture`, `record_video` |
| Git | `git_status`, `git_diff`, `git_log`, `git_branch` |
| Local sheets | `sheet_create`, `sheet_list`, `sheet_get`, `sheet_delete`, `sheet_rename`, `column_add`, `column_remove`, `column_rename`, `row_add`, `row_remove`, `cell_set`, `cell_get`, `sheet_url` |
| Misc | `workspace_info`, `system_stats`, `todo_write`, `todo_read`, `lint_format`, `check_gold_price`, `acp_opencode` |

Some tools depend on external programs (screenshots, OCR, input control, camera
and recording, `opencode`, linters). They fail gracefully when those are missing.

## Platform support

The server builds and runs on Linux, macOS and Windows. File, shell, process,
git and sheet tools are identical on Linux and macOS; on Windows core tools
work but shell commands run through `powershell.exe -NoProfile -NonInteractive
-ExecutionPolicy Bypass -Command ...` instead of bash. Desktop tools use
native utilities per OS:

| Tool(s) | Linux | macOS | Windows |
| --- | --- | --- | --- |
| `screenshot`, `ocr_screenshot` | `import` (ImageMagick), `tesseract` | `screencapture`, `tesseract` (`brew install tesseract`) | not supported |
| `clipboard_get`, `clipboard_set` | `xclip` | `pbpaste`, `pbcopy` | not supported |
| `keyboard_type`, `key_press` | `xdotool` | `osascript` (System Events) | not supported |
| `mouse_click` | `xdotool` | `cliclick` (`brew install cliclick`) | not supported |
| `get_active_window`, `list_windows` | `xdotool` | `osascript` (System Events) | not supported |
| `notify_send` | `notify-send` | `osascript` | not supported |
| `system_stats` | `/proc`, `free`, `upower`, `sensors` | `sysctl`, `vm_stat`, `pmset`, `system_profiler` (temperature needs `osx-cpu-temp`) | not supported |
| `camera_capture`, `record_video` | `ffmpeg` (v4l2, x11grab) | not supported yet | not supported |

The Windows-unsupported tools (`screenshot`, `ocr_screenshot`,
`clipboard_get`, `clipboard_set`, `mouse_click`, `keyboard_type`, `key_press`,
`get_active_window`, `list_windows`, `notify_send`, `camera_capture`,
`record_video`, `system_stats`) return a clear
`"<tool>: not supported on windows"` error instead of failing mysteriously.
Also note `start_process` has no PTY support on Windows: interactive requests
fall back to a non-interactive PowerShell process and the result reports
`"interactive": false` with an explanatory note. `kill_process` on Windows
ignores the signal name and kills the process. Windows paths were verified by
cross-compilation only, not on a real Windows machine.

Notes for macOS:

- Grant the app that launches the server (Terminal, your MCP client, ...)
  **Screen Recording** (screenshots) and **Accessibility** (keyboard, windows)
  in System Settings → Privacy & Security.
- `brew install ripgrep` is required for `search_files` and `grep`. Homebrew's
  `bin` directories are added to `PATH` automatically.
- The `window` argument of `screenshot` is a CGWindowID on macOS (an X window id
  on Linux). `get_active_window` reports the process id instead of a window id.
- The macOS code paths are cross-compiled and unit tested for the parsing logic,
  but have not been exercised on real Apple hardware yet. Issues and PRs are welcome.

## Build and run

The primary server is Rust (see `ryns-code-mcp-rs/README.md` for crate details);
the simplest entrypoint is the `./run` launcher (bash, loads a local `.env`):

```bash
./run                                  # stdio (default) — MCP client launches it
./run --port 8799                      # HTTP mode on 127.0.0.1 (auth optional, see above)
./run --status / --restart / --version
./run -go                              # escape hatch: the legacy Go server
```

The launcher builds the Rust binary for you (`cargo build`, debug — an
existing release binary under `ryns-code-mcp-rs/target/release/` is preferred).

Direct cargo workflow:

```bash
cd ryns-code-mcp-rs
cargo build                            # debug
cargo run                              # stdio mode
RYNS_AUTH_TOKEN=t cargo run -- -port 8799   # HTTP mode (token => middleware arms)
```

Safety layer deploy step (one-time, enables the policy sidecar):

```bash
cd scripts/safety-sidecar && npm install
```

Desktop control panel "Ryns Control" (Tauri, minimize-to-tray):

```bash
cargo run --manifest-path src-tauri/Cargo.toml
```

The app is an MCP control panel: **Start/Stop/Restart** of the bundled MCP
server (the server runs as the app's child), **Launch at login** toggle
(auto-start on boot), a **Tools tab** listing all 55 tools with per-tool
**Access = Enabled / Ask / Disabled** and a per-tool **Safety on/off**
switch (persisted server-side in `<workspace>/.state/tool-policy.json` —
disabled tools vanish from `tools/list` and their calls are rejected; Ask
routes every call through the safety queue first), plus a **Safety tab**
with the live ask/allow/deny console. The **Settings modal** (⌘,) holds the
server port/binary, the safety mode, and the **optional auth token** — saving
a changed token restarts the bundled server so it takes effect immediately
(empty field = clear → loopback-only, no auth; the token is stored in the
app's 0600 settings file and never logged). The installed `.app` bundles the
server binary and the safety sidecar — it does not depend on the repo
checkout. Closing the window hides it to the system tray (the app keeps
running); click the tray icon to bring it back, or use the tray menu
("Open Ryns Control" / "Quit"). Requires `libayatana-appindicator3` on
Linux desktops and Node ≥22 on the host for the sidecar.

In HTTP mode the server also serves `/assets/` (static files from
`<workspace>/assets`, including recordings made by `record_video`),
`/api/save/<name>` (used by the bundled sheet viewer), and
`/api/safety/*`, `/api/config`, `/api/tools`, `/api/tools/policy`
(control-panel API). All routes sit behind optional bearer-token
middleware — see [Gotchas](#gotchas).

## Gotchas

With `RYNS_AUTH_TOKEN` set, plain browser access to the asset routes is
blocked: links returned by `sheet_url` and `record_video` (for example
`<RYNS_PUBLIC_BASE_URL>/assets/sheet.html?id=…`) cannot be opened directly
in a browser, because there is no way to attach the `Authorization:
Bearer …` / `X-RYNS-Auth` header, and the sheet viewer's save button (POST
to `/api/save/`) gets a 401 the same way. Header-only auth for browsers
(signed or expiring links) is not built — front the tunnel with
browser-capable authentication instead. Without the token (default local
install) browsers can open asset links, but the control API still rejects
cross-origin/Host-spoofed requests.

## Connecting to Claude, ChatGPT or other remote clients

Clients that run in the cloud (Claude custom connectors, ChatGPT connectors, and
similar) cannot reach `127.0.0.1` on your machine. They need a public HTTPS URL
that points at the server's Streamable HTTP endpoint. To get one, run the server
in HTTP mode and forward the port with a tunnel such as
[ngrok](https://ngrok.com), [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/)
(`cloudflared`), or a similar service (Tailscale Funnel, localtunnel, your own
reverse proxy, ...).

1. Start the server:

   ```bash
   ./ryns-code-mcp --port 8799
   ```

2. Forward the port with the tunnel of your choice:

   ```bash
   # ngrok
   ngrok http 8799

   # Cloudflare quick tunnel (temporary URL, no account needed)
   cloudflared tunnel --url http://127.0.0.1:8799
   ```

   For a stable hostname, use a named Cloudflare tunnel or a reserved ngrok
   domain instead of a temporary URL.

3. Add the tunnel URL **with `/mcp` at the end** as a remote MCP server /
   connector in your client, for example `https://your-tunnel.example.com/mcp`.

4. Set `RYNS_PUBLIC_BASE_URL` to the tunnel URL (without `/mcp`) so links
   returned by `sheet_url` and `record_video` work from outside.

> **Before you do this, protect the URL.** Anyone who has the tunnel URL can use
> every tool on your machine. Set `RYNS_AUTH_TOKEN` and make sure your client
> sends it as `Authorization: Bearer <token>` (via `--headers`/HTTP header
> options) or `X-RYNS-Auth: <token>` — with no token set the server runs
> auth-free and is meant for loopback-only use. Check which method your client
> supports; if it cannot send headers, put authentication in front of the
> tunnel instead (Cloudflare Access, an authenticating reverse proxy, or your
> tunnel provider's auth features), do not share the URL, and shut the tunnel
> down when you are not using it.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `RYNS_WORKSPACE_ROOT` | `~/RynsWorkspace` | Directory the tools operate in |
| `RYNS_PUBLIC_BASE_URL` | `http://127.0.0.1:8799` | Base URL used in links returned by `sheet_url` and `record_video` |
| `RYNS_AUTH_TOKEN` | *(empty)* | Comma-separated bearer tokens; when set, every HTTP route requires them (`Authorization: Bearer`/`X-RYNS-Auth`), constant-time, fail-closed. Unset → loopback serve without auth, Host/Origin hard-reject on all routes. Stdio ignores it. The desktop app sets this from **Settings** (stored 0600 in its own config file; changing it restarts the bundled server automatically; empty = clear); the panel's own HTTP calls authenticate with the first comma entry |
| `RYNS_SAFETY_MODE` | `enforce` | Safety layer: `enforce` (violations block → human ASK, timeout ⇒ deny), `observe` (log only, everything runs), `off` (zero checks, still audited). Per-tool Safety switches in the app override per tool |
| `RYNS_ASK_TIMEOUT_MS` | `120000` | Pending-approval expiry (timeout ⇒ deny with reason `timeout`) |
| `RYNS_NODE_BIN` | `node` | Node binary used to run the safety sidecar (Node ≥ 22) |
| `RYNS_SIDECAR_SCRIPT` | `scripts/safety-sidecar/index.mjs` | Sidecar script path (relative defaults anchor to the launcher CWD; needs `npm install` in that folder once); the GUI bundles and points at its own copy |

Per-tool policy lives in `<workspace>/.state/tool-policy.json` (written by
the app; edit by hand if you like: `{"tool":{"access":"enabled|disabled|ask","safety":true|false}}`).

Copy `.env.example` to `.env` to set these locally.

## Repository layout

- `ryns-code-mcp-rs/` is the primary server (Rust; crate `ryns-code-mcp`).
- `legacy-go/` holds the frozen Go implementation (launched via `./run -go`;
  `go.mod`/`go.sum` stay at the repo root — `go vet ./...` / `go test ./...`
  still cover it from the root).
- `scripts/safety-sidecar/` ships the Node policy sidecar bridge.
- `src-tauri/` is the desktop GUI (Phase 4).
- `docs/` holds the design notes written while adding each feature.
- `src/index.ts` is an earlier TypeScript prototype with a smaller tool set. It
  is kept for reference and is no longer maintained.

## License

MIT, see [LICENSE](LICENSE).

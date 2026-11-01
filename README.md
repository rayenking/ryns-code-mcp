# ryns-code-mcp

An [MCP](https://modelcontextprotocol.io) server, written in Go, that lets an AI
assistant work on your Linux machine: files, search, shell commands,
long-running processes, git, screenshots, simple local spreadsheets, and more.

> ## ⚠️ Security warning
>
> This server gives whoever can talk to it **full control of the machine it runs
> on**: arbitrary shell commands (`execute_command`), reading/writing/deleting
> files, screenshots, clipboard, keyboard and mouse control, camera and screen
> recording.
>
> **It has no authentication of its own.** Never expose it directly to the
> internet. If you serve it over HTTP through a tunnel or reverse proxy, put real
> authentication in front of it (for example Cloudflare Access or an
> authenticating proxy). Stdio mode is the safe default. Use at your own risk.

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

## Build and run

Requires Go 1.25+.

```bash
go build -o ryns-code-mcp .

# stdio (default): launch it from your MCP client
./ryns-code-mcp

# Streamable HTTP on 127.0.0.1:8799 (for use behind an authenticated tunnel)
./ryns-code-mcp --port 8799
```

`./run` is a small wrapper that builds into `.state/`, tracks state, and loads
a local `.env`. See `./run --help`.

In HTTP mode the server also serves `/assets/` (static files from
`<workspace>/assets`, including recordings made by `record_video`) and
`/api/save/<name>` (used by the bundled sheet viewer). Treat both as public once
the server is reachable.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `RYNS_WORKSPACE_ROOT` | `~/RynsWorkspace` | Directory the tools operate in |
| `RYNS_PUBLIC_BASE_URL` | `http://127.0.0.1:8799` | Base URL used in links returned by `sheet_url` and `record_video` |

Copy `.env.example` to `.env` to set these locally.

## Repository layout

- `main.go` is the current implementation.
- `docs/` holds the design notes written while adding each feature.
- `src/index.ts` is an earlier TypeScript prototype with a smaller tool set. It
  is kept for reference and is no longer maintained.

## License

MIT, see [LICENSE](LICENSE).

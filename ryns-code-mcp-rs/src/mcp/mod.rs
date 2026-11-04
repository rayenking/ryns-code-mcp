//! rmcp wiring: the server handler with the full P2 tool set (mirrors Go
//! `setupTools` + `mcp.NewServer`), plus the transports: stdio (default) and
//! Streamable HTTP with the Go route set (`/mcp` + `/assets/` + `/api/save/`,
//! all behind bearer auth).
//!
//! serverInfo name/version are pinned by Cargo.toml (`ryns-code-mcp`,
//! `1.0.0`) — the contract in docs/PLAN_RUST_PORT.md §2.

use std::sync::Arc;

use axum::extract::{Request as AxumRequest, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde_json::json;

use crate::auth::{self, AuthState};
use crate::policy::{self, Access};
use crate::safety;
use crate::safety::GateResult;
use crate::tools::{self, Ctx, ToolOut};

/// State for the HTTP-mode router: workspace root, safety engine, listen
/// port and whether bearer auth is armed (§A1 auth is now optional) — one
/// `with_state` covers the whole router (tools/assets/save/safety/config).
#[derive(Clone)]
pub struct AppState {
    pub workspace: Arc<str>,
    pub engine: Arc<safety::Engine>,
    pub port: u16,
    pub auth_enabled: bool,
}

// §A2 dispatch marker (edison major: single-ASK): the approved tool name
// for the CURRENT tool invocation, set after its forced_ask Allow and read
// by gate_shell/gate_paths through Ctx (fail direction = not set ⇒ a gate
// re-asks). Task-scoped: no cross-call leakage with concurrent dispatches.
tokio::task_local! {
    static APPROVED_TOOL: Option<String>;
}

#[derive(Clone)]
pub struct RynsServer {
    workspace: Arc<str>,
    engine: Option<Arc<crate::safety::Engine>>,
}

fn ctx_of(workspace: &Arc<str>, engine: Option<Arc<crate::safety::Engine>>) -> Ctx {
    let approved_ask = APPROVED_TOOL
        .try_with(|v| v.clone())
        .unwrap_or(None)
        .map(Arc::from);
    Ctx {
        workspace: workspace.clone(),
        safety: engine,
        approved_ask,
    }
}

/// Mirrors Go `textResult` (main.go ~172): clip the text, emit it as text
/// content AND as structured content `{"text": …}` (Go's OutputSchema
/// guarantees that shape). Image-aware variant: Go's screenshot/camera/
/// read_file handlers add an ImageContent block after the text.
fn text_result(out: ToolOut) -> CallToolResult {
    let (text, image) = match out {
        ToolOut::Text(text) => (text, None),
        ToolOut::TextPlusImage { text, mime, data } => (text, Some((mime, data))),
    };
    let clipped = tools::clip(&text);
    let mut content = vec![ContentBlock::text(clipped.clone())];
    if let Some((mime, data)) = image {
        content.push(ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(data),
            mime,
        ));
    }
    let mut result = CallToolResult::success(content);
    result.structured_content = Some(json!({"text": clipped}));
    // Go's CallToolResult leaves isError omitted (omitempty); match it.
    result.is_error = None;
    result
}

// The tool registry wiring. Each #[tool] entry mirrors one `mcp.AddTool`
// call in Go's setupTools (verbatim name/description, Go title in
// annotations); tools::REGISTRY is the manifest double-check.
#[tool_router]
impl RynsServer {
    pub fn new(workspace: Arc<str>, engine: Option<Arc<crate::safety::Engine>>) -> Self {
        Self { workspace, engine }
    }

    fn ctx(&self) -> Ctx {
        ctx_of(&self.workspace, self.engine.clone())
    }

    #[tool(
        name = "execute_command",
        description = "Run a one-shot shell command inside RynsWorkspace (bash on Linux/macOS, PowerShell on Windows). Good for git, npm, tests, builds and system inspection.",
        annotations(title = "Execute Command")
    )]
    async fn execute_command(
        &self,
        Parameters(input): Parameters<tools::execute_command::ExecuteInput>,
    ) -> CallToolResult {
        let text = tools::execute_command::run(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "start_process",
        description = "Start a long-running shell command and keep its process/session alive.",
        annotations(title = "Start Process")
    )]
    async fn start_process(
        &self,
        Parameters(input): Parameters<tools::processes::StartInput>,
    ) -> CallToolResult {
        let text = tools::processes::run_start(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "process_output",
        description = "Read accumulated stdout/stderr and current state from a long-running process.",
        annotations(title = "Process Output")
    )]
    async fn process_output(
        &self,
        Parameters(input): Parameters<tools::processes::ProcessInput>,
    ) -> CallToolResult {
        let text = tools::processes::run_output(input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "send_input",
        description = "Send stdin to a running process.",
        annotations(title = "Send Process Input")
    )]
    async fn send_input(
        &self,
        Parameters(input): Parameters<tools::processes::SendInput>,
    ) -> CallToolResult {
        let text = tools::processes::run_send(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "list_processes",
        description = "List processes started by this MCP server.",
        annotations(title = "List Processes")
    )]
    async fn list_processes(&self) -> CallToolResult {
        let text = tools::processes::run_list().await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "kill_process",
        description = "Terminate a process started by this MCP server.",
        annotations(title = "Kill Process")
    )]
    async fn kill_process(
        &self,
        Parameters(input): Parameters<tools::processes::KillInput>,
    ) -> CallToolResult {
        let text = tools::processes::run_kill(input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "read_file",
        description = "Read a file. Text is returned with cat -n style line numbers (scope it with start_line/end_line for big files; unscoped reads are capped around 2000 lines). Common image formats (png/jpg/jpeg/gif/webp/bmp) are returned as an inline image instead of text.",
        annotations(title = "Read File")
    )]
    async fn read_file(
        &self,
        Parameters(input): Parameters<tools::read_file::ReadInput>,
    ) -> CallToolResult {
        text_result(tools::read_file::run(&self.ctx(), input).await)
    }

    #[tool(
        name = "write_file",
        description = "Create or overwrite a UTF-8 text file. Reports whether it created a new file or overwrote an existing one, with byte/line counts.",
        annotations(title = "Write File")
    )]
    async fn write_file(
        &self,
        Parameters(input): Parameters<tools::write_file::WriteInput>,
    ) -> CallToolResult {
        let text = tools::write_file::run(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "edit_file",
        description = "Replace exact text in a file; refuses ambiguous matches unless replace_all is enabled. Returns a numbered context snippet around the change so you can confirm it without re-reading the file.",
        annotations(title = "Edit File")
    )]
    async fn edit_file(
        &self,
        Parameters(input): Parameters<tools::files::EditInput>,
    ) -> CallToolResult {
        let text = tools::files::edit_file(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "multi_edit_file",
        description = "Apply several exact-text edits to one file atomically and in order: either every edit matches and the file is written once, or nothing is written and the failing edit is reported.",
        annotations(title = "Multi Edit File")
    )]
    async fn multi_edit_file(
        &self,
        Parameters(input): Parameters<tools::files::MultiEditInput>,
    ) -> CallToolResult {
        let text = tools::files::multi_edit_file(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "apply_patch",
        description = "Apply a unified diff patch to workspace files using the system patch utility.",
        annotations(title = "Apply Patch")
    )]
    async fn apply_patch(
        &self,
        Parameters(input): Parameters<tools::files::PatchInput>,
    ) -> CallToolResult {
        let text = tools::files::apply_patch(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "list_directory",
        description = "List directory entries with type and size.",
        annotations(title = "List Directory")
    )]
    async fn list_directory(
        &self,
        Parameters(input): Parameters<tools::list_directory::ListInput>,
    ) -> CallToolResult {
        let text = tools::list_directory::run(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "directory_tree",
        description = "Return a recursive workspace tree.",
        annotations(title = "Directory Tree")
    )]
    async fn directory_tree(
        &self,
        Parameters(input): Parameters<tools::files::TreeInput>,
    ) -> CallToolResult {
        let text = tools::files::directory_tree(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "search_files",
        description = "Find files using ripgrep glob patterns.",
        annotations(title = "Search Files")
    )]
    async fn search_files(
        &self,
        Parameters(input): Parameters<tools::files::SearchInput>,
    ) -> CallToolResult {
        let text = tools::files::search_files(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "grep",
        description = "Search file contents recursively with ripgrep regex, glob, case and context support.",
        annotations(title = "Grep")
    )]
    async fn grep(&self, Parameters(input): Parameters<tools::files::GrepInput>) -> CallToolResult {
        let text = tools::files::grep(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "create_directory",
        description = "Create a directory recursively.",
        annotations(title = "Create Directory")
    )]
    async fn create_directory(
        &self,
        Parameters(input): Parameters<tools::files::MkdirInput>,
    ) -> CallToolResult {
        let text = tools::files::create_directory(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "delete_file",
        description = "Delete a file or directory; recursive deletion must be explicit.",
        annotations(title = "Delete File")
    )]
    async fn delete_file(
        &self,
        Parameters(input): Parameters<tools::files::DeleteInput>,
    ) -> CallToolResult {
        let text = tools::files::delete_file(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "move_file",
        description = "Move or rename a file/directory inside the workspace.",
        annotations(title = "Move File")
    )]
    async fn move_file(
        &self,
        Parameters(input): Parameters<tools::files::MoveInput>,
    ) -> CallToolResult {
        let text = tools::files::move_file(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "get_file_info",
        description = "Return file/directory metadata.",
        annotations(title = "Get File Info")
    )]
    async fn get_file_info(
        &self,
        Parameters(input): Parameters<tools::files::StatInput>,
    ) -> CallToolResult {
        let text = tools::files::get_file_info(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "screenshot",
        description = "Capture the current screen (or a specific X window) as a PNG image, returned inline. Optionally also saves a copy to disk.",
        annotations(title = "Screenshot")
    )]
    async fn screenshot(
        &self,
        Parameters(input): Parameters<tools::desktop::ScreenshotInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::screenshot(&self.ctx(), input).await)
    }

    #[tool(
        name = "clipboard_get",
        description = "Read the current text on the X clipboard.",
        annotations(title = "Clipboard Get")
    )]
    async fn clipboard_get(
        &self,
        Parameters(input): Parameters<tools::desktop::NoInput>,
    ) -> CallToolResult {
        let _ = &self;
        text_result(tools::desktop::clipboard_get(input).await)
    }

    #[tool(
        name = "clipboard_set",
        description = "Set the X clipboard to the given text.",
        annotations(title = "Clipboard Set")
    )]
    async fn clipboard_set(
        &self,
        Parameters(input): Parameters<tools::desktop::ClipboardSetInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::clipboard_set(input).await)
    }

    #[tool(
        name = "mouse_click",
        description = "Move the mouse to (x,y) and click. button: 1=left, 2=middle, 3=right.",
        annotations(title = "Mouse Click")
    )]
    async fn mouse_click(
        &self,
        Parameters(input): Parameters<tools::desktop::ClickInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::mouse_click(input).await)
    }

    #[tool(
        name = "keyboard_type",
        description = "Type literal text at the current input focus, as if typed on the keyboard.",
        annotations(title = "Keyboard Type")
    )]
    async fn keyboard_type(
        &self,
        Parameters(input): Parameters<tools::desktop::TypeTextInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::keyboard_type(input).await)
    }

    #[tool(
        name = "key_press",
        description = "Send a key or key combo using xdotool key syntax, e.g. 'Return', 'ctrl+c', 'alt+Tab', 'super'.",
        annotations(title = "Key Press")
    )]
    async fn key_press(
        &self,
        Parameters(input): Parameters<tools::desktop::KeyPressInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::key_press(input).await)
    }

    #[tool(
        name = "get_active_window",
        description = "Return the id, title, and window class of the currently focused window.",
        annotations(title = "Get Active Window")
    )]
    async fn get_active_window(
        &self,
        Parameters(input): Parameters<tools::desktop::NoInput>,
    ) -> CallToolResult {
        let _ = &self;
        text_result(tools::desktop::get_active_window(input).await)
    }

    #[tool(
        name = "list_windows",
        description = "List visible windows with their id and title.",
        annotations(title = "List Windows")
    )]
    async fn list_windows(
        &self,
        Parameters(input): Parameters<tools::desktop::NoInput>,
    ) -> CallToolResult {
        let _ = &self;
        text_result(tools::desktop::list_windows(input).await)
    }

    #[tool(
        name = "notify_send",
        description = "Show a desktop notification popup.",
        annotations(title = "Send Notification")
    )]
    async fn notify_send(
        &self,
        Parameters(input): Parameters<tools::desktop::NotifyInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::notify_send(input).await)
    }

    #[tool(
        name = "ocr_screenshot",
        description = "Capture the screen (or a specific window) and run OCR to extract any visible text.",
        annotations(title = "OCR Screenshot")
    )]
    async fn ocr_screenshot(
        &self,
        Parameters(input): Parameters<tools::desktop::OcrInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::ocr_screenshot(input).await)
    }

    #[tool(
        name = "camera_capture",
        description = "Capture a single JPEG photo from the webcam and return it inline. Optionally also saves a copy to disk.",
        annotations(title = "Camera Capture")
    )]
    async fn camera_capture(
        &self,
        Parameters(input): Parameters<tools::desktop::CameraInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::camera_capture(&self.ctx(), input).await)
    }

    #[tool(
        name = "record_video",
        description = "Record the screen or the webcam for a set number of seconds (max 120), save it as an MP4 under the public assets/ folder, and return its public URL — no video bytes are sent back through the tool result, just the link.",
        annotations(title = "Record Video")
    )]
    async fn record_video(
        &self,
        Parameters(input): Parameters<tools::desktop::RecordInput>,
    ) -> CallToolResult {
        text_result(tools::desktop::record_video(&self.ctx(), input).await)
    }

    // The four git tools mirror Go `registerGit` — fixed `git <args>` runs,
    // direct `git` binary (not shellCommand), childEnv, cwd-resolvable.
    #[tool(
        name = "git_status",
        description = "Show git status and branch.",
        annotations(title = "git status")
    )]
    async fn git_status(
        &self,
        Parameters(input): Parameters<tools::git::GitInput>,
    ) -> CallToolResult {
        text_result(ToolOut::Text(
            tools::git::run(&self.ctx(), tools::git::STATUS_ARGS, input).await,
        ))
    }

    #[tool(
        name = "git_diff",
        description = "Show working-tree diff.",
        annotations(title = "git diff")
    )]
    async fn git_diff(
        &self,
        Parameters(input): Parameters<tools::git::GitInput>,
    ) -> CallToolResult {
        text_result(ToolOut::Text(
            tools::git::run(&self.ctx(), tools::git::DIFF_ARGS, input).await,
        ))
    }

    #[tool(
        name = "git_log",
        description = "Show recent commits.",
        annotations(title = "git log")
    )]
    async fn git_log(&self, Parameters(input): Parameters<tools::git::GitInput>) -> CallToolResult {
        text_result(ToolOut::Text(
            tools::git::run(&self.ctx(), tools::git::LOG_ARGS, input).await,
        ))
    }

    #[tool(
        name = "git_branch",
        description = "Show branches and tracking info.",
        annotations(title = "git branch")
    )]
    async fn git_branch(
        &self,
        Parameters(input): Parameters<tools::git::GitInput>,
    ) -> CallToolResult {
        text_result(ToolOut::Text(
            tools::git::run(&self.ctx(), tools::git::BRANCH_ARGS, input).await,
        ))
    }

    #[tool(
        name = "workspace_info",
        description = "Show workspace and runtime information.",
        annotations(title = "Workspace Info")
    )]
    async fn workspace_info(
        &self,
        Parameters(_): Parameters<tools::misc::EmptyInput>,
    ) -> CallToolResult {
        let text = tools::misc::workspace_info(&self.ctx()).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_create",
        description = "Create a new local sheet and register it in the sheet index. Fails if a sheet with the same id already exists unless overwrite is set.",
        annotations(title = "Sheet Create")
    )]
    async fn sheet_create(
        &self,
        Parameters(input): Parameters<tools::sheets::SheetCreateInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_create(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_list",
        description = "List every sheet in the index with its id, title, row count, and column count.",
        annotations(title = "Sheet List")
    )]
    async fn sheet_list(
        &self,
        Parameters(_): Parameters<tools::sheets::SheetListInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_list(&self.ctx()).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_get",
        description = "Show the contents of a sheet as a readable text table, optionally scoped to a row range for large sheets.",
        annotations(title = "Sheet Get")
    )]
    async fn sheet_get(
        &self,
        Parameters(input): Parameters<tools::sheets::SheetGetInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_get(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_delete",
        description = "Delete a sheet's underlying file and remove its entry from the index.",
        annotations(title = "Sheet Delete")
    )]
    async fn sheet_delete(
        &self,
        Parameters(input): Parameters<tools::sheets::SheetIdInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_delete(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_rename",
        description = "Change a sheet's display title while keeping its id and data unchanged.",
        annotations(title = "Sheet Rename")
    )]
    async fn sheet_rename(
        &self,
        Parameters(input): Parameters<tools::sheets::SheetRenameInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_rename(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "column_add",
        description = "Add a new column to a sheet. When after_column is given, the new column is inserted right after that column instead of at the end, and every existing row gains an empty value in the new position.",
        annotations(title = "Column Add")
    )]
    async fn column_add(
        &self,
        Parameters(input): Parameters<tools::sheets::ColumnAddInput>,
    ) -> CallToolResult {
        let text = tools::sheets::column_add(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "column_remove",
        description = "Remove a column from a sheet by name or index, dropping that value from every row.",
        annotations(title = "Column Remove")
    )]
    async fn column_remove(
        &self,
        Parameters(input): Parameters<tools::sheets::ColumnRemoveInput>,
    ) -> CallToolResult {
        let text = tools::sheets::column_remove(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "column_rename",
        description = "Rename a column in a sheet, identified by its current name or index.",
        annotations(title = "Column Rename")
    )]
    async fn column_rename(
        &self,
        Parameters(input): Parameters<tools::sheets::ColumnRenameInput>,
    ) -> CallToolResult {
        let text = tools::sheets::column_rename(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "row_add",
        description = "Add a new row of values to a sheet, appended at the end by default or inserted at a given zero based position.",
        annotations(title = "Row Add")
    )]
    async fn row_add(
        &self,
        Parameters(input): Parameters<tools::sheets::RowAddInput>,
    ) -> CallToolResult {
        let text = tools::sheets::row_add(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "row_remove",
        description = "Remove a row from a sheet by its zero based index.",
        annotations(title = "Row Remove")
    )]
    async fn row_remove(
        &self,
        Parameters(input): Parameters<tools::sheets::RowRemoveInput>,
    ) -> CallToolResult {
        let text = tools::sheets::row_remove(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "cell_set",
        description = "Set the value of a single cell in a sheet, identified by row index and column name or index.",
        annotations(title = "Cell Set")
    )]
    async fn cell_set(
        &self,
        Parameters(input): Parameters<tools::sheets::CellSetInput>,
    ) -> CallToolResult {
        let text = tools::sheets::cell_set(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "cell_get",
        description = "Read the value of a single cell in a sheet, identified by row index and column name or index.",
        annotations(title = "Cell Get")
    )]
    async fn cell_get(
        &self,
        Parameters(input): Parameters<tools::sheets::CellGetInput>,
    ) -> CallToolResult {
        let text = tools::sheets::cell_get(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "sheet_url",
        description = "Return the public URL where a sheet can be opened and edited in a browser.",
        annotations(title = "Sheet URL")
    )]
    async fn sheet_url(
        &self,
        Parameters(input): Parameters<tools::sheets::SheetIdInput>,
    ) -> CallToolResult {
        let text = tools::sheets::sheet_url(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "check_gold_price",
        description = "Fetch today's official Antam Logam Mulia gold price (24K) plus an estimated 22K conversion, sourced live from logammulia.com.",
        annotations(title = "Check Gold Price")
    )]
    async fn check_gold_price(
        &self,
        Parameters(input): Parameters<tools::misc::EmptyInput>,
    ) -> CallToolResult {
        let _ = &self;
        let text = tools::misc::check_gold_price(input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "todo_write",
        description = "Replace the entire todo checklist with the given list of items. Call this with the complete, updated list every time progress changes, mirroring how Claude Code's TodoWrite tracks multi-step work.",
        annotations(title = "Todo Write")
    )]
    async fn todo_write(
        &self,
        Parameters(input): Parameters<tools::todos::TodoWriteInput>,
    ) -> CallToolResult {
        let text = tools::todos::todo_write(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "todo_read",
        description = "Show the current todo checklist without modifying it.",
        annotations(title = "Todo Read")
    )]
    async fn todo_read(
        &self,
        Parameters(_): Parameters<tools::todos::TodoReadInput>,
    ) -> CallToolResult {
        let text = tools::todos::todo_read(&self.ctx()).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "lint_format",
        description = "Check or auto-fix code formatting for a file or directory. Supports Go via gofmt, Rust via rustfmt, JS/TS/JSON/CSS/Markdown via prettier (only if the project already has it installed), and a Python syntax check (formatting not supported since no Python formatter is installed). Set fix=true to apply changes in place; default only reports what would change.",
        annotations(title = "Lint Format")
    )]
    async fn lint_format(
        &self,
        Parameters(input): Parameters<tools::misc::LintFormatInput>,
    ) -> CallToolResult {
        let text = tools::misc::lint_format(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "system_stats",
        description = "Show a snapshot of the machine's health: CPU core count and load average, RAM and swap usage, root disk usage, battery status/health, and temperatures when available.",
        annotations(title = "System Stats")
    )]
    async fn system_stats(
        &self,
        Parameters(input): Parameters<tools::misc::EmptyInput>,
    ) -> CallToolResult {
        let text = tools::misc::system_stats(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }

    #[tool(
        name = "acp_opencode",
        description = "Delegate a coding task to OpenCode as a sub-agent via the Agent Client Protocol (opencode acp). Spawns opencode, sends one prompt, waits for it to finish (auto-approving any permission prompts, since this is meant for autonomous delegation), and returns its final text response.",
        annotations(title = "ACP OpenCode")
    )]
    async fn acp_opencode(
        &self,
        Parameters(input): Parameters<tools::acp::AcpOpenCodeInput>,
    ) -> CallToolResult {
        let text = tools::acp::acp_opencode(&self.ctx(), input).await;
        text_result(ToolOut::Text(text))
    }
}

// Name/version pinned explicitly to the contract (docs/PLAN_RUST_PORT.md §2
// `1.0.0`): the macro's defaults resolve to the rmcp crate's own
// identity, which is wrong for this server. The version must stay a string
// LITERAL (the macro rejects a const path), so `handler_version_tracks_manifest`
// below is the drift guard: bumping CARGO_PKG_VERSION without updating this
// literal fails that test.
#[tool_handler(name = "ryns-code-mcp", version = "1.0.0")]
impl ServerHandler for RynsServer {
    /// §A2 dispatch guard: fresh policy read on EVERY tools/call. Disabled ⇒
    /// `"<tool>_error: disabled by policy"` before any work; ask ⇒ mandatory
    /// pending through the Engine queue (session map honored, timeout⇒deny);
    /// enabled ⇒ the normal pipeline.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let name = &request.name;
        let setting = crate::policy::load(&self.workspace).setting(name);
        let outcome = match setting.access {
            Access::Enabled => GateResult::Run,
            // Disabled: rejected BEFORE any work (frozen text).
            Access::Disabled => GateResult::Blocked(format!("{name}_error: disabled by policy")),
            // Ask: forced queue, independent of the engine verdict. The
            // checked command triple is a per-tool best-effort: the
            // stringified arguments as the command, the workspace as cwd —
            // section A2 only fixes the semantics (queue, session, timeout).
            Access::Ask => {
                let args_command = arguments_command_str(name, request.arguments.as_ref());
                match self.engine_for_ask() {
                    None => GateResult::Run, // engine absent (tests): no queue possible
                    Some(engine) => {
                        engine
                            .forced_ask(name, &args_command, &self.workspace)
                            .await
                    }
                }
            }
        };
        match outcome {
            GateResult::Run => {
                let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
                Self::tool_router().call(tcc).await
            }
            GateResult::Blocked(text) => Ok(text_result(ToolOut::Text(text)).into()),
        }
    }

    /// §A2 dispatch guard: fresh policy read on EVERY tools/list; tools
    /// with access=disabled are HIDDEN (order preserved).
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        let policy = crate::policy::load(&self.workspace);
        let mut tools = Self::tool_router().list_all();
        tools.retain(|t| crate::policy::is_visible(&policy, &t.name));
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }
}

/// Per-tool ask-view command string: prefers the "command" argument (shell
/// tools), else "path"/"pattern", else the full arguments JSON — used only
/// for ask/audit display.
fn arguments_command_str(
    tool: &str,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let str_of = |k: &str| {
        args.and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    str_of("command")
        .or_else(|| str_of("path"))
        .or_else(|| str_of("source"))
        .or_else(|| str_of("patch"))
        .unwrap_or_else(|| match args {
            Some(m) => serde_json::to_string(&serde_json::Value::Object(m.clone()))
                .unwrap_or_else(|_| tool.to_string()),
            None => tool.to_string(),
        })
}

impl RynsServer {
    fn engine_for_ask(&self) -> Option<Arc<safety::Engine>> {
        self.engine.clone()
    }
}

/// MCP over stdio (Go default mode; logs already emitted by main).
pub async fn serve_stdio(workspace: Arc<str>, engine: Arc<safety::Engine>) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let server = RynsServer::new(workspace, Some(engine));
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// MCP over Streamable HTTP on an already-bound listener with the Go route
/// set: `/mcp` (Streamable HTTP), `/assets/` static files, `/api/save/`
/// write-only JSON saver — ALL behind the bearer-auth middleware (mirrors Go
/// control flow, the middleware wrapping the mux, and `Stateless: true` →
/// NeverSessionManager).
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    workspace: Arc<str>,
    engine: Arc<safety::Engine>,
    auth_state: AuthState,
    port: u16,
) -> anyhow::Result<()> {
    // §A1 order: compute auth state first — the rmcp host-validation policy
    // depends on the mode (tunnel vs bare-localhost).
    let auth_enabled = !auth_state.config.tokens.is_empty();
    // Mirrors Go's StreamableHTTPOptions{DisableLocalhostProtection: true,...
    // with the §A1 split: with a token, the server is the tunnel backend
    // (external Host headers pass, auth is the single gate); bare (no-auth)
    // mode KEEPS rmcp's default Host validation for /mcp AND adds the
    // `bare_mode_guard` axum layer for /api/* + /assets/. Never web-served.
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false) // Stateless: true in Go
        .with_json_response(true); // plain JSON (divergence note above)
    if auth_enabled {
        config = config.disable_allowed_hosts();
    }
    let service = StreamableHttpService::new(
        {
            let workspace = workspace.clone();
            let engine = engine.clone();
            move || Ok(RynsServer::new(workspace.clone(), Some(engine.clone())))
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        // Go: http.StripPrefix("/assets/", http.FileServer(http.Dir(assetsDir))).
        .route("/assets/{*asset_path}", axum::routing::get(assets_handler))
        // Go: HandleFunc("/api/save/", ...) with the method guard inside.
        // The literal trailing-slash path is registered separately because
        // axum's wildcard doesn't match the empty rest — that's where Go's
        // `invalid name` branch lives.
        .route(
            "/api/save/",
            axum::routing::post(|| async {
                go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid name\"}")
            })
            .fallback(api_save_method_fallback),
        )
        .route(
            "/api/save/{*api_path}",
            axum::routing::post(api_save_handler).fallback(api_save_method_fallback),
        )
        // Phase 3: GUI safety API (plan §6; frozen shapes). Registered on
        // the outer router (whose with_state carries the engine alongside the
        // workspace) so the parent middleware wraps them like every other
        // authed route.
        .route(
            "/api/safety/pending",
            axum::routing::get(safety_pending_handler),
        )
        .route(
            "/api/safety/decide/{id}",
            axum::routing::post(safety_decide_handler),
        )
        .route(
            "/api/safety/events",
            axum::routing::get(safety_events_handler),
        )
        // §A3 control API (same mux, same auth treatment).
        .route("/api/config", axum::routing::get(config_handler))
        .route("/api/tools", axum::routing::get(tools_handler))
        .route(
            "/api/tools/policy",
            axum::routing::post(tools_policy_handler),
        )
        // Registered before the auth layer so the fallback stays behind it
        // (mirrors Go: authMiddleware wraps buildMux, so the mux's
        // ServeMux->404 sits inside the auth gate).
        .fallback(not_found_fallback);
    // §A1+§C: arm the middleware only when a token is configured; bare
    // mode gets the Host/Origin guard instead (web-page attack defense).
    let router = if auth_enabled {
        router.layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            auth::auth_middleware,
        ))
    } else {
        router.layer(axum::middleware::from_fn_with_state(port, bare_mode_guard))
    }
    .with_state({
        AppState {
            workspace: workspace.clone(),
            engine: engine.clone(),
            port,
            auth_enabled,
        }
    });
    axum::serve(listener, router).await?;
    Ok(())
}

/// Mirrors Go `http.ServeMux`'s default NotFound inside the authed mux
/// (`http.Error(w, "404 page not found", 404)` shape): status 404,
/// `Content-Type: text/plain; charset=utf-8`, `X-Content-Type-Options:
/// nosniff`, body `404 page not found\n`.
async fn not_found_fallback() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        "404 page not found\n",
    )
}

/// §A1 bare-mode web-attack guard (no-auth HTTP mode ONLY): browsers are the
/// threat model local auth does not cover. Rejects:
/// - a `Host` header that is not exactly `127.0.0.1:<port>` or
///   `localhost:<port>` (DNS-rebinding chains),
/// - any request carrying an `Origin` header (browsers always send one on
///   cross-origin requests; Tauri's reqwest core sends none, and MCP clients
///   send none — both are unaffected). 403 `{"error":"forbidden"}`.
pub async fn bare_mode_guard(
    axum::extract::State(port): axum::extract::State<u16>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::header;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    if !bare_mode_allowed(host, origin, port) {
        return json_ok(StatusCode::FORBIDDEN, "{\"error\":\"forbidden\"}");
    }
    next.run(req).await
}

/// Pure decision core of the bare-mode guard (table-tested).
fn bare_mode_allowed(host: Option<&str>, origin: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"))
        && origin.is_none()
}

/// Shared shape of Go's `http.Error(w, jsonLit, code)` responses: text/plain
/// content type + nosniff + trailing "\n" (http.Error appends it).
fn go_http_error<T>(code: StatusCode, body: T) -> Response
where
    T: Into<String>,
{
    (
        code,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        format!("{}\n", body.into()),
    )
        .into_response()
}

/// JSON 2xx response (Content-Type application/json, body exactly as given).
fn json_ok<T>(code: StatusCode, body: T) -> Response
where
    T: Into<String>,
{
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        body.into(),
    )
        .into_response()
}

/// §A3 `GET /api/config` — server snapshot for the GUI status card.
/// `auth` reflects §A1 middleware state; `tool_counts` come from the CURRENT
/// policy file (fresh read).
async fn config_handler(State(app): axum::extract::State<AppState>) -> Response {
    let policy = policy::load(&app.workspace);
    let mut enabled = 0usize;
    let mut ask = 0usize;
    let mut disabled = 0usize;
    for spec in tools::REGISTRY {
        match policy.setting(spec.name).access {
            policy::Access::Enabled => enabled += 1,
            policy::Access::Ask => ask += 1,
            policy::Access::Disabled => disabled += 1,
        }
    }
    let config = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "port": app.port,
        "mode": app.engine.mode().label(),
        "auth": app.auth_enabled,
        "tool_counts": { "enabled": enabled, "ask": ask, "disabled": disabled },
        "workspace": app.workspace,
        "server_pid": std::process::id(),
    });
    json_ok(
        StatusCode::OK,
        serde_json::to_string(&config).unwrap_or_else(|_| "{}".into()),
    )
}

/// §A3 `GET /api/tools` — ALL 55 registry tools with the current policy
/// values plus `safety_capable`. (Registry access always lists every tool
/// here; the MCP tools/list endpoint computes visibility separately.)
async fn tools_handler(State(app): axum::extract::State<AppState>) -> Response {
    let policy = policy::load(&app.workspace);
    let items: Vec<serde_json::Value> = tools::REGISTRY
        .iter()
        .map(|spec| {
            let name = spec.name;
            let setting = policy.setting(name);
            serde_json::json!({
                "name": name,
                "title": spec.title,
                "description": spec.description,
                "access": setting.access.label(),
                "safety": setting.safety,
                "safety_capable": policy::safety_capable(name),
            })
        })
        .collect();
    let body = format!(
        "{{\"tools\":{}}}",
        serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
    );
    json_ok(StatusCode::OK, body)
}

/// Validation outcome for a policy POST body (extracted pure for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyValidationError {
    UnknownTool,
    Invalid,
}

/// Pure validation for POST /api/tools/policy: known tool (else
/// UnknownTool), access ∈ {enabled,disabled,ask} and safety present (else
/// Invalid).
fn validate_policy_entry(
    tool: &str,
    access: Option<&str>,
    safety: Option<bool>,
) -> Result<policy::Setting, PolicyValidationError> {
    if !tools::REGISTRY.iter().any(|t| t.name == tool) {
        return Err(PolicyValidationError::UnknownTool);
    }
    let Some(access) = access.and_then(policy::Access::parse) else {
        return Err(PolicyValidationError::Invalid);
    };
    let Some(safety) = safety else {
        return Err(PolicyValidationError::Invalid);
    };
    Ok(policy::Setting { access, safety })
}

/// §A3 `POST /api/tools/policy` body `{"tool":"…","access":"enabled|
/// disabled|ask","safety":true|false}` → 200 with the updated policy object;
/// unknown tool → `404 {"error":"unknown_tool"}`; bad value →
/// `400 {"error":"invalid_policy"}`. Mutated policy is visible to every
/// tools/call immediately (per-request read).
async fn tools_policy_handler(
    State(app): axum::extract::State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_policy\"}");
        }
    };
    let tool = parsed.get("tool").and_then(serde_json::Value::as_str);
    let access = parsed.get("access").and_then(serde_json::Value::as_str);
    let safety = parsed.get("safety").and_then(serde_json::Value::as_bool);
    let Some(tool) = tool else {
        return go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_policy\"}");
    };
    match validate_policy_entry(tool, access, safety) {
        Ok(setting) => {
            match policy::update_entry(&app.workspace, tool, setting.access, setting.safety) {
                Ok(updated) => json_ok(
                    StatusCode::OK,
                    serde_json::to_string(&updated).unwrap_or_else(|_| "{}".into()),
                ),
                Err(e) => {
                    let body = serde_json::json!({ "error": format!("write failed: {e}") });
                    go_http_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        serde_json::to_string(&body)
                            .unwrap_or_else(|_| "{\"error\":\"write failed\"}".into()),
                    )
                }
            }
        }
        Err(PolicyValidationError::UnknownTool) => {
            go_http_error(StatusCode::NOT_FOUND, "{\"error\":\"unknown_tool\"}")
        }
        Err(PolicyValidationError::Invalid) => {
            go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_policy\"}")
        }
    }
}

/// Phase 3: `/api/safety/*` — the GUI API (plan §6, frozen shapes). All
/// routes live behind the SAME bearer middleware as `/mcp` (the parentRouter
/// layers auth over everything nested here); header auth only.
/// `GET /api/safety/pending` → `200 {"pending":[{id,tool,command,cwd,reason,
/// ruleId,created_at,expires_at}]}` (waiting entries only; expired ones are
/// pruned server-side — timeout⇒deny is enforced regardless).
async fn safety_pending_handler(
    axum::extract::State(app): axum::extract::State<AppState>,
) -> Response {
    let engine = &app.engine;
    let items: Vec<serde_json::Value> = engine
        .pending_snapshot()
        .into_iter()
        .map(|v| {
            serde_json::json!({
                "id": v.id,
                "tool": v.tool,
                "command": v.command,
                "cwd": v.cwd,
                "reason": v.reason,
                "ruleId": v.rule_id,
                "created_at": v.created_at,
                "expires_at": v.expires_at,
            })
        })
        .collect();
    json_ok(
        StatusCode::OK,
        format!(
            "{{\"pending\":{}}}",
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
        ),
    )
}

/// `POST /api/safety/decide/{id}` body `{"decision":"allow"|"deny"|
/// "allow_session"}` → `200 {"ok":true}` | `404 {"error":"not_found"}` |
/// `409 {"error":"already_decided"}`. Malformed bodies/unknown decision
/// values → 400 `{"error":"invalid decision"}` (not in the frozen enum —
/// flagged to the orchestrator as an add-on shape).
async fn safety_decide_handler(
    axum::extract::State(app): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid decision\"}");
        }
    };
    let Some(decision) = parsed
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .and_then(safety::DecisionChoice::parse)
    else {
        return go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid decision\"}");
    };
    match app.engine.decide(&id, decision) {
        Ok(_) => json_ok(StatusCode::OK, "{\"ok\":true}"),
        Err(safety::DecideError::NotFound) => {
            go_http_error(StatusCode::NOT_FOUND, "{\"error\":\"not_found\"}")
        }
        Err(safety::DecideError::AlreadyDecided) => {
            go_http_error(StatusCode::CONFLICT, "{\"error\":\"already_decided\"}")
        }
    }
}

/// `SSE GET /api/safety/events` — `event: pending` / `event: resolved`
/// frames with JSON bodies, keepalive `: ping` every 15s.
async fn safety_events_handler(
    axum::extract::State(app): axum::extract::State<AppState>,
) -> axum::response::Sse<
    impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use tokio_stream::{StreamExt, wrappers::BroadcastStream};
    let stream = BroadcastStream::new(app.engine.subscribe()).filter_map(|msg| {
        let ev = match msg {
            Ok(ev) => ev,
            // A slow SSE consumer lags the broadcast — emit nothing rather
            // than dropping the connection (avoids error events client-side).
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                return None;
            }
        };
        let payload = match ev {
            safety::Event::Pending(v) => {
                axum::response::sse::Event::default().event("pending").data(
                    serde_json::json!({
                        "id": v.id,
                        "tool": v.tool,
                        "command": v.command,
                        "cwd": v.cwd,
                        "reason": v.reason,
                        "ruleId": v.rule_id,
                        "created_at": v.created_at,
                        "expires_at": v.expires_at,
                    })
                    .to_string(),
                )
            }
            safety::Event::Resolved { id, decision } => axum::response::sse::Event::default()
                .event("resolved")
                .data(serde_json::json!({"id": id, "decision": decision}).to_string()),
        };
        Some(Ok::<_, std::convert::Infallible>(payload))
    });
    axum::response::Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
}

/// /assets/ static handler (hand-rolled mirror of Go's `http.FileServer`):
/// GET/HEAD only, rejects `..`/`.` segments and anything escaping the assets
/// dir, serves files with an extension→MIME map, and returns the Go 404
/// shape for misses. Divergences from Go's FileServer (documented): no
/// directory listing HTML and no automatic index.html fallback — directory
/// targets return the Go 404 shape.
async fn assets_handler(
    State(app): axum::extract::State<AppState>,
    axum::extract::Path(raw): axum::extract::Path<String>,
    method: axum::http::Method,
) -> Response {
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        // Go's FileServer also 405s non-GET methods with Allow: GET/HEAD.
        // It uses http.Error too, so the wording matches our shape.
        let mut res = go_http_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "{\"error\":\"method not allowed\"}",
        );
        res.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return res;
    }
    let rel = raw.trim_matches('/');
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        // Directory root: no listing (documented divergence from Go's
        // FileServer HTML index).
        return go_http_error(StatusCode::NOT_FOUND, "404 page not found");
    }
    if parts.iter().any(|p| *p == ".." || *p == ".") {
        return go_http_error(StatusCode::NOT_FOUND, "404 page not found");
    }
    if parts
        .iter()
        .any(|p| p.contains('\\') || p.chars().any(|c| c == '\0' || (c as u32) < 0x20))
    {
        // Backslash fragments encode path-confusion (Windows-style) and
        // NUL/control bytes are never legitimate names: plain 400.
        return go_http_error(
            StatusCode::BAD_REQUEST,
            "{\"error\":\"invalid asset path\"}",
        );
    }
    let target = format!("{}/assets/{}", app.workspace, parts.join("/"));
    let meta = match tokio::fs::metadata(&target).await {
        Ok(m) if m.is_file() => m,
        _ => return go_http_error(StatusCode::NOT_FOUND, "404 page not found"),
    };
    let bytes = match tokio::fs::read(&target).await {
        Ok(b) => b,
        Err(_) => return go_http_error(StatusCode::NOT_FOUND, "404 page not found"),
    };
    let _ = meta;
    let mime = mime_by_extension(&target);
    (StatusCode::OK, [(header::CONTENT_TYPE, mime)], bytes).into_response()
}

/// Extension→MIME table mirroring the file types this server hands out
/// (Go's FileServer guesses via mime.TypeByExtension; the interesting ones
/// for assets are covered here, everything else is octet-stream).
fn mime_by_extension(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// POST handler for /api/save/{*path} — byte-mirrors Go's buildMux /api/save/
/// branch (main.go ~1345): pathSegmentPattern guard per segment, 2MB body
/// cap, JSON validation, then a 0644 write under assets/data/<segments>/
/// and `{"ok":true}` (Go sets Content-Type manually here, no trailing
/// newline). Non-POST methods land in `api_save_method_fallback` (Go checks
/// the method first).
async fn api_save_handler(
    State(app): axum::extract::State<AppState>,
    axum::extract::Path(raw): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let rest = raw.trim_matches('/');
    if rest.is_empty() {
        return go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid name\"}");
    }
    let segments: Vec<&str> = rest.split('/').collect();
    for seg in &segments {
        // Go: pathSegmentPattern = ^[a-zA-Z0-9_-]+$
        let ok = !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !ok {
            return go_http_error(
                StatusCode::BAD_REQUEST,
                "{\"error\":\"invalid path segment\"}",
            );
        }
    }
    if body.len() > 2 << 20 {
        // Go reads through io.LimitReader(2<<20); oversized bodies end up
        // truncated and json-invalid — same error, same code.
        return go_http_error(
            StatusCode::BAD_REQUEST,
            "{\"error\":\"body is not valid JSON\"}",
        );
    }
    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return go_http_error(
            StatusCode::BAD_REQUEST,
            "{\"error\":\"body is not valid JSON\"}",
        );
    }
    let name = segments[segments.len() - 1];
    let mut dir = format!("{}/assets/data", app.workspace);
    for seg in &segments[..segments.len() - 1] {
        dir.push('/');
        dir.push_str(seg);
    }
    if (tokio::fs::create_dir_all(&dir).await).is_err() {
        return go_http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "{\"error\":\"server error\"}",
        );
    }
    let target = format!("{dir}/{name}.json");
    if (tools::write_file_mode(&target, &body).await).is_err() {
        return go_http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "{\"error\":\"write failed\"}",
        );
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        "{\"ok\":true}",
    )
        .into_response()
}

/// Go 405 for non-POST on /api/save/ (Go checks the method FIRST, before the
/// empty-name check).
async fn api_save_method_fallback(_req: AxumRequest) -> Response {
    go_http_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "{\"error\":\"method not allowed\"}",
    )
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Version drift guard: the ServerInfo version literal in
    /// `#[tool_handler(version = …)]` above must equal the package manifest
    /// version (the contract pins 1.0.0 (P5 cutover version); the macro requires a
    /// literal, so this test is what keeps the pair in sync — bumping
    /// CARGO_PKG_VERSION without updating the literal fails here).
    #[test]
    fn handler_version_tracks_manifest() {
        assert_eq!("1.0.0", env!("CARGO_PKG_VERSION"));
    }

    /// The rmcp wiring must expose exactly the tools::REGISTRY manifest
    /// (55 tools, Go's setupTools list and order).
    #[test]
    fn registry_matches_the_go_setup_list() {
        let names: Vec<&'static str> = tools::REGISTRY.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 55, "Go registers 55 tools");
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicate tool names");
        assert!(
            tools::REGISTRY
                .iter()
                .all(|t| !t.title.is_empty() && !t.description.is_empty()),
            "every entry carries Go's title + description"
        );
    }

    /// text_result must produce the Go-shaped payload: text content plus
    /// structured `{"text": …}`.
    #[test]
    fn text_result_shape() {
        let res = text_result(ToolOut::Text("hello".to_string()));
        assert_eq!(res.content.len(), 1);
        assert_eq!(res.structured_content, Some(json!({"text": "hello"})));
    }
}

#[cfg(test)]
mod control_panel_tests {
    use super::*;
    use crate::policy::{self, Access, Setting};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_ws(tag: &str) -> Arc<str> {
        let dir = std::env::temp_dir().join(format!(
            "ryns-ctl-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::from(dir.to_string_lossy().to_string())
    }

    fn temp_engine(tag: &str) -> (Arc<str>, Arc<safety::Engine>) {
        let ws = temp_ws(tag);
        let engine = Arc::new(safety::Engine::new(
            &safety::Config {
                mode: safety::Mode::Enforce,
                ask_timeout_ms: 150,
                node_bin: "node".into(),
                sidecar_script: "/nonexistent/index.mjs".into(),
            },
            &ws,
        ));
        (ws, engine)
    }

    /// §A4: with the DEFAULT policy, every one of the 55 registry tools is
    /// visible in tools/list order.
    #[test]
    fn default_policy_keeps_all_55_visible() {
        let policy = policy::default_policy();
        for spec in tools::REGISTRY.iter() {
            assert!(
                policy::is_visible(&policy, spec.name),
                "{} must be visible",
                spec.name
            );
        }
        assert_eq!(tools::REGISTRY.len(), 55);
        // A disabled tool must be hidden by the predicate.
        let mut pk = policy.clone();
        pk.tools.insert(
            "edit_file".into(),
            Setting {
                access: Access::Disabled,
                safety: true,
            },
        );
        assert!(!policy::is_visible(&pk, "edit_file"));
    }

    /// §A5: POST validation matrix.
    #[test]
    fn validate_policy_entry_table() {
        use PolicyValidationError::{Invalid, UnknownTool};
        assert_eq!(
            validate_policy_entry("edit_file", Some("ask"), Some(true)),
            Ok(Setting {
                access: Access::Ask,
                safety: true
            })
        );
        assert_eq!(
            validate_policy_entry("nonexistent_tool", Some("ask"), Some(true)),
            Err(UnknownTool)
        );
        assert_eq!(
            validate_policy_entry("edit_file", Some("clown"), Some(true)),
            Err(Invalid)
        );
        assert_eq!(
            validate_policy_entry("edit_file", Some("ask"), None),
            Err(Invalid)
        );
    }

    fn app() -> AppState {
        let (ws, engine) = temp_engine("api");
        policy::ensure_defaults(&ws);
        AppState {
            workspace: ws,
            engine,
            port: 8799,
            auth_enabled: false,
        }
    }

    /// §A5: /api/config shape with default policy.
    #[test]
    fn bare_mode_guard_table() {
        // clean requests (good Host, no Origin) pass; browsers (Origin —
        // always sent cross-origin) and rebinding Hosts are rejected.
        assert!(bare_mode_allowed(Some("127.0.0.1:8799"), None, 8799));
        assert!(bare_mode_allowed(Some("localhost:8799"), None, 8799));
        assert!(!bare_mode_allowed(Some("evil.com"), None, 8799));
        assert!(!bare_mode_allowed(Some("127.0.0.1:9999"), None, 8799));
        assert!(!bare_mode_allowed(
            Some("127.0.0.1:8799"),
            Some("https://evil.com"),
            8799
        ));
        // missing Host header at all → rejected (defensive).
        assert!(!bare_mode_allowed(None, None, 8799));
    }

    #[tokio::test]
    async fn api_config_shape() {
        let app = app();
        let resp = config_handler(axum::extract::State(app.clone())).await;
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["port"], 8799);
        assert_eq!(v["mode"], "enforce");
        assert_eq!(v["auth"], false);
        assert_eq!(v["tool_counts"]["enabled"], 55);
        assert_eq!(v["tool_counts"]["ask"], 0);
        assert_eq!(v["tool_counts"]["disabled"], 0);
        assert_eq!(v["workspace"], app.workspace.to_string());
        assert!(v["server_pid"].as_u64().unwrap() > 0);
    }

    /// §A5: /api/tools exposes all 55 with safety_capable on the 12 and the
    /// per-call flag; post-update reflects WITHOUT restart (fresh read).
    #[tokio::test]
    async fn api_tools_shape_and_fresh_visibility() {
        let app = app();
        let resp = tools_handler(axum::extract::State(app.clone())).await;
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let list = v["tools"].as_array().unwrap();
        assert_eq!(list.len(), 55);
        let edit = list.iter().find(|t| t["name"] == "edit_file").unwrap();
        assert_eq!(edit["access"], "enabled");
        assert_eq!(edit["safety"], true);
        assert_eq!(edit["safety_capable"], true);
        let shot = list.iter().find(|t| t["name"] == "screenshot").unwrap();
        assert_eq!(shot["safety_capable"], false);
        assert_eq!(shot["safety"], false);
        // POST policy → the next handler call sees the new values.
        policy::update_entry(&app.workspace, "edit_file", Access::Ask, false).unwrap();
        let v2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(
                tools_handler(axum::extract::State(app.clone()))
                    .await
                    .into_body(),
                1 << 20,
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let edit2 = v2["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "edit_file")
            .unwrap();
        assert_eq!(edit2["access"], "ask");
        assert_eq!(edit2["safety"], false);
        // /api/config counts reflect too.
        let config_body = axum::body::to_bytes(
            config_handler(axum::extract::State(app.clone()))
                .await
                .into_body(),
            1 << 20,
        )
        .await
        .unwrap()
        .to_vec();
        let cfg: serde_json::Value = serde_json::from_slice(&config_body).unwrap();
        assert_eq!(cfg["tool_counts"]["ask"], 1);
        assert_eq!(cfg["tool_counts"]["enabled"], 54);
    }

    /// §A5: POST /api/tools/policy handler validation via the real handler.
    #[tokio::test]
    async fn api_tools_policy_handler_validation() {
        let app = app();
        let post = |body: &'static str| {
            let app = app.clone();
            async move {
                let r =
                    tools_policy_handler(axum::extract::State(app), axum::body::Bytes::from(body))
                        .await;
                let status = r.status();
                let text = String::from_utf8_lossy(
                    &axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap(),
                )
                .into_owned();
                (status, text)
            }
        };
        let (s1, b1) = post(r#"{"tool":"edit_file","access":"ask","safety":true}"#).await;
        assert_eq!(s1, 200);
        let v: serde_json::Value = serde_json::from_str(&b1).unwrap();
        assert_eq!(v["tools"]["edit_file"]["access"], "ask");
        assert_eq!(v["tools"]["edit_file"]["safety"], true);
        // persisted → visible to a per-request read.
        assert_eq!(
            policy::load(&app.workspace).tools["edit_file"].access,
            Access::Ask
        );
        let (s2, b2) = post(r#"{"tool":"nonexistent_tool","access":"ask","safety":true}"#).await;
        assert_eq!(s2, 404);
        assert!(b2.contains("unknown_tool"));
        let (s3, b3) = post(r#"{"tool":"edit_file","access":"clown","safety":true}"#).await;
        assert_eq!(s3, 400);
        assert!(b3.contains("invalid_policy"));
        let (s4, b4) = post(r#"{"tool":"edit_file","access":"ask"}"#).await;
        assert_eq!(s4, 400);
        assert!(b4.contains("invalid_policy"));
        let (s5, b5) = post("definitely not json").await;
        assert_eq!(s5, 400);
        assert!(b5.contains("invalid_policy"));
    }
}

#[cfg(test)]
mod go_order_tests {
    use super::*;

    /// The registration order is a contract: tools/list advertises this exact
    /// sequence (matches Go setupTools; verified by the tools/list parity
    /// harness — the exact 55 names in Go's order).
    #[test]
    fn registry_exact_go_order() {
        let names: Vec<&'static str> = tools::REGISTRY.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "execute_command",
                "start_process",
                "process_output",
                "send_input",
                "list_processes",
                "kill_process",
                "read_file",
                "write_file",
                "edit_file",
                "multi_edit_file",
                "apply_patch",
                "list_directory",
                "directory_tree",
                "search_files",
                "grep",
                "create_directory",
                "delete_file",
                "move_file",
                "get_file_info",
                "screenshot",
                "clipboard_get",
                "clipboard_set",
                "mouse_click",
                "keyboard_type",
                "key_press",
                "get_active_window",
                "list_windows",
                "notify_send",
                "ocr_screenshot",
                "camera_capture",
                "record_video",
                "git_status",
                "git_diff",
                "git_log",
                "git_branch",
                "workspace_info",
                "sheet_create",
                "sheet_list",
                "sheet_get",
                "sheet_delete",
                "sheet_rename",
                "column_add",
                "column_remove",
                "column_rename",
                "row_add",
                "row_remove",
                "cell_set",
                "cell_get",
                "sheet_url",
                "check_gold_price",
                "todo_write",
                "todo_read",
                "lint_format",
                "system_stats",
                "acp_opencode",
            ]
        );
    }
}

#[cfg(test)]
mod http_helpers {
    use super::*;

    /// assets fileserver MIME table (extension classes handed out through
    /// /assets/).
    #[test]
    fn mime_by_extension_table() {
        assert_eq!(mime_by_extension("a.png"), "image/png");
        assert_eq!(mime_by_extension("a.JPG"), "image/jpeg");
        assert_eq!(mime_by_extension("a.webp"), "image/webp");
        assert_eq!(mime_by_extension("a.json"), "application/json");
        assert_eq!(mime_by_extension("a.html"), "text/html; charset=utf-8");
        assert_eq!(mime_by_extension("a.css"), "text/css; charset=utf-8");
        assert_eq!(mime_by_extension("a.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_by_extension("a.md"), "text/plain; charset=utf-8");
        assert_eq!(mime_by_extension("weird.xyz"), "application/octet-stream");
        assert_eq!(mime_by_extension("noext"), "application/octet-stream");
    }

    /// text_result(ToolOut::TextPlusImage) → text content + base64 image +
    /// structured `{"text": …}` (screenshot/camera/read_file branch).
    #[test]
    fn image_result_shape() {
        let out = ToolOut::image(
            "shot (image/png, 4 bytes)".into(),
            "image/png",
            b"x".repeat(3),
        );
        let res = text_result(out);
        assert_eq!(res.content.len(), 2, "content: {res:?}");
        assert_eq!(
            res.structured_content,
            Some(json!({"text": "shot (image/png, 4 bytes)"}))
        );
    }

    /// go_http_error appends the trailing "\n" exactly like http.Error.
    #[test]
    fn go_http_error_trailing_newline() {
        let r: Response =
            go_http_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid name\"}").into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}

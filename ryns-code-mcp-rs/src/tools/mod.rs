//! Tool implementations mirroring Go `setupTools` (main.go ~1391): one
//! module per tool where helpful, exact tool names, and the friendly
//! `"<prefix>_error: …"` plain-text error convention (as successful tool
//! results, like Go — errors are text payloads, never isError flags).
//!
//! `REGISTRY` is the single manifest of the tools this server exposes;
//! the rmcp `#[tool_router]` wiring in `crate::mcp` must match it.

pub mod acp;
pub mod desktop;
pub mod execute_command;
pub mod files;
pub mod git;
pub mod list_directory;
pub mod misc;
pub mod processes;
pub mod read_file;
pub mod sheets;
pub mod todos;
pub mod write_file;

use std::sync::Arc;

pub const MAX_OUTPUT: usize = 100_000;
pub const MAX_READ_LINES: i64 = 2000;
pub const MAX_LINE_LENGTH: usize = 2000;
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;

/// One entry in the tool registry, mirroring a Go `mcp.AddTool` call.
/// The manifest is exercised by the registry parity test in `crate::mcp`;
/// rmcp's `#[tool_router]` wiring is the runtime counterpart.
#[allow(dead_code)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

/// The full P2 tool set, in the order Go registers them in `setupTools`.
#[allow(dead_code)]
pub const REGISTRY: &[ToolSpec] = &[
    ToolSpec {
        name: "execute_command",
        title: "Execute Command",
        description: "Run a one-shot shell command inside RynsWorkspace (bash on Linux/macOS, PowerShell on Windows). Good for git, npm, tests, builds and system inspection.",
    },
    ToolSpec {
        name: "start_process",
        title: "Start Process",
        description: "Start a long-running shell command and keep its process/session alive.",
    },
    ToolSpec {
        name: "process_output",
        title: "Process Output",
        description: "Read accumulated stdout/stderr and current state from a long-running process.",
    },
    ToolSpec {
        name: "send_input",
        title: "Send Process Input",
        description: "Send stdin to a running process.",
    },
    ToolSpec {
        name: "list_processes",
        title: "List Processes",
        description: "List processes started by this MCP server.",
    },
    ToolSpec {
        name: "kill_process",
        title: "Kill Process",
        description: "Terminate a process started by this MCP server.",
    },
    ToolSpec {
        name: "read_file",
        title: "Read File",
        description: "Read a file. Text is returned with cat -n style line numbers (scope it with start_line/end_line for big files; unscoped reads are capped around 2000 lines). Common image formats (png/jpg/jpeg/gif/webp/bmp) are returned as an inline image instead of text.",
    },
    ToolSpec {
        name: "write_file",
        title: "Write File",
        description: "Create or overwrite a UTF-8 text file. Reports whether it created a new file or overwrote an existing one, with byte/line counts.",
    },
    ToolSpec {
        name: "edit_file",
        title: "Edit File",
        description: "Replace exact text in a file; refuses ambiguous matches unless replace_all is enabled. Returns a numbered context snippet around the change so you can confirm it without re-reading the file.",
    },
    ToolSpec {
        name: "multi_edit_file",
        title: "Multi Edit File",
        description: "Apply several exact-text edits to one file atomically and in order: either every edit matches and the file is written once, or nothing is written and the failing edit is reported.",
    },
    ToolSpec {
        name: "apply_patch",
        title: "Apply Patch",
        description: "Apply a unified diff patch to workspace files using the system patch utility.",
    },
    ToolSpec {
        name: "list_directory",
        title: "List Directory",
        description: "List directory entries with type and size.",
    },
    ToolSpec {
        name: "directory_tree",
        title: "Directory Tree",
        description: "Return a recursive workspace tree.",
    },
    ToolSpec {
        name: "search_files",
        title: "Search Files",
        description: "Find files using ripgrep glob patterns.",
    },
    ToolSpec {
        name: "grep",
        title: "Grep",
        description: "Search file contents recursively with ripgrep regex, glob, case and context support.",
    },
    ToolSpec {
        name: "create_directory",
        title: "Create Directory",
        description: "Create a directory recursively.",
    },
    ToolSpec {
        name: "delete_file",
        title: "Delete File",
        description: "Delete a file or directory; recursive deletion must be explicit.",
    },
    ToolSpec {
        name: "move_file",
        title: "Move File",
        description: "Move or rename a file/directory inside the workspace.",
    },
    ToolSpec {
        name: "get_file_info",
        title: "Get File Info",
        description: "Return file/directory metadata.",
    },
    ToolSpec {
        name: "screenshot",
        title: "Screenshot",
        description: "Capture the current screen (or a specific X window) as a PNG image, returned inline. Optionally also saves a copy to disk.",
    },
    ToolSpec {
        name: "clipboard_get",
        title: "Clipboard Get",
        description: "Read the current text on the X clipboard.",
    },
    ToolSpec {
        name: "clipboard_set",
        title: "Clipboard Set",
        description: "Set the X clipboard to the given text.",
    },
    ToolSpec {
        name: "mouse_click",
        title: "Mouse Click",
        description: "Move the mouse to (x,y) and click. button: 1=left, 2=middle, 3=right.",
    },
    ToolSpec {
        name: "keyboard_type",
        title: "Keyboard Type",
        description: "Type literal text at the current input focus, as if typed on the keyboard.",
    },
    ToolSpec {
        name: "key_press",
        title: "Key Press",
        description: "Send a key or key combo using xdotool key syntax, e.g. 'Return', 'ctrl+c', 'alt+Tab', 'super'.",
    },
    ToolSpec {
        name: "get_active_window",
        title: "Get Active Window",
        description: "Return the id, title, and window class of the currently focused window.",
    },
    ToolSpec {
        name: "list_windows",
        title: "List Windows",
        description: "List visible windows with their id and title.",
    },
    ToolSpec {
        name: "notify_send",
        title: "Send Notification",
        description: "Show a desktop notification popup.",
    },
    ToolSpec {
        name: "ocr_screenshot",
        title: "OCR Screenshot",
        description: "Capture the screen (or a specific window) and run OCR to extract any visible text.",
    },
    ToolSpec {
        name: "camera_capture",
        title: "Camera Capture",
        description: "Capture a single JPEG photo from the webcam and return it inline. Optionally also saves a copy to disk.",
    },
    ToolSpec {
        name: "record_video",
        title: "Record Video",
        description: "Record the screen or the webcam for a set number of seconds (max 120), save it as an MP4 under the public assets/ folder, and return its public URL — no video bytes are sent back through the tool result, just the link.",
    },
    ToolSpec {
        name: "git_status",
        title: "git status",
        description: "Show git status and branch.",
    },
    ToolSpec {
        name: "git_diff",
        title: "git diff",
        description: "Show working-tree diff.",
    },
    ToolSpec {
        name: "git_log",
        title: "git log",
        description: "Show recent commits.",
    },
    ToolSpec {
        name: "git_branch",
        title: "git branch",
        description: "Show branches and tracking info.",
    },
    ToolSpec {
        name: "workspace_info",
        title: "Workspace Info",
        description: "Show workspace and runtime information.",
    },
    ToolSpec {
        name: "sheet_create",
        title: "Sheet Create",
        description: "Create a new local sheet and register it in the sheet index. Fails if a sheet with the same id already exists unless overwrite is set.",
    },
    ToolSpec {
        name: "sheet_list",
        title: "Sheet List",
        description: "List every sheet in the index with its id, title, row count, and column count.",
    },
    ToolSpec {
        name: "sheet_get",
        title: "Sheet Get",
        description: "Show the contents of a sheet as a readable text table, optionally scoped to a row range for large sheets.",
    },
    ToolSpec {
        name: "sheet_delete",
        title: "Sheet Delete",
        description: "Delete a sheet's underlying file and remove its entry from the index.",
    },
    ToolSpec {
        name: "sheet_rename",
        title: "Sheet Rename",
        description: "Change a sheet's display title while keeping its id and data unchanged.",
    },
    ToolSpec {
        name: "column_add",
        title: "Column Add",
        description: "Add a new column to a sheet. When after_column is given, the new column is inserted right after that column instead of at the end, and every existing row gains an empty value in the new position.",
    },
    ToolSpec {
        name: "column_remove",
        title: "Column Remove",
        description: "Remove a column from a sheet by name or index, dropping that value from every row.",
    },
    ToolSpec {
        name: "column_rename",
        title: "Column Rename",
        description: "Rename a column in a sheet, identified by its current name or index.",
    },
    ToolSpec {
        name: "row_add",
        title: "Row Add",
        description: "Add a new row of values to a sheet, appended at the end by default or inserted at a given zero based position.",
    },
    ToolSpec {
        name: "row_remove",
        title: "Row Remove",
        description: "Remove a row from a sheet by its zero based index.",
    },
    ToolSpec {
        name: "cell_set",
        title: "Cell Set",
        description: "Set the value of a single cell in a sheet, identified by row index and column name or index.",
    },
    ToolSpec {
        name: "cell_get",
        title: "Cell Get",
        description: "Read the value of a single cell in a sheet, identified by row index and column name or index.",
    },
    ToolSpec {
        name: "sheet_url",
        title: "Sheet URL",
        description: "Return the public URL where a sheet can be opened and edited in a browser.",
    },
    ToolSpec {
        name: "check_gold_price",
        title: "Check Gold Price",
        description: "Fetch today's official Antam Logam Mulia gold price (24K) plus an estimated 22K conversion, sourced live from logammulia.com.",
    },
    ToolSpec {
        name: "todo_write",
        title: "Todo Write",
        description: "Replace the entire todo checklist with the given list of items. Call this with the complete, updated list every time progress changes, mirroring how Claude Code's TodoWrite tracks multi-step work.",
    },
    ToolSpec {
        name: "todo_read",
        title: "Todo Read",
        description: "Show the current todo checklist without modifying it.",
    },
    ToolSpec {
        name: "lint_format",
        title: "Lint Format",
        description: "Check or auto-fix code formatting for a file or directory. Supports Go via gofmt, Rust via rustfmt, JS/TS/JSON/CSS/Markdown via prettier (only if the project already has it installed), and a Python syntax check (formatting not supported since no Python formatter is installed). Set fix=true to apply changes in place; default only reports what would change.",
    },
    ToolSpec {
        name: "system_stats",
        title: "System Stats",
        description: "Show a snapshot of the machine's health: CPU core count and load average, RAM and swap usage, root disk usage, battery status/health, and temperatures when available.",
    },
    ToolSpec {
        name: "acp_opencode",
        title: "ACP OpenCode",
        description: "Delegate a coding task to OpenCode as a sub-agent via the Agent Client Protocol (opencode acp). Spawns opencode, sends one prompt, waits for it to finish (auto-approving any permission prompts, since this is meant for autonomous delegation), and returns its final text response.",
    },
];

// ---- tool result envelope ----------------------------------------------------

/// What a tool handler produces: plain text (Go textResult) or text plus an
/// inline image (screenshot/camera_capture/read_file image branch —
/// Go returns a TextContent + ImageContent pair).
pub enum ToolOut {
    Text(String),
    TextPlusImage {
        text: String,
        mime: &'static str,
        data: Vec<u8>,
    },
}

impl ToolOut {
    pub fn image(text: String, mime: &'static str, data: Vec<u8>) -> ToolOut {
        ToolOut::TextPlusImage { text, mime, data }
    }
}

/// `fs.MkdirAll(filepath.Dir(dest), 0755)` equivalent used by the save-copy
/// branches (screenshot/camera_capture).
pub async fn mkdir_parent(dest: &str) {
    if let Some(dir) = std::path::Path::new(dest).parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
}

/// Go `time.Now().Format("20060102-150405")` (asset filename stamps).
pub fn rfc3339_stamp_digits(epoch_secs: i64) -> String {
    let (y, mo, d, h, mi, s, _) = civil_from_epoch(epoch_secs, 0);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// "Now" flavor of `rfc3339_stamp_digits` used by filename generators
/// (record_video: `record-{source}-20060102-150405.mp4`).
pub fn ts_filename() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_stamp_digits(secs)
}

// ---- per-server execution context ---------------------------------------------
#[derive(Clone)]
pub struct Ctx {
    pub workspace: Arc<str>,
    /// Phase 3: the safety engine, when available (None in some unit tests).
    /// Always Some in the real server (modes enforce/observe/off all audit).
    pub safety: Option<Arc<crate::safety::Engine>>,
    /// §A2 single-ASK marker: set for the current invocation when its forced
    /// ask was approved; gate paths must not re-ask this tool.
    pub approved_ask: Option<Arc<str>>,
}

impl Ctx {
    /// Test/offline constructor without a safety engine (used only from
    /// unit-test modules across the tools modules).
    #[cfg(test)]
    pub fn plain(workspace: impl Into<Arc<str>>) -> Ctx {
        Ctx {
            workspace: workspace.into(),
            safety: None,
            approved_ask: None,
        }
    }
}

// ---- path helpers mirroring Go's defaultString/safePath/Clean/Join ----------

/// Mirrors Go `defaultString(v, fallback)`.
pub fn default_string(v: Option<&str>, fallback: &str) -> String {
    match v {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => fallback.to_string(),
    }
}

/// Mirrors Go `filepath.Clean` for the separators used on the current OS
/// (no filesystem access — a pure normalization, like Go). The unix branch
/// and the windows branch (`path_clean_windows`) mirror their respective Go
/// files; the dispatch is a compile-time `cfg!`.
pub fn path_clean(input: &str) -> String {
    if cfg!(windows) {
        path_clean_windows(input)
    } else {
        path_clean_unix(input)
    }
}

/// Mirrors Go `filepath.Clean` (unix separators, path_unix.go).
pub fn path_clean_unix(input: &str) -> String {
    let rooted = input.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for elem in input.split('/') {
        match elem {
            "" | "." => {}
            ".." => {
                if rooted {
                    // "." elements above root are dropped.
                    if !parts.is_empty() {
                        parts.pop();
                    }
                } else if parts.last() == Some(&"..") || parts.pop().is_none() {
                    parts.push("..");
                }
            }
            _ => parts.push(elem),
        }
    }
    let joined = parts.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Mirrors Go `filepath.Clean` for Windows (path_windows.go semantics):
/// - verbatim device prefixes `\\?\…` (including `\\?\UNC\…`) and `\\.\…` are
///   returned **unchanged** (Go does not clean inside the device namespace;
///   this also guarantees `safe_path` never mangles such roots);
/// - raw UNC roots — a leading double separator (`\\server\share`) — keep
///   exactly two leading separators; extra leading separators collapse and
///   the remainder is cleaned lexically as rooted (so `..` above the share
///   root is dropped, like Go's rooted handling);
/// - a volume-relative path like `C:` keeps the volume and appends `.` when
///   no element follows (Go: `Clean("C:") = "C:."`); `C:elem` keeps `C:`
///   glued to the cleaned remainder without a separator; `C:\…` is cleaned
///   as rooted;
/// - otherwise: a single leading separator means rooted; `.` / `..` follow
///   the usual lexical rules.
///
/// Pure (no filesystem access), so the table tests run on every target.
pub fn path_clean_windows(input: &str) -> String {
    // Verbatim / device namespace: Go leaves these alone.
    for prefix in [r"\\?\UNC\", r"\\?\", r"\\.\"] {
        if input.starts_with(prefix) {
            return input.to_string();
        }
    }
    // Two-of-two separator check for a raw UNC root. Both separators count
    // (any mix of / and \\ works on Windows).
    let bytes = input.as_bytes();
    let is_sep = |b: &u8| *b == b'/' || *b == b'\\';
    if bytes.len() >= 2 && is_sep(&bytes[0]) && is_sep(&bytes[1]) {
        let rest = input[2..].trim_start_matches(['/', '\\']);
        let cleaned = clean_elements(rest, false, true);
        if cleaned.is_empty() {
            return "\\\\".to_string();
        }
        return format!("\\\\{cleaned}");
    }
    // Volume prefix: "C:…".
    if !bytes.is_empty() && bytes[0].is_ascii_alphabetic() && bytes.get(1) == Some(&b':') {
        let vol = &input[..2];
        let rest = &input[2..];
        let rooted = matches!(bytes.get(2), Some(c) if is_sep(c));
        let rest = if rooted { &rest[1..] } else { rest };
        let cleaned = clean_elements(rest, rooted, false);
        if cleaned.is_empty() {
            return format!("{vol}{}", if rooted { "\\" } else { "." });
        }
        return format!("{vol}{}{cleaned}", if rooted { "\\" } else { "" });
    }
    // Plain relative or "/"-rooted (single leading separator).
    let rooted = matches!(bytes.first(), Some(c) if is_sep(c));
    let rest = if rooted { &input[1..] } else { input };
    let cleaned = clean_elements(rest, rooted, false);
    if cleaned.is_empty() {
        return if rooted {
            "\\".to_string()
        } else {
            ".".to_string()
        };
    }
    if rooted {
        format!("\\{cleaned}")
    } else {
        cleaned
    }
}

/// Dot / dot-dot element loop behind `path_clean_windows` (input split on
/// `\\` and `/`): keeps non-"."/".." elements; `..` pops the accumulated
/// stack, except it pushes its own ".." at the relative bottom (single
/// leading ".." in a relative path) or is dropped at a root (`rooted`, and
/// the `\\` prefix for UNC). Returns the backslash-joined result, possibly
/// empty (when everything collapses to the root). Pure, no fs access.
fn clean_elements(input: &str, rooted: bool, unc: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for elem in input.split(['/', '\\']) {
        match elem {
            "" | "." => {}
            ".." => {
                if rooted || unc {
                    if !parts.is_empty() {
                        parts.pop();
                    }
                } else if parts.last() == Some(&"..") || parts.pop().is_none() {
                    parts.push("..");
                }
            }
            _ => parts.push(elem),
        }
    }
    parts.join("\\")
}

/// Mirrors Go `filepath.Join(base, elems...)`: concatenate with the OS
/// separator, then Clean.
pub fn path_join(base: &str, rel: &str) -> String {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    path_clean(&format!("{base}{sep}{rel}"))
}

fn is_abs(p: &str) -> bool {
    if cfg!(windows) {
        if p.starts_with(['/', '\\']) {
            return true;
        }
        let b = p.as_bytes();
        return b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    }
    p.starts_with('/')
}

/// Mirrors Go `filepath.Abs`: relative paths resolve against the process
/// working directory, then everything is cleaned (no filesystem access).
pub fn path_abs(p: &str) -> Result<String, String> {
    if is_abs(p) {
        return Ok(path_clean(p));
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cannot get working directory: {e}"))?;
    let cwd = cwd.to_string_lossy();
    Ok(path_clean(&format!(
        "{cwd}{}{p}",
        if cfg!(windows) { '\\' } else { '/' }
    )))
}

/// Mirrors Go `safePath` (main.go ~253): this box is a dedicated AI-agent
/// machine, so paths are NOT jailed to the workspace root — relative paths
/// resolve against the workspace for convenience, but absolute paths pass
/// through untouched (intentionally no escape check). An empty input
/// resolves to the workspace root itself.
pub fn safe_path(ctx: &Ctx, input: &str) -> Result<String, String> {
    if input.is_empty() {
        return path_abs(&ctx.workspace);
    }
    let mut p = input.to_string();
    if !is_abs(&p) {
        let base = path_abs(&ctx.workspace)?;
        p = path_join(&base, &p);
    }
    path_abs(&p)
}

// ---- output helpers mirroring Go's clip/textResult --------------------------

/// Mirrors Go `clip`: cut byte output at `maxOutput` with a trailing notice
/// (Go slices the raw bytes, so surviving invalid UTF-8 at the cut point is
/// replaced with U+FFFD here — matching how Go's JSON marshaling would
/// replace it anyway).
pub fn clip(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    String::from_utf8_lossy(&s.as_bytes()[..MAX_OUTPUT]).into_owned() + "\n...[output truncated]"
}

// ---- shared text/line helpers ----------------------------------------------

/// Mirrors Go `lineOfOffset` (main.go ~196): the 1-based line number that
/// contains byte offset idx.
pub fn line_of_offset(s: &str, idx: i64) -> i64 {
    let idx = idx.clamp(0, s.len() as i64);
    s.as_bytes()[..idx as usize]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as i64
        + 1
}

/// Mirrors Go `numberLines` (main.go ~183): "%6d\t%s\n" per line, a single
/// absurdly long line cut to `maxLineLength` bytes with a notice keeping the
/// total byte length.
pub fn number_lines<S: AsRef<str>>(lines: &[S], start: i64, end: i64) -> String {
    // Go's loop `for i := start - 1; i < end` prints nothing for an empty
    // range; guard against slice panics the same way.
    if start > end || end > lines.len() as i64 || start < 1 {
        return String::new();
    }
    let mut sb = String::new();
    for (idx, line) in lines[(start - 1) as usize..end as usize].iter().enumerate() {
        let line = line.as_ref();
        let shown: String = if line.len() > MAX_LINE_LENGTH {
            String::from_utf8_lossy(&line.as_bytes()[..MAX_LINE_LENGTH]).into_owned()
                + &format!("...[line truncated, {} chars total]", line.len())
        } else {
            line.to_string()
        };
        sb.push_str(&format!(
            "{:>6}\t{}\n",
            (start - 1) as usize + idx + 1,
            shown
        ));
    }
    sb
}

/// Shared writer used by write_file/edit_file/multi_edit_file: Unix creates
/// with explicit 0o644 (Go os.WriteFile semantics, pre-umask); other targets
/// use the platform default.
pub async fn write_file_mode(path: &str, bytes: &[u8]) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(path)
            .await?;
        f.write_all(bytes).await?;
        f.flush().await // drop flushes, like Go's Close
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(path, bytes).await
    }
}

/// Maps an `io::Error` onto the shape of Go's `os.PathError` messages
/// ("open /path: no such file or directory") so friendly error text stays
/// close to the Go server on unix. Unknown codes fall back to the Rust
/// Display text (documented divergence). Windows uses the Display fallback
/// for everything (its raw codes are Win32 values, not BSD/POSIX errno).
pub fn go_io_error(op: &str, path: &str, e: &std::io::Error) -> String {
    format!("{op} {path}: {}", errno_display(e))
}

/// The message body of an `io::Error` in Go's `syscall.Errno.Error()` wording
/// when the code is known for this OS, else Rust's `Display` text.
pub fn errno_display(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => errno_text(code)
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string()),
        None => e.to_string(),
    }
}

/// errno → Go `syscall.Errno.Error()` text. The tables are split per OS
/// because the numeric values differ (the value 62 is ELOOP on macOS but
/// ETIME on Linux; 11 is EAGAIN on Linux but EDEADLK on macOS; 40 is
/// `too many levels of symbolic links` on Linux but `message too long`
/// (EMSGSIZE) on macOS) — honest per-OS labels only, no shared table.
#[cfg(unix)]
fn errno_text(code: i32) -> Option<&'static str> {
    // macOS / BSD errno table (EAGAIN=35, ELOOP=62).
    #[cfg(target_os = "macos")]
    return Some(match code {
        1 => "operation not permitted",
        2 => "no such file or directory",
        3 => "no such process",
        4 => "interrupted system call",
        5 => "input/output error",
        9 => "bad file descriptor",
        12 => "cannot allocate memory",
        13 => "permission denied",
        17 => "file exists",
        20 => "not a directory",
        21 => "is a directory",
        22 => "invalid argument",
        24 => "too many open files",
        28 => "no space left on device",
        35 => "resource temporarily unavailable",
        36 => "file name too long",
        39 => "directory not empty",
        40 => "message too long",
        62 => "too many levels of symbolic links",
        _ => return None,
    });

    // Linux (glibc/musl) errno table (EAGAIN=11, ELOOP=40, ETIME=62).
    #[cfg(not(target_os = "macos"))]
    return Some(match code {
        1 => "operation not permitted",
        2 => "no such file or directory",
        3 => "no such process",
        4 => "interrupted system call",
        5 => "input/output error",
        9 => "bad file descriptor",
        11 => "resource temporarily unavailable",
        12 => "cannot allocate memory",
        13 => "permission denied",
        17 => "file exists",
        20 => "not a directory",
        21 => "is a directory",
        22 => "invalid argument",
        24 => "too many open files",
        28 => "no space left on device",
        36 => "file name too long",
        39 => "directory not empty",
        40 => "too many levels of symbolic links",
        62 => "timer expired",
        _ => return None,
    });
}

#[cfg(windows)]
fn errno_text(_code: i32) -> Option<&'static str> {
    // Windows raw codes are Win32 values, not BSD/POSIX errno; mapping them
    // with the unix table above would mislabel every message. Display
    // fallback only (documented divergence).
    None
}

// ---- shared output/result helpers -------------------------------------------

/// Mirrors Go `mustJSON`: pretty JSON with two-space indentation.
/// `serde_json::to_string_pretty` sorts object keys alphabetically (same as
/// Go's `json.MarshalIndent` on maps), so shapes stay byte-comparable.
pub fn must_json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())
}

/// Mirrors Go's `publicBaseURL` package var: `RYNS_PUBLIC_BASE_URL` when set
/// (trailing "/" trimmed), else `http://127.0.0.1:8799`.
pub fn public_base_url() -> String {
    match std::env::var("RYNS_PUBLIC_BASE_URL") {
        Ok(v) if !v.is_empty() => v.trim_end_matches('/').to_string(),
        _ => "http://127.0.0.1:8799".to_string(),
    }
}

/// Mirrors Go `hostname()` (os.Hostname); "" when nothing is discoverable.
/// std has no hostname API, so: HOSTNAME env → /proc/sys/kernel/hostname
/// (Linux) → `hostname` binary (macOS, resolved once).
pub fn hostname() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            if let Ok(h) = std::env::var("HOSTNAME")
                && !h.is_empty()
            {
                return h;
            }
            #[cfg(target_os = "linux")]
            if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname")
                && !h.trim().is_empty()
            {
                return h.trim().to_string();
            }
            #[cfg(not(target_os = "linux"))]
            if let Ok(out) = std::process::Command::new("hostname").output() {
                let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !h.is_empty() {
                    return h;
                }
            }
            String::new()
        })
        .clone()
}

// ---- time formatting (no chrono) ---------------------------------------------

/// Splits a unix-epoch stamp into (y, mo, d, h, mi, s, nanos) UTC, using the
/// standard civil-from-days algorithm (Howard Hinnant).
#[allow(clippy::too_many_arguments)]
fn civil_from_epoch(epoch_secs: i64, nanos: u32) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // Civil-from-days (Valid range: years 0..=9999 is far beyond what we use).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, h as u32, mi as u32, s as u32, nanos)
}

/// Formats a SystemTime as Go `time.RFC3339` in UTC ("2026-11-03T13:02:15Z").
pub fn rfc3339_secs(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s, _) = civil_from_epoch(secs, 0);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Formats a SystemTime as Go `time.RFC3339Nano` in UTC — like RFC3339 but
/// with the fractional part trimmed of trailing zeros (and omitted entirely
/// when zero), matching Go's formatting.
pub fn rfc3339_nano(t: std::time::SystemTime) -> String {
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (y, mo, dd, h, mi, s, nanos) = civil_from_epoch(d.as_secs() as i64, d.subsec_nanos());
    let base = format!("{y:04}-{mo:02}-{dd:02}T{h:02}:{mi:02}:{s:02}");
    if nanos == 0 {
        return format!("{base}Z");
    }
    let frac = format!("{nanos:09}");
    let frac = frac.trim_end_matches('0');
    format!("{base}.{frac}Z")
}

/// Parses "YYYY-MM-DD" into (y, m, d), mirroring Go `time.Parse("2006-01-02")`
/// for the gold-price date labels. None when it doesn't parse.
pub fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c2() -> Ctx {
        Ctx::plain("/ws")
    }

    #[test]
    fn path_clean_cases() {
        assert_eq!(path_clean(""), ".");
        assert_eq!(path_clean("."), ".");
        assert_eq!(path_clean("/"), "/");
        assert_eq!(path_clean("//a//b///"), "/a/b");
        assert_eq!(path_clean("a/./b/../c"), "a/c");
        assert_eq!(path_clean("/../a"), "/a");
        assert_eq!(path_clean("../../a"), "../../a");
        assert_eq!(path_clean("a/b/../../.."), "..");
    }

    #[test]
    fn path_join_and_abs() {
        if cfg!(unix) {
            assert_eq!(path_join("/ws", "sub/file.txt"), "/ws/sub/file.txt");
            assert_eq!(path_join("/ws", "./x/.."), "/ws");
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_path_relative_and_absolute() {
        let ctx = c2();
        assert_eq!(safe_path(&ctx, "a.txt").unwrap(), "/ws/a.txt");
        assert_eq!(safe_path(&ctx, "sub/../b.txt").unwrap(), "/ws/b.txt");
        assert_eq!(safe_path(&ctx, "/etc/passwd").unwrap(), "/etc/passwd");
        assert_eq!(safe_path(&ctx, "").unwrap(), "/ws");
        assert_eq!(safe_path(&ctx, "../outside.txt").unwrap(), "/outside.txt");
    }

    #[test]
    fn default_string_fallback() {
        assert_eq!(default_string(None, "."), ".");
        assert_eq!(default_string(Some(""), "."), ".");
        assert_eq!(default_string(Some("x"), "."), "x");
    }

    #[test]
    fn clip_matches_go_framing() {
        let s = "x".repeat(MAX_OUTPUT);
        assert_eq!(clip(&s), s);
        let long = "x".repeat(MAX_OUTPUT + 1);
        assert_eq!(clip(&long), format!("{s}\n...[output truncated]"));
    }

    #[cfg(unix)]
    #[test]
    fn go_io_error_messages() {
        let e = std::fs::read("/ryns-certainly-not-here/zz").unwrap_err();
        assert_eq!(
            go_io_error("open", "/p", &e),
            "open /p: no such file or directory"
        );
    }

    /// Honest per-OS errno labels: value 62 is ELOOP on macOS but ETIME on
    /// Linux; 40 is ELOOP on Linux but EMSGSIZE (= "message too long") on
    /// macOS. Windows must fall back to no table at all.
    #[test]
    fn errno_labels_are_per_os() {
        if cfg!(target_os = "macos") {
            assert_eq!(errno_text(62), Some("too many levels of symbolic links"));
            assert_eq!(errno_text(40), Some("message too long"));
        } else if cfg!(unix) {
            assert_eq!(errno_text(62), Some("timer expired"));
            assert_eq!(errno_text(40), Some("too many levels of symbolic links"));
        } else {
            // Windows: no numeric table (Display fallback via errno_display).
            assert_eq!(errno_text(2), None);
        }
    }

    #[test]
    fn errno_labels_share_common_values() {
        // Codes stable across unix flavors stay mapped on both.
        assert_eq!(errno_text(2), Some("no such file or directory"));
        assert_eq!(errno_text(13), Some("permission denied"));
    }

    /// Table for `path_clean_windows`. The function is pure (no fs access),
    /// so it is exercised directly on every target even though `path_clean`
    /// only dispatches to it on Windows hosts.
    #[test]
    fn path_clean_windows_cases() {
        // Verbatim / device prefixes are preserved untouched.
        assert_eq!(path_clean_windows(r"\\?\C:\x\y\.."), r"\\?\C:\x\y\..");
        assert_eq!(path_clean_windows(r"\\.\C:tools"), r"\\.\C:tools");
        assert_eq!(
            path_clean_windows(r"\\?\UNC\server\share\x"),
            r"\\?\UNC\server\share\x"
        );
        // Raw UNC roots: exactly two leading separators, remainder cleaned.
        assert_eq!(
            path_clean_windows(r"\\server\share\x\..\y"),
            r"\\server\share\y"
        );
        assert_eq!(path_clean_windows(r"\\server\share"), r"\\server\share");
        assert_eq!(path_clean_windows(r"\\\server\share"), r"\\server\share");
        // Volume-relative (Go: Clean("C:") == "C:.").
        assert_eq!(path_clean_windows("C:"), "C:.");
        assert_eq!(path_clean_windows(r"C:foo\b"), r"C:foo\b");
        // Rooted volume.
        assert_eq!(path_clean_windows(r"C:\a\b\.."), r"C:\a");
        assert_eq!(path_clean_windows(r"C:\"), r"C:\");
        // Plain relative / rooted.
        assert_eq!(path_clean_windows(r"a\b\.."), "a");
        assert_eq!(path_clean_windows(r"\"), r"\");
        assert_eq!(path_clean_windows(r"..\..\a"), r"..\..\a");
        assert_eq!(path_clean_windows(""), ".");
    }

    /// The unix algorithm keeps its shape (windows transforms are additive).
    #[test]
    fn path_clean_unix_unchanged_cases() {
        assert_eq!(path_clean_unix("a/./b/../c"), "a/c");
        assert_eq!(path_clean_unix("/../a"), "/a");
        assert_eq!(path_clean_unix("a/b/../../.."), "..");
    }
}

#[cfg(test)]
mod p2_helper_tests {
    use super::*;

    /// numberLines table incl. the Go loop's empty-window no-op.
    #[test]
    fn number_lines_matrix() {
        let v = vec!["a", "_SHORT_", "c"];
        assert_eq!(
            number_lines(&v, 1, 3),
            "     1\ta\n     2\t_SHORT_\n     3\tc\n"
        );
        assert_eq!(number_lines(&v, 4, 5), "");
        assert_eq!(number_lines(&v, 0, 2), "");
    }

    /// long-line truncation keeps the total byte count in the notice.
    #[test]
    fn number_lines_truncates_line_at_max_bytes() {
        let long = "y".repeat(MAX_LINE_LENGTH + 50);
        let v = vec![long.as_str()];
        let out = number_lines(&v, 1, 1);
        assert!(
            out.contains("...[line truncated, 2050 chars total]"),
            "{out}"
        );
    }

    #[test]
    fn parse_ymd_matrix() {
        assert_eq!(parse_ymd("2026-08-30"), Some((2026, 8, 30)));
        assert_eq!(parse_ymd("bad"), None);
        assert_eq!(parse_ymd("2026-13-01"), None);
    }

    #[test]
    fn ts_filename_is_stamped() {
        let stamp = ts_filename();
        // "YYYYMMDD-HHMMSS"
        assert_eq!(stamp.len(), 15, "stamp: {stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-', "stamp: {stamp}");
    }

    #[test]
    fn default_string_pending_semantics() {
        assert_eq!(default_string(Some(""), "."), ".");
        assert_eq!(default_string(Some("x"), "."), "x");
        assert_eq!(default_string(None, "."), ".");
    }
}

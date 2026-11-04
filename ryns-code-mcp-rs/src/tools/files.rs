//! File/search tools mirroring Go's `setupTools` entries for edit_file,
//! multi_edit_file, apply_patch, directory_tree, search_files, grep,
//! create_directory, delete_file, move_file, get_file_info.
//! Error convention: `"<prefix>_error: …"` text payloads (Go `toolError`).

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Ctx, default_string, go_io_error, must_json, number_lines, safe_path};
use crate::auth::child_env;
use crate::safety::GateResult;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditInput {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditOp {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MultiEditInput {
    pub path: String,
    pub edits: Vec<EditOp>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PatchInput {
    pub patch: String,
    #[serde(default, rename = "cwd")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TreeInput {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub max_depth: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchInput {
    #[serde(default)]
    pub path: Option<String>,
    pub pattern: String,
    #[serde(default)]
    pub max_results: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "glob")]
    pub glob: Option<String>,
    #[serde(default, rename = "ignore_case")]
    pub ignore_case: bool,
    #[serde(default)]
    pub context: i64,
    #[serde(default)]
    pub max_results: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MkdirInput {
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteInput {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveInput {
    pub source: String,
    pub destination: String,
}

/// Go's get_file_info reuses ListInput: `{"path"}` — safePath("") resolves
/// to the workspace root.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatInput {
    #[serde(default)]
    pub path: String,
}

/// Mirror of Go `contextAroundLine` (main.go ~208): a small numbered snippet
/// centered on line_num.
fn context_around_line(content: &str, line_num: i64, context: i64) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut start = line_num - context;
    if start < 1 {
        start = 1;
    }
    let mut end = line_num + context;
    if end > lines.len() as i64 {
        end = lines.len() as i64;
    }
    if start > lines.len() as i64 {
        return String::new();
    }
    number_lines(&lines, start, end)
}

/// edit_file (main.go ~1596).
pub async fn edit_file(ctx: &Ctx, input: EditInput) -> String {
    let p = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("edit_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on edit targets (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "edit_file", &[&p]).await {
        return text;
    }
    let bytes = match tokio::fs::read(&p).await {
        Ok(b) => b,
        Err(e) => return format!("edit_error: {}", go_io_error("open", &p, &e)),
    };
    let old = String::from_utf8_lossy(&bytes).into_owned();
    let count = old.matches(&input.old_text).count();
    if count == 0 {
        return "edit_error: old_text not found".to_string();
    }
    if !input.replace_all && count != 1 {
        return format!(
            "edit_error: {count} matches; make old_text unique or set replace_all=true"
        );
    }
    let line_num = super::line_of_offset(&old, old.find(&input.old_text).unwrap_or(0) as i64);
    let updated = if input.replace_all {
        old.replace(&input.old_text, &input.new_text)
    } else {
        old.replacen(&input.old_text, &input.new_text, 1)
    };
    if !input.dry_run
        && let Err(e) = super::write_file_mode(&p, updated.as_bytes()).await
    {
        return format!("edit_error: {}", go_io_error("open", &p, &e));
    }
    let n = if input.replace_all { count } else { 1 };
    let prefix = if input.dry_run { "dry-run: " } else { "" };
    let snippet = context_around_line(&updated, line_num, 2);
    format!("{prefix}replaced {n} occurrence(s) in {p} (near line {line_num})\n{snippet}")
}

/// multi_edit_file (main.go ~1638): atomic — every edit must match, else
/// nothing is written and the failing edit is reported.
pub async fn multi_edit_file(ctx: &Ctx, input: MultiEditInput) -> String {
    if input.edits.is_empty() {
        return "multi_edit_error: edits is empty".to_string();
    }
    let p = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("multi_edit_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on multi_edit targets (all modes audit).
    if let GateResult::Blocked(text) =
        crate::safety::gate_paths(ctx, "multi_edit_file", &[&p]).await
    {
        return text;
    }
    let bytes = match tokio::fs::read(&p).await {
        Ok(b) => b,
        Err(e) => return format!("multi_edit_error: {}", go_io_error("open", &p, &e)),
    };
    let mut cur = String::from_utf8_lossy(&bytes).into_owned();
    for (i, op) in input.edits.iter().enumerate() {
        let count = cur.matches(&op.old_text).count();
        if count == 0 {
            return format!(
                "multi_edit_error: edit {} of {} — old_text not found (no changes written)",
                i + 1,
                input.edits.len()
            );
        }
        if !op.replace_all && count != 1 {
            return format!(
                "multi_edit_error: edit {} of {} — {} matches; make old_text unique or set replace_all=true (no changes written)",
                i + 1,
                input.edits.len(),
                count
            );
        }
        if op.replace_all {
            cur = cur.replace(&op.old_text, &op.new_text);
        } else {
            cur = cur.replacen(&op.old_text, &op.new_text, 1);
        }
    }
    let prefix = if input.dry_run { "dry-run: " } else { "" };
    if !input.dry_run
        && let Err(e) = super::write_file_mode(&p, cur.as_bytes()).await
    {
        return format!("multi_edit_error: {}", go_io_error("open", &p, &e));
    }
    format!("{prefix}applied {} edit(s) to {p}", input.edits.len())
}

/// Extracts the file paths a unified diff touches: `+++` / `---` headers
/// (skipping `/dev/null`), plus `rename/copy from/to` git extended headers.
/// Paths are used as strings and stripped of the a// b/ prefixes; a leading
/// `\\` prefix `\\ No newline` zero-remnants are ignored. De-duplicated,
/// first-occurrence order.
pub fn patch_targets(patch: &str) -> Vec<String> {
    fn push(out: &mut Vec<String>, path: Option<String>) {
        if let Some(path) = path {
            if path.is_empty() || path == "/dev/null" || path == "\\ No newline" {
                return;
            }
            if !out.iter().any(|e| e == &path) {
                out.push(path);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let unpack = |raw: &str| -> Option<String> {
            let raw = raw.trim();
            if raw.is_empty() || raw == "/dev/null" {
                return None;
            }
            // Git quotes headers whose paths contain special characters
            // ("b/name with spaces"); strip optional quoting + a/ b/ prefix.
            let unquoted = if raw.starts_with('"') && raw.len() >= 2 && raw.ends_with('"') {
                raw[1..raw.len() - 1]
                    .replace("\\\\", "\\")
                    .replace("\\\"", "\"")
            } else {
                raw.to_string()
            };
            let body = unquoted
                .strip_prefix("a/")
                .or_else(|| unquoted.strip_prefix("b/"))
                .unwrap_or(&unquoted);
            Some(body.to_string())
        };
        if let Some(rest) = line.strip_prefix("+++ ") {
            push(&mut out, unpack(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            push(&mut out, unpack(rest));
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            push(&mut out, unpack(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            push(&mut out, unpack(rest));
        } else if let Some(rest) = line.strip_prefix("copy from ") {
            push(&mut out, unpack(rest));
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            push(&mut out, unpack(rest));
        }
    }
    out
}

/// Resolves `path` against the patch's working directory (the `-p1` root),
/// mirroring Go's filepath.Join semantics through `path_clean`. Absolute or
/// already-normalized targets pass through.
fn safe_path_from(workspace: &str, path: &str) -> Result<String, String> {
    if path.len() >= 2 && (path.starts_with('/') || path.starts_with("\\\\")) {
        return Ok(crate::tools::path_clean(path));
    }
    Ok(crate::tools::path_clean(&format!("{}/{}", workspace, path)))
}

/// apply_patch (main.go ~1676): feeds the patch to the system `patch` tool on
/// stdin, replicating Go's CombinedOutput + ExitError handling.
pub async fn apply_patch(ctx: &Ctx, input: PatchInput) -> String {
    let dir = match safe_path(ctx, &default_string(input.cwd.as_deref(), ".")) {
        Ok(d) => d,
        Err(e) => return format!("patch_error: {e}"),
    };
    // Phase 3 gate (sidecar, all modes audit): the checked command is the
    // real primary invocation ("patch -p1 [--dry-run]").
    let mut command_str = String::from("patch -p1");
    let mut args: Vec<String> = vec!["-p1".into()];
    if input.dry_run {
        args.push("--dry-run".to_string());
        command_str.push_str(" --dry-run");
    }
    if let GateResult::Blocked(text) =
        crate::safety::gate_shell(ctx, "apply_patch", &command_str, &dir).await
    {
        return text;
    }
    // Phase 3 gate (secret-path rules): every path the patch text references
    // goes through the SAME deny→ASK flow as write_file — a patch that
    // rewrites .env must open a pending review (frozen rule coverage).
    for target in patch_targets(&input.patch) {
        let path = match safe_path_from(&dir, &target) {
            Ok(p) => p,
            Err(_) => continue, // unresolvable target: patch itself will fail
        };
        if let GateResult::Blocked(text) =
            crate::safety::gate_paths(ctx, "apply_patch", &[&path]).await
        {
            return text;
        }
    }
    let child_env = child_env();
    let patch = input.patch;
    let out = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("patch");
        cmd.args(&args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env_clear();
        for (k, v) in &child_env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Result::<_, String>::Err(format!("start patch in {dir}: {e}")),
        };
        // Patch may stream output while reading stdin; write in a task-like
        // fashion: drop stdin after writing so patch sees EOF.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(patch.as_bytes());
        }
        match child.wait_with_output() {
            Ok(o) => {
                let mut combined = o.stdout;
                combined.extend_from_slice(&o.stderr);
                Ok((
                    String::from_utf8_lossy(&combined).into_owned(),
                    o.status.code(),
                ))
            }
            Err(e) => Err(format!("patch: {e}")),
        }
    })
    .await;
    match out {
        Ok(Ok((text, Some(0)))) => format!("exit_code: 0\n{text}"),
        Ok(Ok((text, Some(c)))) => format!("exit_code: {c}\n{text}"),
        // Go prints ExitCode() = -1 when the process was signaled.
        Ok(Ok((text, None))) => format!("exit_code: -1\n{text}"),
        // Outer Err is a JoinError (task panic); inner Err is the spawn/exec
        // failure — both map to Go's toolError wording.
        Ok(Err(e)) => format!("patch_error: {e}"),
        Err(je) => format!("patch_error: join error: {je}"),
    }
}

/// directory_tree (main.go ~1728). NOTE: Go emits "[FILE]"+Name without a
/// space (unlike list_directory's "[FILE] …") — mirrored verbatim.
pub async fn directory_tree(ctx: &Ctx, input: TreeInput) -> String {
    let root = match safe_path(ctx, &default_string(input.path.as_deref(), ".")) {
        Ok(r) => r,
        Err(e) => return format!("tree_error: {e}"),
    };
    let depth = match input.max_depth {
        Some(d) if d > 0 => d,
        _ => 6,
    };

    // Boxed for async recursion (Go's `walk` recurses the same way).
    async fn walk(d: String, n: i64, depth: i64) -> Result<Vec<String>, String> {
        let mut rd = match tokio::fs::read_dir(&d).await {
            Ok(rd) => rd,
            Err(e) => return Err(go_io_error("open", &d, &e)),
        };
        let mut kids: Vec<(String, bool)> = Vec::new();
        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    kids.push((name, is_dir));
                }
                Ok(None) => break,
                Err(e) => return Err(go_io_error("readdirent", &d, &e)),
            }
        }
        kids.sort_by(|a, b| a.0.cmp(&b.0));
        let mut rows: Vec<String> = Vec::new();
        for (name, is_dir) in kids {
            let prefix = "  ".repeat(n as usize);
            if is_dir {
                rows.push(format!("{prefix}[DIR] {name}"));
                if n < depth {
                    rows.extend(Box::pin(walk(super::path_join(&d, &name), n + 1, depth)).await?);
                }
            } else {
                rows.push(format!("{prefix}[FILE]{name}"));
            }
        }
        Ok(rows)
    }

    match walk(root.clone(), 0, depth).await {
        Ok(rows) => format!("{root}\n{}", rows.join("\n")),
        Err(e) => format!("tree_error: {e}"),
    }
}

/// search_files (main.go ~1769): `rg --files --glob <pattern> <path>`.
pub async fn search_files(ctx: &Ctx, input: SearchInput) -> String {
    let p = match safe_path(ctx, &default_string(input.path.as_deref(), ".")) {
        Ok(p) => p,
        Err(e) => return format!("search_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on the search root (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "search_files", &[&p]).await {
        return text;
    }
    let mut max = input.max_results.unwrap_or(0);
    if max <= 0 {
        max = 500;
    }
    let args = vec![
        "--files".to_string(),
        "--glob".to_string(),
        input.pattern,
        p.clone(),
    ];
    run_ripgrep(args, &p, "search_error", max).await
}

/// grep (main.go ~1797).
pub async fn grep(ctx: &Ctx, input: GrepInput) -> String {
    let p = match safe_path(ctx, &default_string(input.path.as_deref(), ".")) {
        Ok(p) => p,
        Err(e) => return format!("grep_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on the grep root (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "grep", &[&p]).await {
        return text;
    }
    let mut max = input.max_results.unwrap_or(0);
    if max <= 0 {
        max = 500;
    }
    let mut args: Vec<String> = vec![
        "-n".into(),
        "--color=never".into(),
        format!("--max-count={max}"),
    ];
    if input.ignore_case {
        args.push("-i".into());
    }
    if input.context > 0 {
        args.push(format!("-C{}", input.context));
    }
    if let Some(glob) = input.glob.as_deref().filter(|g| !g.is_empty()) {
        args.push("--glob".into());
        args.push(glob.to_string());
    }
    args.push(input.pattern);
    args.push(p.clone());
    run_ripgrep(args, &p, "grep_error", -1).await
}

/// Shared runner for search_files/grep. Go `cmd.Output()` semantics: success
/// → stdout text (search_files trims/caps to max); exit 1 → "(no matches)";
/// other exits → `<prefix>_error: exit status N` (stderr is NOT printed —
/// Go's ExitError.Error() carries only the status).
async fn run_ripgrep(args: Vec<String>, _p: &str, prefix: &str, max: i64) -> String {
    let child_env = child_env();
    let res = tokio::task::spawn_blocking(move || {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("rg");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env_clear();
        for (k, v) in &child_env {
            cmd.env(k, v);
        }
        cmd.output()
    })
    .await;
    match res {
        Err(e) => format!("{prefix}_error: {e}"),
        Ok(Err(e)) => format!("{prefix}_error: {e}"),
        Ok(Ok(o)) => match o.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                if max >= 0 {
                    let lines: Vec<&str> = text.trim().split('\n').take(max as usize).collect();
                    if lines.len() == 1 && lines[0].is_empty() {
                        return "(no matches)".to_string();
                    }
                    return lines.join("\n");
                }
                text
            }
            Some(1) => "(no matches)".to_string(),
            Some(c) => format!("{prefix}_error: exit status {c}"),
            None => format!("{prefix}_error: signal: terminated"),
        },
    }
}

/// create_directory (main.go ~1829).
pub async fn create_directory(ctx: &Ctx, input: MkdirInput) -> String {
    let p = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("mkdir_error: {e}"),
    };
    match tokio::fs::create_dir_all(&p).await {
        Err(e) => format!("mkdir_error: {}", go_io_error("mkdir", &p, &e)),
        Ok(()) => format!("created/exists: {p}"),
    }
}

/// delete_file (main.go ~1839).
pub async fn delete_file(ctx: &Ctx, input: DeleteInput) -> String {
    let p = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("delete_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on delete targets (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "delete_file", &[&p]).await {
        return text;
    }
    let info = match tokio::fs::metadata(&p).await {
        Ok(i) => i,
        Err(e) => return format!("delete_error: {}", go_io_error("stat", &p, &e)),
    };
    if info.is_dir() && !input.recursive {
        return "delete_error: directory requires recursive=true".to_string();
    }
    let res = if info.is_dir() {
        tokio::fs::remove_dir_all(&p).await
    } else {
        tokio::fs::remove_file(&p).await
    };
    match res {
        Err(e) => format!("delete_error: {}", go_io_error("remove", &p, &e)),
        Ok(()) => format!("deleted: {p}"),
    }
}

/// move_file (main.go ~1856).
pub async fn move_file(ctx: &Ctx, input: MoveInput) -> String {
    let a = match safe_path(ctx, &input.source) {
        Ok(a) => a,
        Err(e) => return format!("move_error: {e}"),
    };
    let b = match safe_path(ctx, &input.destination) {
        Ok(b) => b,
        Err(e) => return format!("move_error: {e}"),
    };
    // Phase 3 gate: secret-path rules on move endpoints (all modes audit).
    if let GateResult::Blocked(text) = crate::safety::gate_paths(ctx, "move_file", &[&a, &b]).await
    {
        return text;
    }
    match tokio::fs::rename(&a, &b).await {
        Err(e) => format!("move_error: {}", go_io_error("rename", &a, &e)),
        Ok(()) => format!("moved {a} -> {b}"),
    }
}

/// get_file_info (main.go ~1871): Go prints `mode` via
/// `fmt.Sprintf("%o", s.Mode().Perm())` and created/modified both from
/// ModTime in RFC3339Nano.
pub async fn get_file_info(ctx: &Ctx, input: StatInput) -> String {
    let p = match safe_path(ctx, &input.path) {
        Ok(p) => p,
        Err(e) => return format!("stat_error: {e}"),
    };
    let info = match tokio::fs::metadata(&p).await {
        Ok(i) => i,
        Err(e) => return format!("stat_error: {}", go_io_error("stat", &p, &e)),
    };
    let kind = if info.is_dir() { "directory" } else { "file" };
    let modified = info.modified().map(super::rfc3339_nano).unwrap_or_default();
    let mode: String = unix_or_windows_perm(&info);
    let value = serde_json::json!({
        "path": p,
        "type": kind,
        "size": info.len(),
        "mode": mode,
        "created": modified,
        "modified": modified,
    });
    must_json(&value)
}

/// Go's `(s.Mode().Perm())` rendering per OS: unix — real permission bits;
/// windows — Go synthesizes 0666/0777 (dir) with 0222 removed when read-only.
fn unix_or_windows_perm(info: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        format!("{:o}", info.permissions().mode() & 0o777)
    }
    #[cfg(windows)]
    {
        // std::fs::Metadata has no is_readonly(); Files::metadata on
        // Windows exposes read-only via Permissions::readonly().
        return format!(
            "{:o}",
            if info.permissions().readonly() {
                0o444
            } else {
                0o666
            }
        );
    }
}

// ---- patch target extraction (Phase 3 gate) --------------------------------

#[test]
fn patch_targets_table() {
    let patch = "--- a/.env\n+++ b/.env\n@@ -1,1 +1,1 @@\n-A=1\n+A=2\n";
    assert_eq!(patch_targets(patch), vec![".env"]);
    let patch2 = "diff --git a/notes.md b/notes.md
--- a/notes.md
+++ b/notes.md
diff --git a/old name.txt b/newer name 2.txt
rename from old name.txt
rename to \"newer name 2.txt\"
diff --git a/.env b/.env
--- a/.env
+++ b/.env
diff --git a/deleted.txt b/deleted.txt
deleted file mode 100644
--- a/deleted.txt
+++ /dev/null
";
    assert_eq!(
        patch_targets(patch2),
        vec![
            "notes.md",
            "old name.txt",
            "newer name 2.txt",
            ".env",
            "deleted.txt"
        ]
    );
    assert_eq!(patch_targets("ref patch"), Vec::<String>::new());
}

#[test]
fn safe_path_from_table() {
    assert_eq!(
        safe_path_from("/ws/a", "sub/.env").unwrap(),
        "/ws/a/sub/.env"
    );
    assert_eq!(safe_path_from("/ws/a", "/abs/.env").unwrap(), "/abs/.env");
    assert_eq!(safe_path_from("/ws/a", "../b/x").unwrap(), "/ws/b/x");
}

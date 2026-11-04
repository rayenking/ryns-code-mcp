//! Todo tracker (main.go ~1115-1290): ONE global checklist at
//! `assets/data/todos.json` with the Go drop-in layout
//! `{"items":[{"id","content","status","active_form"}],"updated_at"}`,
//! replaced wholesale by todo_write and rendered by todo_read.
//! Go's MarshalIndent preserves struct field order here too — serialized via
//! structs, not maps.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use super::{Ctx, rfc3339_secs};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    #[serde(rename = "active_form")]
    pub active_form: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoState {
    #[serde(default)]
    pub items: Vec<TodoItem>,
    #[serde(default, rename = "updated_at")]
    pub updated_at: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoItemInput {
    pub content: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "active_form")]
    pub active_form: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoWriteInput {
    #[serde(default)]
    pub items: Vec<TodoItemInput>,
}

/// todo_read takes no arguments (Go: struct{}); marker for the mcp wrapper.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TodoReadInput {}

fn todo_file_path(workspace: &str) -> PathBuf {
    PathBuf::from(workspace)
        .join("assets")
        .join("data")
        .join("todos.json")
}

fn load_todo_state(workspace: &str) -> Result<TodoState, String> {
    let data = match std::fs::read_to_string(todo_file_path(workspace)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TodoState::default());
        }
        Err(e) => return Err(e.to_string()),
    };
    serde_json::from_str(&data).map_err(|e| e.to_string())
}

fn save_todo_state(workspace: &str, mut st: TodoState) -> Result<(), String> {
    let dir = todo_file_path(workspace).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    st.updated_at = rfc3339_secs(std::time::SystemTime::now());
    let data = serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?;
    std::fs::write(todo_file_path(workspace), data).map_err(|e| e.to_string())
}

/// Mirror of Go `renderTodoList` (main.go ~1260): status markers
/// (" " pending, "~" in_progress with active_form label, "x" completed), the
/// multi-in-progress note, and the "diupdate <ts>" header.
pub fn render_todo_list(st: &TodoState) -> String {
    if st.items.is_empty() {
        return "Belum ada todo list aktif.".to_string();
    }
    let mut sb = String::new();
    if !st.updated_at.is_empty() {
        sb.push_str(&format!("Todo List (diupdate {})\n", st.updated_at));
    } else {
        sb.push_str("Todo List\n");
    }
    let mut in_progress_count = 0;
    for (i, item) in st.items.iter().enumerate() {
        let mut mark = " ";
        let mut label = item.content.clone();
        match item.status.as_str() {
            "in_progress" => {
                mark = "~";
                in_progress_count += 1;
                if !item.active_form.is_empty() {
                    label = item.active_form.clone();
                }
            }
            "completed" => mark = "x",
            _ => {}
        }
        sb.push_str(&format!("[{}] {}. {}\n", mark, i + 1, label));
    }
    if in_progress_count > 1 {
        sb.push_str(
            "\nCatatan: lebih dari satu item berstatus in_progress sekaligus; idealnya cuma satu yang dikerjain di satu waktu.\n",
        );
    }
    sb
}

/// todo_write (main.go ~2553): validate + renumber (ids "1".."n") + save +
/// re-read and render the saved state.
pub async fn todo_write(ctx: &Ctx, input: TodoWriteInput) -> String {
    let ws = ctx.workspace.to_string();
    let mut items: Vec<TodoItem> = Vec::with_capacity(input.items.len());
    for (i, it) in input.items.iter().enumerate() {
        let content = it.content.trim().to_string();
        if content.is_empty() {
            return format!(
                "todo_write failed: item ke-{}: content tidak boleh kosong",
                i + 1
            );
        }
        let status = if it.status.is_empty() {
            "pending".to_string()
        } else {
            it.status.clone()
        };
        if !matches!(status.as_str(), "pending" | "in_progress" | "completed") {
            return format!(
                "todo_write failed: item ke-{}: status {:?} tidak valid, harus pending/in_progress/completed",
                i + 1,
                it.status
            );
        }
        let active_form = match &it.active_form {
            Some(a) if !a.is_empty() => a.clone(),
            _ => content.clone(),
        };
        items.push(TodoItem {
            id: (i + 1).to_string(),
            content,
            status,
            active_form,
        });
    }
    if let Err(e) = save_todo_state(
        &ws,
        TodoState {
            items,
            updated_at: String::new(),
        },
    ) {
        return format!("todo_write failed: {e}");
    }
    match load_todo_state(&ws) {
        Ok(saved) => render_todo_list(&saved),
        Err(e) => format!("todo_write failed: {e}"),
    }
}

/// todo_read (main.go ~2589).
pub async fn todo_read(ctx: &Ctx) -> String {
    match load_todo_state(ctx.workspace.as_ref()) {
        Ok(st) => render_todo_list(&st),
        Err(e) => format!("todo_read failed: {e}"),
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn item(content: &str, status: &str, active: Option<&str>) -> TodoItem {
        TodoItem {
            id: "1".into(),
            content: content.into(),
            status: status.into(),
            active_form: active
                .map(String::from)
                .unwrap_or_else(|| content.to_string()),
        }
    }

    #[test]
    fn empty_state_line() {
        assert_eq!(
            render_todo_list(&TodoState::default()),
            "Belum ada todo list aktif."
        );
    }

    /// mark/label behavior incl. the >1 in-progress note (Go render loop).
    #[test]
    fn marks_and_note() {
        let st = TodoState {
            items: vec![
                item("first", "in_progress", Some("firsting")),
                item("second", "pending", None),
                item("third", "in_progress", None),
                item("fourth", "completed", None),
            ],
            updated_at: "2026-11-03T13:00:00Z".into(),
        };
        let out = render_todo_list(&st);
        assert!(out.contains("Todo List (diupdate 2026-11-03T13:00:00Z)\n"));
        assert!(out.contains("[~] 1. firsting\n"));
        assert!(out.contains("[ ] 2. second\n"));
        assert!(out.contains("[~] 3. third\n")); // active_form fallback = content
        assert!(out.contains("[x] 4. fourth\n"));
        assert!(out.contains(
            "\nCatatan: lebih dari satu item berstatus in_progress sekaligus; idealnya cuma satu yang dikerjain di satu waktu.\n"
        ));
    }

    #[test]
    fn single_in_progress_has_no_note() {
        let st = TodoState {
            items: vec![item("only", "in_progress", None)],
            updated_at: "".into(),
        };
        let out = render_todo_list(&st);
        // No timestamp → plain header (Go: "Todo List\n").
        assert!(out.starts_with("Todo List\n"));
        assert!(!out.contains("Catatan"), "got {out:?}");
    }
}

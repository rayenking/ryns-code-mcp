//! LocalSheet tools (main.go ~268-449 + sheet handlers ~2259-2503) with the
//! EXACT DROP-IN JSON layout of the Go server: `assets/data/sheets/_index.json`
//! (`{"sheets":[{"id","title"}]}`) and `assets/data/sheets/<id>.json`
//! (`{"id","title","columns","rows","note"(omit-empty),"created_at",
//! "updated_at"}`), MarshalIndent-similar pretty printing with sorted keys.
//!
//! Go's MarshalIndent on structs PRESERVES struct field order (not sorted!).
//! To stay layout-parity with Go sheet files, serialize Sheet/Index via a
//! struct with the same field order (not a map).

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use super::{Ctx, rfc3339_secs};

// ---- wire types (field order = Go struct order) ------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sheet {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SheetIndexEntry {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SheetIndex {
    #[serde(default)]
    pub sheets: Vec<SheetIndexEntry>,
}

/// Mutex so tool mutations are serialized like Go's tool handlers (single
/// request goroutine, but two rapid calls must still yield consistent state).
fn save_lock() -> &'static StdMutex<()> {
    static L: StdMutex<()> = StdMutex::new(());
    &L
}

// Shared workspace context — sheets need the workspace path but also a Ctx.
// ctx.workspace is the Arc<str>. Workspace path resolution identical to Go's
// `workspace` package var used by sheetsDataDir().
fn workspace_path(ctx: &Ctx) -> String {
    ctx.workspace.to_string()
}

fn sheets_data_dir(workspace: &str) -> PathBuf {
    PathBuf::from(workspace)
        .join("assets")
        .join("data")
        .join("sheets")
}

fn sheet_index_path(workspace: &str) -> PathBuf {
    sheets_data_dir(workspace).join("_index.json")
}

fn sheet_file_path(workspace: &str, id: &str) -> PathBuf {
    sheets_data_dir(workspace).join(format!("{id}.json"))
}

pub fn valid_sheet_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(
            "id sheet harus non-kosong dan hanya boleh berisi huruf, angka, underscore, atau dash"
                .into(),
        )
    }
}

/// Pretty-print with Go's MarshalIndent field order for the wire structs.
fn marshal(v: impl Serialize) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "null".into())
}

fn load_sheet_index(workspace: &str) -> Result<SheetIndex, String> {
    let p = sheet_index_path(workspace);
    let data = match std::fs::read_to_string(&p) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SheetIndex::default()),
        Err(e) => return Err(e.to_string()),
    };
    let idx: SheetIndex = serde_json::from_str(&data).map_err(|e| format!("{e}"))?;
    Ok(idx)
}

fn save_sheet_index(workspace: &str, idx: &SheetIndex) -> Result<(), String> {
    let dir = sheets_data_dir(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(sheet_index_path(workspace), marshal(idx)).map_err(|e| e.to_string())
}

fn upsert_index_entry(workspace: &str, id: &str, title: &str) -> Result<(), String> {
    let mut idx = load_sheet_index(workspace)?;
    if let Some(e) = idx.sheets.iter_mut().find(|e| e.id == id) {
        e.title = title.to_string();
    } else {
        idx.sheets.push(SheetIndexEntry {
            id: id.to_string(),
            title: title.to_string(),
        });
    }
    save_sheet_index(workspace, &idx)
}

fn remove_index_entry(workspace: &str, id: &str) -> Result<(), String> {
    let mut idx = load_sheet_index(workspace)?;
    idx.sheets.retain(|e| e.id != id);
    save_sheet_index(workspace, &idx)
}

fn load_sheet(workspace: &str, id: &str) -> Result<Sheet, String> {
    let data =
        std::fs::read_to_string(sheet_file_path(workspace, id)).map_err(|e| e.to_string())?;
    serde_json::from_str(&data).map_err(|e| e.to_string())
}

fn save_sheet(workspace: &str, mut s: Sheet) -> Result<(), String> {
    let dir = sheets_data_dir(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    s.updated_at = rfc3339_secs(std::time::SystemTime::now());
    std::fs::write(sheet_file_path(workspace, &s.id), marshal(&s)).map_err(|e| e.to_string())
}

/// resolveColumnIndex (main.go ~407): name or stringified zero-based index.
fn resolve_column_index(columns: &[String], r: &str) -> Result<usize, String> {
    if let Ok(n) = r.parse::<i64>() {
        if n < 0 || n as usize >= columns.len() {
            return Err(format!(
                "index kolom {} di luar jangkauan (0..{})",
                n,
                (columns.len() as i64) - 1
            ));
        }
        return Ok(n as usize);
    }
    for (i, c) in columns.iter().enumerate() {
        if c == r {
            return Ok(i);
        }
    }
    Err(format!("kolom {r:?} tidak ditemukan"))
}

/// renderSheetTable (main.go ~424), optionally row-scoped [start, end).
pub fn render_sheet_table(s: &Sheet, start_row: i64, end_row: i64) -> String {
    let mut sb = String::new();
    sb.push_str(&format!("Sheet: {} ({})\n", s.id, s.title));
    if !s.note.is_empty() {
        sb.push_str(&format!("Note: {}\n", s.note));
    }
    sb.push_str(&format!("Columns: {}\n\n", s.columns.join(" | ")));
    if s.rows.is_empty() {
        sb.push_str("(belum ada baris)\n");
        return sb;
    }
    let mut start = start_row;
    if start < 0 {
        start = 0;
    }
    let mut end = end_row;
    if end <= 0 || end as usize > s.rows.len() {
        end = s.rows.len() as i64;
    }
    if start >= end {
        sb.push_str("(range baris kosong)\n");
        return sb;
    }
    for i in start..end {
        sb.push_str(&format!("[{}] {}\n", i, s.rows[i as usize].join(" | ")));
    }
    sb
}

// ---- tool input types (field names mirror Go) -------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetCreateInput {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub overwrite: bool,
}

/// sheet_list takes no arguments (Go: struct{}); a marker input keeps the
/// rmcp `Parameters<T>` plumbing happy.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SheetListInput {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetIdInput {
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetGetInput {
    pub id: String,
    #[serde(default)]
    pub start_row: Option<i64>,
    #[serde(default)]
    pub end_row: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetRenameInput {
    pub id: String,
    pub new_title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColumnAddInput {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub after_column: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColumnRemoveInput {
    pub id: String,
    pub column: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColumnRenameInput {
    pub id: String,
    pub old_name: String,
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RowAddInput {
    pub id: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RowRemoveInput {
    pub id: String,
    pub row_index: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CellSetInput {
    pub id: String,
    pub row_index: i64,
    pub column: String,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CellGetInput {
    pub id: String,
    pub row_index: i64,
    pub column: String,
}

// ---- tool handlers -----------------------------------------------------------

/// sheet_create (main.go ~2259).
pub async fn sheet_create(ctx: &Ctx, input: SheetCreateInput) -> String {
    let _guard = save_lock().lock().unwrap();
    if let Err(e) = valid_sheet_id(&input.id) {
        return format!("sheet_create failed: {e}");
    }
    if input.title.trim().is_empty() {
        return "sheet_create failed: title tidak boleh kosong".to_string();
    }
    let ws = workspace_path(ctx);
    if sheet_file_path(&ws, &input.id).is_file() && !input.overwrite {
        return format!(
            "sheet_create failed: sheet {:?} sudah ada; set overwrite=true untuk menimpa",
            input.id
        );
    }
    let columns = input.columns.unwrap_or_default();
    let now = rfc3339_secs(std::time::SystemTime::now());
    let s = Sheet {
        id: input.id.clone(),
        title: input.title.clone(),
        columns: columns.clone(),
        rows: Vec::new(),
        note: String::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    if let Err(e) = save_sheet(&ws, s) {
        return format!("sheet_create failed: {e}");
    }
    if let Err(e) = upsert_index_entry(&ws, &input.id, &input.title) {
        return format!("sheet_create failed: {e}");
    }
    format!(
        "Sheet {:?} ({}) dibuat dengan {} kolom.",
        input.id,
        input.title,
        columns.len()
    )
}

/// sheet_list (main.go ~2284).
pub async fn sheet_list(ctx: &Ctx) -> String {
    let ws = workspace_path(ctx);
    let idx = match load_sheet_index(&ws) {
        Ok(i) => i,
        Err(e) => return format!("sheet_list failed: {e}"),
    };
    if idx.sheets.is_empty() {
        return "Belum ada sheet.".to_string();
    }
    let mut sb = String::new();
    for e in &idx.sheets {
        match load_sheet(&ws, &e.id) {
            Ok(s) => sb.push_str(&format!(
                "{} — {} ({} baris, {} kolom)\n",
                s.id,
                s.title,
                s.rows.len(),
                s.columns.len()
            )),
            Err(err) => sb.push_str(&format!("{} — {} (gagal dibaca: {err})\n", e.id, e.title)),
        }
    }
    sb
}

/// sheet_get (main.go ~2304).
pub async fn sheet_get(ctx: &Ctx, input: SheetGetInput) -> String {
    let ws = workspace_path(ctx);
    match load_sheet(&ws, &input.id) {
        Ok(s) => render_sheet_table(&s, input.start_row.unwrap_or(0), input.end_row.unwrap_or(0)),
        Err(e) => format!("sheet_get failed: {e}"),
    }
}

/// sheet_delete (main.go ~2312).
pub async fn sheet_delete(ctx: &Ctx, input: SheetIdInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    match std::fs::remove_file(sheet_file_path(&ws, &input.id)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return format!("sheet_delete failed: {e}"),
        Ok(()) => {}
    }
    if let Err(e) = remove_index_entry(&ws, &input.id) {
        return format!("sheet_delete failed: {e}");
    }
    format!("Sheet {:?} dihapus.", input.id)
}

/// sheet_rename (main.go ~2322).
pub async fn sheet_rename(ctx: &Ctx, input: SheetRenameInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("sheet_rename failed: {e}"),
    };
    s.title = input.new_title.clone();
    if let Err(e) = save_sheet(&ws, s) {
        return format!("sheet_rename failed: {e}");
    }
    if let Err(e) = upsert_index_entry(&ws, &input.id, &input.new_title) {
        return format!("sheet_rename failed: {e}");
    }
    format!(
        "Sheet {:?} diganti judulnya jadi {:?}.",
        input.id, input.new_title
    )
}

/// column_add (main.go ~2337).
pub async fn column_add(ctx: &Ctx, input: ColumnAddInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("column_add failed: {e}"),
    };
    if input.name.trim().is_empty() {
        return "column_add failed: nama kolom tidak boleh kosong".to_string();
    }
    let insert_at = if input.after_column.is_empty() {
        s.columns.len()
    } else {
        match resolve_column_index(&s.columns, &input.after_column) {
            Ok(i) => i + 1,
            Err(e) => return format!("column_add failed: {e}"),
        }
    };
    let insert_at = insert_at.min(s.columns.len());
    s.columns.insert(insert_at, input.name.clone());
    for row in &mut s.rows {
        let cut = insert_at.min(row.len());
        row.insert(cut, String::new());
    }
    if let Err(e) = save_sheet(&ws, s.clone()) {
        return format!("column_add failed: {e}");
    }
    format!(
        "Kolom {:?} ditambahkan ke sheet {:?} pada posisi {}.",
        input.name, input.id, insert_at
    )
}

/// column_remove (main.go ~2377).
pub async fn column_remove(ctx: &Ctx, input: ColumnRemoveInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("column_remove failed: {e}"),
    };
    let idx = match resolve_column_index(&s.columns, &input.column) {
        Ok(i) => i,
        Err(e) => return format!("column_remove failed: {e}"),
    };
    let removed = s.columns[idx].clone();
    s.columns.remove(idx);
    for row in &mut s.rows {
        if idx < row.len() {
            row.remove(idx);
        }
    }
    if let Err(e) = save_sheet(&ws, s) {
        return format!("column_remove failed: {e}");
    }
    format!("Kolom {:?} dihapus dari sheet {:?}.", removed, input.id)
}

/// column_rename (main.go ~2399).
pub async fn column_rename(ctx: &Ctx, input: ColumnRenameInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("column_rename failed: {e}"),
    };
    let idx = match resolve_column_index(&s.columns, &input.old_name) {
        Ok(i) => i,
        Err(e) => return format!("column_rename failed: {e}"),
    };
    let old = s.columns[idx].clone();
    s.columns[idx] = input.new_name.clone();
    if let Err(e) = save_sheet(&ws, s) {
        return format!("column_rename failed: {e}");
    }
    format!(
        "Kolom {:?} di sheet {:?} diganti jadi {:?}.",
        old, input.id, input.new_name
    )
}

/// row_add (main.go ~2416).
pub async fn row_add(ctx: &Ctx, input: RowAddInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("row_add failed: {e}"),
    };
    let pos = match input.position {
        Some(p) => {
            if p < 0 || p as usize > s.rows.len() {
                return format!(
                    "row_add failed: position {} di luar jangkauan (0..{})",
                    p,
                    s.rows.len()
                );
            }
            p as usize
        }
        None => s.rows.len(),
    };
    s.rows.insert(pos, input.values.clone());
    if let Err(e) = save_sheet(&ws, s) {
        return format!("row_add failed: {e}");
    }
    format!(
        "Baris baru ditambahkan ke sheet {:?} pada posisi {}.",
        input.id, pos
    )
}

/// row_remove (main.go ~2440).
pub async fn row_remove(ctx: &Ctx, input: RowRemoveInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("row_remove failed: {e}"),
    };
    if input.row_index < 0 || input.row_index as usize >= s.rows.len() {
        return format!(
            "row_remove failed: row_index {} di luar jangkauan (0..{})",
            input.row_index,
            s.rows.len() as i64 - 1
        );
    }
    s.rows.remove(input.row_index as usize);
    if let Err(e) = save_sheet(&ws, s) {
        return format!("row_remove failed: {e}");
    }
    format!(
        "Baris {} dihapus dari sheet {:?}.",
        input.row_index, input.id
    )
}

/// cell_set (main.go ~2455): pads the row with "" until the column exists.
pub async fn cell_set(ctx: &Ctx, input: CellSetInput) -> String {
    let _guard = save_lock().lock().unwrap();
    let ws = workspace_path(ctx);
    let mut s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("cell_set failed: {e}"),
    };
    if input.row_index < 0 || input.row_index as usize >= s.rows.len() {
        return format!(
            "cell_set failed: row_index {} di luar jangkauan (0..{})",
            input.row_index,
            s.rows.len() as i64 - 1
        );
    }
    let col_idx = match resolve_column_index(&s.columns, &input.column) {
        Ok(i) => i,
        Err(e) => return format!("cell_set failed: {e}"),
    };
    let row = &mut s.rows[input.row_index as usize];
    while row.len() <= col_idx {
        row.push(String::new());
    }
    row[col_idx] = input.value.clone();
    let col_name = s.columns[col_idx].clone();
    if let Err(e) = save_sheet(&ws, s) {
        return format!("cell_set failed: {e}");
    }
    format!(
        "Sel [{}, {}] di sheet {:?} diset jadi {:?}.",
        input.row_index, col_name, input.id, input.value
    )
}

/// cell_get (main.go ~2479): "" when the column doesn't exist in the row.
pub async fn cell_get(ctx: &Ctx, input: CellGetInput) -> String {
    let ws = workspace_path(ctx);
    let s = match load_sheet(&ws, &input.id) {
        Ok(s) => s,
        Err(e) => return format!("cell_get failed: {e}"),
    };
    if input.row_index < 0 || input.row_index as usize >= s.rows.len() {
        return format!(
            "cell_get failed: row_index {} di luar jangkauan (0..{})",
            input.row_index,
            s.rows.len() as i64 - 1
        );
    }
    let col_idx = match resolve_column_index(&s.columns, &input.column) {
        Ok(i) => i,
        Err(e) => return format!("cell_get failed: {e}"),
    };
    let row = &s.rows[input.row_index as usize];
    if col_idx < row.len() {
        row[col_idx].clone()
    } else {
        String::new()
    }
}

/// sheet_url (main.go ~2499).
pub async fn sheet_url(ctx: &Ctx, input: SheetIdInput) -> String {
    let ws = workspace_path(ctx);
    if let Err(e) = load_sheet(&ws, &input.id) {
        return format!("sheet_url failed: {e}");
    }
    format!(
        "{}/assets/sheet.html?id={}",
        super::public_base_url(),
        input.id
    )
}

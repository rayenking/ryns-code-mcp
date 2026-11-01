# PLAN: Todo Tracker (todo_write / todo_read) untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.2, 2026-08-31). `todo_write` dan `todo_read`
> udah jalan, dites lewat `tools/call` langsung (render checklist, validasi
> status invalid, kosongin list) — semua sesuai desain di bawah.

## Tujuan
Tambah 2 tool MCP baru: **`todo_write`** dan **`todo_read`** — checklist
task tracker mirip `TodoWrite`-nya Claude Code, biar pas ngerjain task
multi-step keliatan progress-nya ("langkah 3/7 selesai"), bukan cuma
diomongin di teks doang.

## Kenapa full-replace, bukan incremental add/update
Ngikutin desain asli Claude Code punya: `todo_write` selalu terima ARRAY
LENGKAP dari seluruh todo list saat ini (bukan cuma 1 item), dan itu
meng-overwrite state lama sepenuhnya. Ini sengaja biar simpel — ga perlu
ID matching yang ribet, pemanggil (Claude) tinggal kirim ulang array
dengan status yang udah diupdate tiap kali ada progress.

## Desain data
Persist ke `assets/data/todos.json` (satu file global, bukan per-sheet
kayak LocalSheet — todo list ini sifatnya satu aktif per waktu).

```go
type TodoItem struct {
	ID         string `json:"id"`
	Content    string `json:"content"`
	Status     string `json:"status"`      // pending | in_progress | completed
	ActiveForm string `json:"active_form"` // present-continuous, ditampilin pas in_progress
}
type TodoState struct {
	Items     []TodoItem `json:"items"`
	UpdatedAt string     `json:"updated_at"`
}
```

## Desain tool

```go
type TodoItemInput struct {
	Content    string `json:"content" jsonschema:"the task description, in imperative form, e.g. 'Run the test suite'"`
	Status     string `json:"status,omitempty" jsonschema:"one of pending, in_progress, or completed; defaults to pending when omitted"`
	ActiveForm string `json:"active_form,omitempty" jsonschema:"present-continuous phrasing shown while this item is in progress, e.g. 'Running the test suite'; defaults to content when omitted"`
}
type TodoWriteInput struct {
	Items []TodoItemInput `json:"items" jsonschema:"the complete todo list for the current task, replacing whatever list was stored before"`
}
type TodoReadInput struct{}
```

Handler `todo_write`:
1. Validasi tiap item: `content` ga boleh kosong, `status` harus salah
   satu dari `pending`/`in_progress`/`completed` (kosong → default
   `pending`).
2. Generate `id` sequential ("1", "2", ...) berdasar posisi di array.
3. `active_form` default ke `content` kalau kosong.
4. Simpan ke `assets/data/todos.json` (timestamp `updated_at`).
5. Return rendered checklist teks:
   ```
   Todo List (diupdate 2026-08-30T10:15:00Z)
   [ ] 1. Buat struct dan tool
   [~] 2. Build & sanity test   <- in_progress (pakai active_form)
   [x] 3. Tulis plan file
   ```
6. (Opsional, nice-to-have) Kalau lebih dari 1 item `in_progress`
   sekaligus, tambahin catatan peringatan di akhir output (bukan block,
   cuma reminder best-practice "idealnya cuma 1 in_progress").

Handler `todo_read`: baca file, kalau belum ada return "Belum ada todo
list aktif." — kalau ada, render sama kayak di atas.

## ⚠️ Checklist WAJIB sebelum restart pm2
```bash
cd ~/RynsWorkspace/ryns-code-mcp
grep -n 'jsonschema:"[0-9]' main.go     # harus kosong
go build -o /tmp/verifyX . && echo BUILD_OK
RYNS_WORKSPACE_ROOT=~/RynsWorkspace timeout 3 /tmp/verifyX --port 8799
# exit code HARUS 124, BUKAN panic
pm2 restart ryns-mcp
pm2 status ryns-mcp   # cek restart count ga meledak & status online
```

## Urutan implementasi
1. Cek `serverVersion` & `grep -n 'todo_write\|TodoItem' main.go` — siapa
   tau kepotong sesi sebelumnya.
2. Tambah struct `TodoItem`, `TodoState`, `TodoItemInput`,
   `TodoWriteInput`, `TodoReadInput`.
3. Tambah helper `todoFilePath()`, `loadTodoState()`, `saveTodoState()`,
   `renderTodoList()`.
4. Registrasi tool `todo_write` dan `todo_read`.
5. Build → sanity run isolated → restart pm2 → verify.
6. Test manual: `todo_write` sama beberapa item campuran status, cek
   `todo_read` balikin hal yang sama.
7. Bump `serverVersion` (misal `0.7.2`).
8. Update file ini jadi `Status: SELESAI`.

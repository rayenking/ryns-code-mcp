# PLAN: LocalSheet Tools untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.0, 2026-08-29). Semua 12 tool di tabel bawah
> sudah diimplementasi, dibuild, dites lewat MCP tools/call langsung dan
> lewat endpoint HTTP publik, dan `belanja.json` sudah dimigrasi ke
> `assets/data/sheets/belanja.json`. Detail lengkap ada di bawah untuk
> referensi kalau ada follow-up/bugfix.

## Tujuan
User mau punya "Google Sheets versi lokal" yang dikontrol lewat MCP tools,
supaya bisa minta hal-hal kayak:
- "list sheet apa aja yang ada" → `sheet_list`
- "tambahin 123456789 ke sheet X, taruh di kolom baru setelah kolom Y" →
  kombinasi `column_add` (posisi setelah kolom Y) + `cell_set`
- "hapus kolom Z dari sheet X" → `column_remove`
- "tampilin isi sheet X" → `sheet_get`

Sheet-nya juga harus bisa dibuka & diedit manual lewat browser (HP/laptop),
bukan cuma lewat saya — makanya desainnya nyambung ke HTML viewer yang udah
ada (`assets/daftar-belanja-material.html` + `assets/data/belanja.json`),
cuma digeneralisasi jadi multi-sheet.

## Yang SUDAH ada (jangan dibangun ulang)
- Server: `~/RynsWorkspace/ryns-code-mcp/main.go`, saat ini **v0.6.0**,
  jalan via `pm2` dengan nama proses `ryns-mcp`, listen di
  `127.0.0.1:8730`, di-expose publik lewat cloudflared tunnel di
  `https://your-tunnel.example.com` (semua path di-forward: `/mcp`, `/assets/*`,
  `/api/save/*` — sudah dites kerja).
- `safePath()` sudah dilonggarkan (ga di-jail ke workspace, absolute path
  bebas diakses) — ini PC didedikasikan buat AI agent.
- Static file server `/assets/` → serve dari folder `assets/` di workspace
  root, sudah publik & dites (HTTP 200).
- Endpoint `POST /api/save/<name>` sudah ada — nulis body JSON ke
  `assets/data/<name>.json`. **Ini prototype yang perlu digeneralisasi**
  buat nge-support sub-folder `sheets/<id>` (lihat langkah 1 di bawah).
- Prototype HTML editable table: `assets/daftar-belanja-material.html`
  (fetch dari `/assets/data/belanja.json`, contenteditable, tombol
  tambah/hapus baris & kolom, auto-hitung baris total, tombol simpan yang
  POST ke `/api/save/belanja`). **Ini yang jadi basis template
  `sheet.html` generik.**
- Tools lain yang udah ditambahin di luar bawaan awal: `screenshot`,
  `multi_edit_file`, `clipboard_get/set`, `mouse_click`, `keyboard_type`,
  `key_press`, `get_active_window`, `list_windows`, `notify_send`,
  `ocr_screenshot`, `camera_capture`, `record_video`.

## ⚠️ Gotcha penting (JANGAN DIULANGI)
Tag `jsonschema` di Go SDK ini **panic saat startup** kalau isinya kayak
key-value (`"1=left, 2=middle"`) — parser nganggep itu syntax tag yang
salah. Akibatnya proses crash-loop di pm2, dan karena ini server yang sama
yang jadi jalur akses Claude ke laptop, Claude ikut ke-lockout (pernah
kejadian beneran). **Selalu tulis deskripsi jsonschema dalam bentuk
prosa biasa**, hindari pola `sesuatu=nilai` di awal string.

Checklist wajib sebelum restart pm2 tiap kali abis edit:
```bash
cd ~/RynsWorkspace/ryns-code-mcp
grep -n 'jsonschema:"[0-9]' main.go     # harus kosong
go build -o /tmp/verifyX . && echo BUILD_OK
RYNS_WORKSPACE_ROOT=~/RynsWorkspace timeout 3 /tmp/verifyX --port 8799
# exit code HARUS 124 (ditutup timeout), BUKAN panic
pm2 restart ryns-mcp
pm2 status ryns-mcp   # cek restart count ga meledak & status online
```

## Data model
Simpan tiap sheet sebagai 1 file JSON di `assets/data/sheets/<id>.json`:
```json
{
  "id": "belanja",
  "title": "Daftar Belanja Material Bangunan",
  "columns": ["Item", "Jumlah/Satuan", "Harga", "Subtotal"],
  "rows": [["Geres", "2 Kijang", "270000", "540000"]],
  "note": "opsional",
  "created_at": "RFC3339",
  "updated_at": "RFC3339"
}
```
Plus index registry `assets/data/sheets/_index.json` (list semua id+title
biar `sheet_list` ga perlu buka semua file satu-satu):
```json
{"sheets": [{"id": "belanja", "title": "Daftar Belanja Material Bangunan"}]}
```

## Tools yang mau ditambahin (semua pakai pola AddTool yang sudah ada —
lihat `screenshot`/`camera_capture` sebagai contoh pola input struct +
handler + toolError/textResult)

| Tool | Input | Fungsi |
|---|---|---|
| `sheet_create` | `id, title, columns[]` | Bikin sheet baru + daftarin di index. Tolak kalau id udah ada (kecuali `overwrite=true`) |
| `sheet_list` | (kosong) | List semua sheet: id, title, jumlah baris, jumlah kolom |
| `sheet_get` | `id, start_row?, end_row?` | Tampilin isi sheet sebagai tabel teks (rapi, kayak `read_file` yang bisa di-scope biar ga kepanjangan buat sheet gede) |
| `sheet_delete` | `id` | Hapus file sheet + entry index |
| `sheet_rename` | `id, new_title` | Ganti title doang, id/data tetap |
| `column_add` | `id, name, after_column?` | Tambah kolom baru; kalau `after_column` diisi, sisipkan tepat setelah kolom itu (bukan cuma nambah di ujung) — ini yang dipakai buat request "tambahin X setelah kolom Y" |
| `column_remove` | `id, column` | Hapus kolom (by nama atau index) |
| `column_rename` | `id, old_name, new_name` | Ganti nama kolom |
| `row_add` | `id, values[], position?` | Tambah baris baru (append atau insert di posisi tertentu) |
| `row_remove` | `id, row_index` | Hapus baris tertentu |
| `cell_set` | `id, row_index, column, value` | Set 1 sel spesifik (row × kolom by nama/index) |
| `cell_get` | `id, row_index, column` | Baca 1 sel spesifik |
| `sheet_url` | `id` | Balikin URL publik buat buka sheet itu di browser (`https://your-tunnel.example.com/assets/sheet.html?id=<id>`) |

## Perubahan di HTML viewer
- Generalisasi `daftar-belanja-material.html` → `assets/sheet.html`,
  ambil `id` dari query string (`?id=belanja`), fetch dari
  `/assets/data/sheets/<id>.json`, simpan lewat
  `POST /api/save/sheets/<id>`.
- Update handler `/api/save/` di `main.go` biar terima path bertingkat
  (`sheets/<id>`) dengan tetap disanitize ketat (whitelist karakter
  `[a-zA-Z0-9_-]` buat tiap segment, tolak `..`).
- Migrasi data lama: pindahin `assets/data/belanja.json` →
  `assets/data/sheets/belanja.json`, hapus/redirect
  `daftar-belanja-material.html` ke `sheet.html?id=belanja`, update index.

## Urutan implementasi
1. Update `/api/save/` handler → support nested `sheets/<id>` path, validasi ketat.
2. Tambah helper Go: `loadSheetIndex()`, `saveSheetIndex()`, `loadSheet(id)`, `saveSheet(sheet)` — dipakai bareng oleh semua tool baru.
3. Implementasi 12 tool di tabel atas, satu-satu, cek jsonschema tag tiap selesai nambah.
4. Bikin `assets/sheet.html` (generalisasi dari file lama).
5. Migrasi `belanja.json` ke lokasi baru (`assets/data/sheets/belanja.json`), update index, hapus file lama yang duplikat.
6. Build → sanity run isolated port → restart pm2 → verify via curl (tools/list harus include semua tool baru).
7. User refresh tool list di app → tes tiap tool sekali dari chat (create, add row, add column after X, get, list, delete test-sheet).
8. Kalau semua lolos, bump `serverVersion` final (misal `0.7.0`), update file ini jadi `Status: SELESAI`.

## Kalau lanjut sesi baru, mulai dari sini:
Cek dulu versi & isi main.go saat ini (`grep serverVersion main.go`,
`grep -n 'AddTool.*sheet' main.go`) buat tau langkah mana yang udah/belum
kekerjain, karena bisa aja sebagian sempat diimplementasi sebelum limit.

## Catatan implementasi (2026-08-29, v0.7.0)
- Semua 12 tool sudah ada di `main.go` (search `sheet_create`, `sheet_list`,
  dll). Helper: `loadSheetIndex/saveSheetIndex/loadSheet/saveSheet/
  upsertSheetIndexEntry/removeSheetIndexEntry/resolveColumnIndex/
  renderSheetTable`.
- **Gotcha yang ketemu**: workspace root server itu
  `~/RynsWorkspace` (bukan folder repo `ryns-code-mcp/`), jadi
  `assets/` yang di-serve publik ada di `~/RynsWorkspace/assets`,
  terpisah dari source code. Jangan taruh `sheet.html` atau file assets
  lain di `ryns-code-mcp/assets/` — itu tidak ke-serve.
- `assets/sheet.html` sudah dibikin generik: baca `?id=` dari query string,
  fetch `/assets/data/sheets/<id>.json`, simpan lewat
  `POST /api/save/sheets/<id>`, ada tombol tambah/hapus baris & kolom, judul
  editable, indikator status simpan.
- `/api/save/` sudah support nested path (`sheets/<id>`) dengan whitelist
  regex `^[a-zA-Z0-9_-]+$` per segment, `..` otomatis ditolak (sudah dites).
- `belanja.json` lama (ternyata masih ada di
  `assets/data/belanja.json`, ketemu pas migrasi) sudah dipindah jadi
  `assets/data/sheets/belanja.json` dengan `id: "belanja"`, terdaftar di
  index. `daftar-belanja-material.html` sekarang jadi halaman redirect ke
  `sheet.html?id=belanja`.
- Sudah dites: create/get/list/column_add(after_column)/row_add/cell_set/
  delete via `tools/call` langsung ke `http://127.0.0.1:8730/mcp`, plus GET
  `sheet.html` & `POST /api/save/sheets/<id>` via curl (lokal & lewat
  tunnel publik `https://your-tunnel.example.com`). Semua HTTP 200 / hasil sesuai
  ekspektasi.
- Update dari sesi berikutnya (2026-08-30): sudah dites beneran dari chat
  (bukan cuma curl) — `sheet_get` & `row_add` dipanggil langsung buat nambah
  pengeluaran ke sheet `belanja`, hasilnya sesuai & total di `sheet.html`
  ikut update. HTML viewer juga sudah dites beneran (total row sempat ilang
  pas generalisasi ke `sheet.html`, sudah diperbaiki — lihat commit fix
  auto-total di `sheet.html`).

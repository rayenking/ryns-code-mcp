# PLAN: Lint/Format Tool (lint_format) untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.3, 2026-08-31). `lint_format` udah jalan,
> dites lewat `tools/call` langsung: mode CHECK kedeteksi file Go yang
> berantakan, mode FIX beneran ngebenerin (cross-check pake `gofmt -l`
> manual abis fix = kosong), dan pesan "tidak ada file yang didukung"
> muncul benar buat direktori tanpa file yang cocok.

## Tujuan
Satu tool `lint_format` yang bisa cek atau auto-fix formatting kode di
file/direktori, biar abis edit ga perlu jalanin formatter manual lewat
`execute_command`.

## Yang keinstall di laptop (dicek 2026-08-31)
- Go: `gofmt` ✅ (ada, `go1.26.3`)
- Rust: `rustfmt`/`cargo` ✅ (ada, di `~/.cargo/bin`)
- Node: `node`/`npm`/`npx` ✅ ada, TAPI `prettier`/`eslint` GLOBAL **tidak**
  keinstall — cuma bisa jalan kalau project yang bersangkutan punya
  `prettier` di `node_modules`-nya sendiri (dijalanin via
  `npx --no-install prettier`, biar ga otomatis download create surprise
  network call).
- Python: `python3` ✅, tapi `black`/`ruff`/`flake8` **tidak ada**. Jadi
  Python cuma bisa disupport sebatas syntax-check (`python3 -m
  py_compile`), BUKAN auto-format.
- Shell (`shellcheck`/`shfmt`): tidak ada, skip dulu.

## Desain tool

```go
type LintFormatInput struct {
	Path string `json:"path" jsonschema:"file or directory to check or format; relative paths resolve under workspace"`
	Fix  bool   `json:"fix,omitempty" jsonschema:"when true, apply formatting fixes in place; when false (the default) only report what needs fixing without changing any file"`
}
```

Alur:
1. `safePath(path)`, terus walk (kalau direktori) buat ngumpulin file,
   di-skip: `.git`, `node_modules`, `vendor`, `target`, `dist`, `build`,
   `.next`, `__pycache__`, dan direktori dot lain (kecuali root-nya
   sendiri).
2. Klasifikasi per ekstensi ke 4 grup: `go` (.go), `rust` (.rs), `js`
   (.js/.jsx/.ts/.tsx/.json/.css/.scss/.html/.md/.yaml/.yml), `py` (.py).
3. Kalau ga ada file yang match grup manapun → bilang "ga ada file yang
   didukung".
4. Per grup, jalanin (semua **check dulu**, baru **apply** kalau
   `fix=true`, biar bisa laporan "file mana yang diformat ulang" akurat):
   - **Go**: `gofmt -l <files>` buat cek (list nonkosong = butuh format).
     Kalau fix: `gofmt -w <files>` abis itu.
   - **Rust**: `rustfmt --check <files>` buat cek. Kalau fix: `rustfmt
     <files>` (tanpa `--check`) abis itu.
   - **JS/TS/dst**: `npx --no-install prettier --check <files>`. Kalau
     output/error nunjukin prettier ga ketemu (`could not determine
     executable to run` dsb) → kasih pesan jelas "install dulu: npm
     install --save-dev prettier", JANGAN coba auto-install. Kalau fix:
     `npx --no-install prettier --write <files>`.
   - **Python**: `python3 -m py_compile <file>` per file, cuma report
     error sintaks, TIDAK ada auto-format (sesuai keterbatasan tool yang
     ada), kasih catatan di output.
5. Gabungin semua section jadi satu laporan teks, per grup dikasih header
   `== Go (gofmt) ==` dst.
6. Timeout 30 detik per command (`context.WithTimeout`).

## ⚠️ Checklist WAJIB sebelum restart pm2
```bash
cd ~/RynsWorkspace/ryns-code-mcp
grep -n 'jsonschema:"[0-9]' main.go     # harus kosong
go build -o /tmp/verifyX . && echo BUILD_OK
RYNS_WORKSPACE_ROOT=~/RynsWorkspace timeout 3 /tmp/verifyX --port 8799
# exit code HARUS 124, BUKAN panic
pm2 restart ryns-mcp
pm2 status ryns-mcp
```

## Urutan implementasi
1. Cek dulu `grep -n 'lint_format\|LintFormatInput' main.go` — siapa tau
   udah sempet dikerjain sesi lain.
2. Tambah `LintFormatInput`, helper `classifyLintFile`, `collectLintFiles`
   (pakai `filepath.WalkDir`, perlu import `io/fs`), `runLintTool`.
3. Registrasi tool `lint_format` dengan 4 section (go/rust/js/py) sesuai
   desain di atas.
4. Build → sanity run isolated → restart pm2 → verify.
5. Tes manual: bikin file `.go` yang formatnya berantakan, jalanin
   `lint_format` mode check (harus kedetek), terus mode fix (harus
   kebenerin), cross-check pake `gofmt -l` manual.
6. Bump `serverVersion` (misal `0.7.3`).
7. Update file ini jadi `Status: SELESAI`.

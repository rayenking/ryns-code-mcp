# PLAN: System Stats Tool (system_stats) untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.4, 2026-09-01). `system_stats` udah jalan,
> hasilnya cocok sama pengecekan manual (CPU/uptime/load/RAM/swap/disk/
> baterai/suhu). Sempet ada bug: helper `runCommand` yang ada di
> codebase ternyata ngebungkus output pake format `exit_code:`/`stdout:`
> (didesain buat tool `execute_command`, bukan buat diparsing), jadi
> parsing awal berantakan — dibikinin helper baru `runRawCommand` yang
> balikin stdout mentah aja buat kasus kayak gini.

## Tujuan
Satu tool `system_stats` (no input) yang ngerangkum kondisi laptop:
CPU/load, RAM, swap, disk, baterai, suhu (kalau ada) — biar ga perlu
`execute_command` manual tiap mau ngecek "laptop gimana kondisinya".

## Sumber data (dicek manual 2026-09-01, semua ada di laptop ini)
- CPU core: `nproc`
- Uptime + load average: `uptime -p` (human readable) + `cat /proc/loadavg`
  (3 angka pertama) — lebih robust daripada regex-parsing output `uptime`
  penuh yang formatnya suka beda-beda.
- RAM & swap: `free -h`, ambil baris `Mem:` (kolom total/used/available)
  dan `Swap:` (total/used).
- Disk root: `df -h /`, ambil baris terakhir (kolom size/used/avail/use%).
- Baterai: `upower -i $(upower -e | grep -i BAT) 2>/dev/null` — parse
  `state`, `percentage`, `capacity` (ini yang nunjukin kesehatan baterai
  dibanding kapasitas desain awal), `model`, `vendor`. Kalau kosong
  (device tanpa baterai / upower ga ada) → tampilin "tidak terdeteksi",
  JANGAN dianggap error.
- Suhu: `sensors 2>/dev/null`, ambil semua baris yang ada `°C` di
  dalamnya (fleksibel, ga expect chip tertentu karena beda laptop beda
  chip yang kedeteksi). Kalau `sensors` ga ada / kosong, skip section ini
  aja, jangan report error.

## Desain tool
```go
type SystemStatsInput struct{}
```
Semua command dijalanin lewat `runCommand(ctx, cmd, workspace, 5*time.Second)`
yang udah ada (bash -lc, ada timeout). Tiap section independen — kalau
satu command gagal/ga ada, section itu di-skip/dikasih catatan "tidak
tersedia", TIDAK bikin seluruh tool gagal (toolError). Ini penting karena
target laptop lain mungkin ga punya baterai/sensors.

Urutan output:
```
CPU: <n> core
Uptime: <human readable>
Load average (1/5/15 menit): <a> / <b> / <c>

RAM: total <x>, terpakai <y>, tersedia <z>
Swap: total <x>, terpakai <y>

Disk (/): total <x>, terpakai <y> (<pct>), tersisa <z>

Baterai: <vendor> <model>, <pct>%, status <state>, kesehatan <capacity>% dari kapasitas awal
   (atau: "Baterai: tidak terdeteksi (mungkin desktop / tanpa baterai)")

Suhu:
  <baris1>
  <baris2>
   (section ini di-skip total kalau ga ada data suhu)
```

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
1. Cek dulu `grep -n 'system_stats\|SystemStatsInput' main.go`.
2. Tambah struct `SystemStatsInput` (kosong).
3. Registrasi tool `system_stats`, tiap section pakai `runCommand` +
   parsing ringan (`strings.Fields`, bukan regex ribet), semua error
   per-section di-swallow jadi fallback text, bukan `toolError`.
4. Build → sanity run isolated → restart pm2 → verify.
5. Tes manual, cocokin sama command manual (`free -h`, `df -h /`,
   `upower -i ...`).
6. Bump `serverVersion` (misal `0.7.4`).
7. Update file ini jadi `Status: SELESAI`.

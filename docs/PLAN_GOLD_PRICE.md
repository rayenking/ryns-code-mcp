# PLAN: Gold Price Tool (check_gold_price) untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.1, 2026-08-30). Tool `check_gold_price` udah
> jalan, dites langsung ke server via `tools/call`, hasil cocok sama data
> mentah API (`weight=1` → sellPrice 2.670.000, 22K estimasi 2.447.500).
> User tinggal refresh tool list di app buat mulai pake dari chat.

## Tujuan
Tambah 1 tool MCP baru: **`check_gold_price`** — ngecek harga emas hari ini
(Logam Mulia Antam 24K resmi + hitungan 22K), biar user bisa tanya
"harga emas hari ini berapa" dan langsung dapet jawaban tanpa saya harus
web_search manual tiap kali.

## Sumber data (sudah divalidasi & ditest, tinggal pakai)
- **Base URL API**: `https://logam-mulia-api.iamutaki.workers.dev`
  (open-source, MIT license, gratis, TANPA API key, di-host Cloudflare
  Workers — GitHub: `iamutaki/logam-mulia-api`)
- **Endpoint yang dipakai**: `GET /api/prices/logammulia`
  (data asli hasil scrape dari `logammulia.com`, situs resmi Antam,
  update harian)
- **Contoh response** (sudah ditest via curl, real):
  ```json
  {
    "success": true,
    "data": [
      {
        "source": "logammulia",
        "material": "gold",
        "materialType": "Emas Batangan",
        "weight": 0.5,
        "weightUnit": "gr",
        "sellPrice": 1385000,
        "buybackPrice": null,
        "currency": "IDR",
        "recordedDate": "2026-08-30",
        "displayName": "Logam Mulia",
        "urlHomepage": "https://www.logammulia.com"
      },
      { "weight": 1, "sellPrice": 2670000, ... },
      { "weight": 2, ... }
      // dst untuk berbagai pecahan berat (0.5gr, 1gr, 2gr, 5gr, 10gr, ...)
    ]
  }
  ```
- **Ambil harga per gram**: cari item di `data[]` dengan `weight == 1` →
  itu `sellPrice`-nya adalah harga 24K per gram hari ini (misal
  Rp2.670.000 per 30 Agu 2026).
- Endpoint force-refresh (skip cache harian) kalau perlu data paling
  fresh: `?refresh=true` di query string.

## Kenapa 22K dihitung, bukan fetch langsung
Antam Logam Mulia cuma jual produk **24K murni (kadar 99,99%)** — ga ada
varian resmi 22K dari Antam. 22K itu konteksnya emas perhiasan. Cara
umum yang dipakai situs kalkulator emas (jualemas.id dkk) buat estimasi
22K: **rasio kadar dari harga 24K**.

```
harga_22K_per_gram = harga_24K_per_gram × (22 / 24)
```//
Ini estimasi ya, bukan harga resmi — perhiasan beneran biasanya nambah
ongkos pembuatan & margin toko, harga_22K ini cuma acuan kadar logam
murni.

## Desain tool

```go
type GoldPriceInput struct {
	// (kosong — ga butuh parameter, selalu ambil harga hari ini)
}
```

Handler flow:
1. `http.Get("https://logam-mulia-api.iamutaki.workers.dev/api/prices/logammulia")`
   dengan timeout wajar (misal 10 detik context timeout, ikuti pola
   `runXTool`/tool lain yang udah ada di codebase — pakai
   `http.Client{Timeout: ...}` biasa karena ini bukan command CLI, tapi
   HTTP request langsung dari Go, jadi ga lewat `exec.Command`).
2. Parse JSON response ke struct Go yang cocok (`Success bool`,
   `Data []struct{ Weight float64; SellPrice int64; RecordedDate string }`).
3. Cari entry dengan `Weight == 1`.
4. Kalau ga ketemu atau `Success == false` → return error yang jelas
   (`gold_price_error: ...`), jangan silent fail.
5. Hitung 22K = `sellPrice24K * 22 / 24` (pembulatan ke rupiah terdekat).
6. Format output teks yang rapi, contoh:
   ```
   Harga Emas Hari Ini (30 Agustus 2026)
   Sumber: Logam Mulia (Antam resmi) via logammulia.com

   24K (Logam Mulia, kadar 99,99%): Rp 2.670.000 / gram
   22K (estimasi kadar 91,7%):       Rp 2.447.500 / gram

   Catatan: harga 22K adalah estimasi rasio kadar dari harga 24K resmi,
   bukan harga jual resmi Antam (Antam hanya menjual produk 24K).
   ```
7. Registrasi tool pakai pola yang sama kayak tool lain
   (`mcp.AddTool(server, &mcp.Tool{...}, func(...) {...})`), naruhnya di
   deket tool-tool lain yang sifatnya "fetch info" (atau di section
   baru).

## ⚠️ Checklist WAJIB sebelum restart pm2 (gotcha yang udah pernah kejadian)
Tag `jsonschema` di Go SDK ini **panic saat startup** kalau isinya kayak
key-value (`"1=left, 2=middle"`) — jangan pernah tulis jsonschema tag
dengan pola `sesuatu=nilai` di awal string, selalu prosa biasa. Karena
tool ini ga butuh parameter (kosong), risikonya rendah, tapi tetep cek:

```bash
cd ~/RynsWorkspace/ryns-code-mcp
grep -n 'jsonschema:"[0-9]' main.go     # harus kosong
go build -o /tmp/verifyX . && echo BUILD_OK
RYNS_WORKSPACE_ROOT=~/RynsWorkspace timeout 3 /tmp/verifyX --port 8799
# exit code HARUS 124 (ditutup timeout), BUKAN panic
pm2 restart ryns-mcp
pm2 status ryns-mcp   # cek restart count ga meledak & status online
```

## Konteks server saat ini (biar ga kaget)
- Server: `~/RynsWorkspace/ryns-code-mcp/main.go`, versi terakhir
  **v0.6.0** (base sebelum tool ini), jalan via `pm2` nama proses
  `ryns-mcp`, listen `127.0.0.1:8730`, expose publik via cloudflared
  tunnel di `https://your-tunnel.example.com` (semua path ke-forward:
  `/mcp`, `/assets/*`, `/api/save/*`).
- `safePath()` sudah dilonggarkan (PC ini didedikasikan buat AI agent,
  bebas akses absolute path).
- Tools yang udah ada (jangan dibangun ulang): `screenshot`,
  `multi_edit_file`, `clipboard_get/set`, `mouse_click`,
  `keyboard_type`, `key_press`, `get_active_window`, `list_windows`,
  `notify_send`, `ocr_screenshot`, `camera_capture`, `record_video`,
  plus tools dasar (`read_file`, `write_file`, `edit_file`,
  `execute_command`, `grep`, `search_files`, git tools, dll).
- Ada juga rencana besar lain yang lagi jalan paralel:
  `PLAN_LOCALSHEET.md` (di folder yang sama) — sheet/table tools.
  **Jangan campur kerjaan dua plan ini**, kerjain terpisah biar ga
  bentrok pas edit `main.go` (conflict kalau dua agent edit file yang
  sama bersamaan).

## Urutan implementasi
1. Cek dulu `serverVersion` & `grep -n 'check_gold_price' main.go` —
   siapa tau udah sempat dikerjain sebagian oleh sesi lain (kayak yang
   kejadian di sheet tools kemarin, keimplementasi duluan sebelum
   sempat dicek).
2. Tambah struct `GoldPriceInput` (kosong) + struct buat parse response
   API.
3. Tambah 1 tool `check_gold_price` sesuai desain di atas.
4. Build → sanity run isolated port → restart pm2 → verify.
5. Test manual: panggil tool-nya, pastiin hasil masuk akal (bandingin
   manual ke `curl https://logam-mulia-api.iamutaki.workers.dev/api/prices/logammulia`).
6. Bump `serverVersion` (misal jadi `0.6.1` kalau localsheet plan belum
   selesai duluan, atau versi berikutnya yang available).
7. User refresh tool list di app → tes dari chat.
8. Update file ini jadi `Status: SELESAI` kalau semua lolos.

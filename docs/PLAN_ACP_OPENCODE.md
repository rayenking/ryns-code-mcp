# PLAN: ACP Sub-Agent Tool — acp_opencode untuk ryns-code-mcp

> Status: **SELESAI** (v0.7.5, 2026-09-03). `acp_opencode` udah jalan dan
> dites end-to-end 3x: (1) prompt tanpa tool call → balik "OK" bersih,
> `tool_calls: 0`; (2) prompt suruh bikin file → file BENERAN kebuat di
> disk dengan isi yang benar, permission auto-approve jalan mulus tanpa
> nge-hang, `tool_calls: 2`; (3) setelah fix noise-stripping, output bersih
> tanpa tag internal. Ketemu 1 masalah kecil di jalan: OpenCode nyelipin
> tag housekeeping internal (`<dcp-system-reminder>`, `<dcp-message-id>`)
> ke `agent_message_chunk`, sudah difilter lewat `stripACPNoiseTags()`
> sebelum di-report — bukan sesuatu yang didokumentasikan resmi di spec
> ACP, murni observasi dari testing nyata.

## Tujuan
Tool MCP baru `acp_opencode` yang spawn OpenCode sebagai **sub-agent**
lewat Agent Client Protocol (ACP), kirim satu prompt, tunggu sampai
selesai, balikin hasilnya sebagai teks — biar Claude (lewat server ini)
bisa mendelegasikan task coding ke OpenCode dan dapet hasilnya balik.

## Kondisi laptop (dicek 2026-09-02)
- `opencode` binary ada di `~/.opencode/bin/opencode`, versi
  1.18.26, native speak ACP lewat `opencode acp`.
- `claude` CLI juga ada (v2.1.243) tapi ditunda dulu sesuai keputusan di
  atas.

## Riset protokol ACP (dari agentclientprotocol.com/protocol/schema, akurat per 2026-09-02)
JSON-RPC 2.0, **newline-delimited** (satu JSON per baris) lewat stdin/stdout.
Client (kita) spawn agent sebagai subprocess.

Alur satu-shot (tanpa reuse session antar tool call, demi kesederhanaan v1):
1. `initialize` → `{protocolVersion: 1, clientCapabilities: {fs:{readTextFile:false,writeTextFile:false}, terminal:false}, clientInfo:{name,version}}`
   - Sengaja declare `fs`/`terminal` capability **false** — biar OpenCode
     fallback pakai akses filesystem & proses miliknya sendiri langsung
     (dia kan CLI agent asli, bukan cuma "otak" doang), jadi kita GA PERLU
     implementasi handler `fs/read_text_file`, `fs/write_text_file`,
     `terminal/*` sama sekali. Ini penyederhanaan besar dan valid karena
     agent CLI headless secara umum fallback ke tool built-in mereka kalau
     client bilang ga support.
2. `session/new` → `{cwd: <absolute path>, mcpServers: []}` → dapet `sessionId`.
3. `session/prompt` → `{sessionId, prompt: [{type:"text", text: <prompt>}]}`
   - Ini request yang BLOCKING sampai turn selesai. Selama nunggu response
     ini, agent bisa ngirim:
     - **notification** `session/update` (progress: `agent_message_chunk`
       berisi teks jawaban yang di-stream, `tool_call`/`tool_call_update`
       buat tool calls) → kita **kolektif accumulate** teks dari
       `agent_message_chunk`, dan itung jumlah `tool_call`.
     - **request** `session/request_permission` (WAJIB dijawab, ga ada
       fallback otomatis dari sisi agent) → kita **auto-approve**: pilih
       option dengan `kind: "allow_once"`, kalau ga ada baru
       `"allow_always"`, kalau ga ada opsi allow sama sekali → balas
       `{outcome:{outcome:"cancelled"}}`. Ini masuk akal karena tool ini
       memang didesain buat delegasi otonom di laptop pribadi user sendiri.
   - Respons akhirnya `{stopReason: "end_turn"|"max_tokens"|...}`.
4. Bunuh proses (`stdin.Close()` lalu `Process.Kill()` + `Wait()`) — ga
   ada method "shutdown" resmi di spec, jadi sesi ini memang one-shot per
   tool call (approach paling simpel & aman buat v1).

### Wire format kunci yang perlu presisi
- Response ke request kita: `{"jsonrpc":"2.0","id":N,"result":{...}}` atau
  `{"jsonrpc":"2.0","id":N,"error":{"code":...,"message":"..."}}`.
- Notification dari agent: `{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"...","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"..."}}}}`
  — discriminator field-nya **`sessionUpdate`** (bukan `type`) di dalam
  object `update`.
- Request dari agent `session/request_permission`:
  ```json
  {"jsonrpc":"2.0","id":5,"method":"session/request_permission",
   "params":{"sessionId":"...","toolCall":{...},"options":[
     {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
     {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
   ]}}
  ```
  Kita balas: `{"jsonrpc":"2.0","id":5,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}`
  atau `{"jsonrpc":"2.0","id":5,"result":{"outcome":{"outcome":"cancelled"}}}`.

## Desain kode Go

### Tipe generik ACP client (dipakai lagi kalau nanti nambah acp_claudecode)
```go
type acpPendingResult struct {
	Result json.RawMessage
	Err    error
}

type acpClient struct {
	stdin   io.WriteCloser
	reader  *bufio.Reader
	writeMu sync.Mutex

	mu      sync.Mutex
	nextID  int64
	pending map[int64]chan acpPendingResult

	textMu    sync.Mutex
	textBuf   strings.Builder
	toolCalls int
}
```
Method: `newACPClient`, `readLoop` (baca baris demi baris, dispatch ke
response/request/notification), `call(ctx, method, params)`,
`writeMessage`, `handleAgentRequest` (khusus `session/request_permission`,
method lain dijawab error `-32601` biar ga nge-hang), `handleNotification`
(khusus `session/update`, ekstrak `agent_message_chunk` & itung
`tool_call`), `collectedText()`, `toolCallCount()`.

Pakai `bufio.NewReaderSize(stdout, 1<<20)` (buffer 1MB) karena baris JSON
ACP bisa panjang (isinya bisa termasuk diff/file content).

### Tool input
```go
type AcpOpenCodeInput struct {
	Prompt         string `json:"prompt" jsonschema:"the task or question to hand off to the OpenCode sub-agent"`
	Path           string `json:"path,omitempty" jsonschema:"working directory for the sub-agent, relative paths resolve under workspace; defaults to the workspace root"`
	TimeoutSeconds int    `json:"timeout_seconds,omitempty" jsonschema:"maximum seconds to wait for the sub-agent to finish; defaults to 300, capped at 1800"`
}
```

### Handler
1. Validasi prompt ga kosong.
2. `safePath(path atau ".")`.
3. Timeout: default 300s, clamp max 1800s (30 menit) — task coding bisa
   makan waktu.
4. Spawn `opencode acp` dengan `cmd.Dir = cwd`, `cmd.Env = os.Environ()`
   (supaya semua env auth OpenCode/model API key ke-inherit).
5. Jalanin alur ACP di atas.
6. Return teks: hasil `agent_message_chunk` yang di-collect + baris akhir
   `[stop_reason: X, tool_calls: N]`.
7. Kalau ada tahap yang gagal (initialize/session/new/session/prompt
   error, atau proses ga ketemu / gagal start), `toolError` dengan pesan
   jelas + sertakan stderr proses kalau ada (`opencode` mungkin nulis log
   ke stderr).

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
1. Cek dulu `grep -n 'acp_opencode\|acpClient' main.go` — siapa tau kepotong sesi sebelumnya.
2. Tambah import `bufio` (dan pastikan `sync`, `sync/atomic` ada — cek dulu, `sync` based mutex udah ada, atomic belum tentu).
3. Tambah tipe `acpClient` + semua method di atas.
4. Tambah `AcpOpenCodeInput` + handler tool `acp_opencode`.
5. Build → sanity run isolated → restart pm2 → verify.
6. Tes manual dengan prompt AMAN dulu (misal "cuma balas kata OK, jangan ubah file apapun") di direktori scratch (`/tmp/acp_test`), biar ga ada risiko ke workspace asli kalau ada bug di auto-approve permission.
7. Bump `serverVersion`.
8. Update file ini jadi `Status: SELESAI`.

## Catatan buat lanjutan (acp_claudecode, DITUNDA)
- `claude` CLI di laptop ini udah authenticated (ada `ANTHROPIC_AUTH_TOKEN`
  + `ANTHROPIC_BASE_URL` di env, BUKAN `ANTHROPIC_API_KEY` — kemungkinan
  pakai subscription/proxy custom, bukan billing API biasa).
- Ada 2 kandidat adapter:
  - `@agentclientprotocol/claude-agent-acp` (dulu `@zed-industries/claude-code-acp`) — pakai Claude Agent SDK, **butuh `ANTHROPIC_API_KEY`** (bakal override auth yang udah ada → billing API terpisah, BUKAN yang diinginkan).
  - `claude-code-acp` (npm, unscoped, versi 0.1.1, "Claude Code agent for Zed Editor via ACP") — perlu dicek lagi lebih detail apakah dia PTY-wrap `claude` CLI asli (respect auth yang ada) atau juga butuh API key sendiri. **Belum dikonfirmasi**, jangan asumsikan sebelum baca source/README-nya langsung.
- Jangan lanjut ke acp_claudecode sebelum ricek ulang detail auth adapter
  ini, biar ga tau-tau kepake billing API padahal ada subscription aktif.

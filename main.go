package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

const (
	serverName    = "ryns-code-mcp"
	serverVersion = "0.7.5"
	maxOutput     = 100000
	maxReadLines  = 2000
	maxLineLength = 2000
	maxRecordSecs = 120
)

// imageExt maps file extensions read_file treats as images to their MIME
// type, so read_file can return them inline instead of as garbled text.
var imageExt = map[string]string{
	".png":  "image/png",
	".jpg":  "image/jpeg",
	".jpeg": "image/jpeg",
	".gif":  "image/gif",
	".webp": "image/webp",
	".bmp":  "image/bmp",
}

var toolOutputSchema = json.RawMessage(`{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"text":{"type":"string","description":"Human-readable tool result"}},"required":["text"],"additionalProperties":false}`)

type ToolOutput struct {
	Text string `json:"text" jsonschema:"human-readable tool result"`
}

var workspace = func() string {
	if v := os.Getenv("RYNS_WORKSPACE_ROOT"); v != "" {
		return filepath.Clean(v)
	}
	if home, err := os.UserHomeDir(); err == nil {
		return filepath.Join(home, "RynsWorkspace")
	}
	return "."
}()

// publicBaseURL is the externally reachable base URL (for example your
// tunnel hostname) used when tools hand back links to files under assets/.
// Set RYNS_PUBLIC_BASE_URL; the default only works on the local machine.
var publicBaseURL = func() string {
	if v := os.Getenv("RYNS_PUBLIC_BASE_URL"); v != "" {
		return strings.TrimRight(v, "/")
	}
	return "http://127.0.0.1:8799"
}()

func clip(s string) string {
	if len(s) <= maxOutput {
		return s
	}
	return s[:maxOutput] + "\n...[output truncated]"
}

func textResult(s string) (*mcp.CallToolResult, any, error) {
	text := clip(s)
	return &mcp.CallToolResult{Content: []mcp.Content{&mcp.TextContent{Text: text}}}, ToolOutput{Text: text}, nil
}

func toolError(prefix string, err error) (*mcp.CallToolResult, any, error) {
	return textResult(fmt.Sprintf("%s: %v", prefix, err))
}

// numberLines renders lines[start-1:end] as "cat -n" style output, one line
// per source line, truncating any single line that's absurdly long.
func numberLines(lines []string, start, end int) string {
	var sb strings.Builder
	for i := start - 1; i < end; i++ {
		line := lines[i]
		if len(line) > maxLineLength {
			line = line[:maxLineLength] + fmt.Sprintf("...[line truncated, %d chars total]", len(lines[i]))
		}
		fmt.Fprintf(&sb, "%6d\t%s\n", i+1, line)
	}
	return sb.String()
}

// lineOfOffset returns the 1-based line number containing byte offset idx.
func lineOfOffset(s string, idx int) int {
	if idx < 0 {
		return 1
	}
	if idx > len(s) {
		idx = len(s)
	}
	return strings.Count(s[:idx], "\n") + 1
}

// contextAroundLine returns a small numbered snippet centered on lineNum, so
// a caller can see an edit's effect without re-reading the whole file.
func contextAroundLine(content string, lineNum, context int) string {
	lines := strings.Split(content, "\n")
	start := lineNum - context
	if start < 1 {
		start = 1
	}
	end := lineNum + context
	if end > len(lines) {
		end = len(lines)
	}
	if start > len(lines) {
		return ""
	}
	return numberLines(lines, start, end)
}

// xEnv returns the environment for commands that talk to the X display
// (screenshot, clipboard, mouse/keyboard, window queries) — this box runs
// its desktop on :0, so we point every such command there explicitly since
// tool invocations don't otherwise inherit the user's desktop session env.
func xEnv() []string {
	env := append(os.Environ(), "DISPLAY=:0")
	home := os.Getenv("HOME")
	if _, err := os.Stat(filepath.Join(home, ".Xauthority")); err == nil {
		env = append(env, "XAUTHORITY="+filepath.Join(home, ".Xauthority"))
	}
	return env
}

// runXTool runs a desktop-automation command (xdotool, xclip, notify-send,
// import, tesseract, ...) with xEnv() applied and a bounded timeout.
func runXTool(ctx context.Context, timeout time.Duration, name string, args ...string) ([]byte, error) {
	cctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(cctx, name, args...)
	cmd.Env = xEnv()
	return cmd.CombinedOutput()
}

// safePath resolves a path for file/exec tools. This machine is a dedicated
// AI-agent box, so unlike a typical sandboxed MCP server, paths are NOT
// jailed to the workspace root: relative paths resolve against workspace
// (for convenience), but absolute paths (/etc, /home/other-user, /, etc.)
// are allowed through untouched. There is intentionally no directory-escape
// check anymore.
func safePath(input string) (string, error) {
	if input == "" {
		return filepath.Abs(workspace)
	}
	p := input
	if !filepath.IsAbs(p) {
		base, err := filepath.Abs(workspace)
		if err != nil {
			return "", err
		}
		p = filepath.Join(base, p)
	}
	return filepath.Abs(p)
}

// ---- LocalSheet: a tiny "Google Sheets"-alike stored as JSON under
// assets/data/sheets/, editable both via MCP tools and via the generic
// assets/sheet.html viewer served over the public tunnel. See
// PLAN_LOCALSHEET.md for the design.

type Sheet struct {
	ID        string     `json:"id"`
	Title     string     `json:"title"`
	Columns   []string   `json:"columns"`
	Rows      [][]string `json:"rows"`
	Note      string     `json:"note,omitempty"`
	CreatedAt string     `json:"created_at"`
	UpdatedAt string     `json:"updated_at"`
}

type SheetIndexEntry struct {
	ID    string `json:"id"`
	Title string `json:"title"`
}

type SheetIndex struct {
	Sheets []SheetIndexEntry `json:"sheets"`
}

var sheetIDPattern = regexp.MustCompile(`^[a-zA-Z0-9_-]+$`)

// pathSegmentPattern whitelists a single path segment accepted by the
// /api/save/ HTTP endpoint (e.g. "sheets" or a sheet id), rejecting ".."
// and anything else that could escape the assets/data directory.
var pathSegmentPattern = regexp.MustCompile(`^[a-zA-Z0-9_-]+$`)

func validSheetID(id string) error {
	if id == "" || !sheetIDPattern.MatchString(id) {
		return fmt.Errorf("id sheet harus non-kosong dan hanya boleh berisi huruf, angka, underscore, atau dash")
	}
	return nil
}

func sheetsDataDir() string {
	return filepath.Join(workspace, "assets", "data", "sheets")
}

func sheetIndexPath() string {
	return filepath.Join(sheetsDataDir(), "_index.json")
}

func sheetFilePath(id string) string {
	return filepath.Join(sheetsDataDir(), id+".json")
}

func loadSheetIndex() (SheetIndex, error) {
	data, err := os.ReadFile(sheetIndexPath())
	if errors.Is(err, os.ErrNotExist) {
		return SheetIndex{Sheets: []SheetIndexEntry{}}, nil
	}
	if err != nil {
		return SheetIndex{}, err
	}
	var idx SheetIndex
	if err := json.Unmarshal(data, &idx); err != nil {
		return SheetIndex{}, err
	}
	if idx.Sheets == nil {
		idx.Sheets = []SheetIndexEntry{}
	}
	return idx, nil
}

func saveSheetIndex(idx SheetIndex) error {
	if err := os.MkdirAll(sheetsDataDir(), 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(idx, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(sheetIndexPath(), data, 0644)
}

func upsertSheetIndexEntry(id, title string) error {
	idx, err := loadSheetIndex()
	if err != nil {
		return err
	}
	found := false
	for i, e := range idx.Sheets {
		if e.ID == id {
			idx.Sheets[i].Title = title
			found = true
			break
		}
	}
	if !found {
		idx.Sheets = append(idx.Sheets, SheetIndexEntry{ID: id, Title: title})
	}
	return saveSheetIndex(idx)
}

func removeSheetIndexEntry(id string) error {
	idx, err := loadSheetIndex()
	if err != nil {
		return err
	}
	out := idx.Sheets[:0]
	for _, e := range idx.Sheets {
		if e.ID != id {
			out = append(out, e)
		}
	}
	idx.Sheets = out
	return saveSheetIndex(idx)
}

func loadSheet(id string) (Sheet, error) {
	var s Sheet
	data, err := os.ReadFile(sheetFilePath(id))
	if err != nil {
		return s, err
	}
	if err := json.Unmarshal(data, &s); err != nil {
		return s, err
	}
	return s, nil
}

func saveSheet(s Sheet) error {
	if err := os.MkdirAll(sheetsDataDir(), 0755); err != nil {
		return err
	}
	s.UpdatedAt = time.Now().UTC().Format(time.RFC3339)
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(sheetFilePath(s.ID), data, 0644)
}

// resolveColumnIndex accepts either a column name or a stringified zero
// based index and returns the resolved zero based index.
func resolveColumnIndex(columns []string, ref string) (int, error) {
	if n, err := strconv.Atoi(ref); err == nil {
		if n < 0 || n >= len(columns) {
			return -1, fmt.Errorf("index kolom %d di luar jangkauan (0..%d)", n, len(columns)-1)
		}
		return n, nil
	}
	for i, c := range columns {
		if c == ref {
			return i, nil
		}
	}
	return -1, fmt.Errorf("kolom %q tidak ditemukan", ref)
}

// renderSheetTable formats a sheet as a readable text table, optionally
// scoped to [startRow, endRow) like read_file's start_line/end_line.
func renderSheetTable(s Sheet, startRow, endRow int) string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "Sheet: %s (%s)\n", s.ID, s.Title)
	if s.Note != "" {
		fmt.Fprintf(&sb, "Note: %s\n", s.Note)
	}
	fmt.Fprintf(&sb, "Columns: %s\n\n", strings.Join(s.Columns, " | "))
	if len(s.Rows) == 0 {
		sb.WriteString("(belum ada baris)\n")
		return sb.String()
	}
	if startRow < 0 {
		startRow = 0
	}
	if endRow <= 0 || endRow > len(s.Rows) {
		endRow = len(s.Rows)
	}
	if startRow >= endRow {
		sb.WriteString("(range baris kosong)\n")
		return sb.String()
	}
	for i := startRow; i < endRow; i++ {
		fmt.Fprintf(&sb, "[%d] %s\n", i, strings.Join(s.Rows[i], " | "))
	}
	return sb.String()
}

// runRawCommand runs a shell command and returns its raw trimmed stdout,
// for callers that need to parse the output programmatically (unlike
// runCommand, which formats a human-readable "exit_code/stdout/stderr"
// report meant for the execute_command tool).
func runRawCommand(ctx context.Context, command string, timeout time.Duration) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, "/bin/bash", "-lc", command)
	cmd.Dir = workspace
	cmd.Env = os.Environ()
	out, err := cmd.Output()
	return strings.TrimSpace(string(out)), err
}

// ---- ACP (Agent Client Protocol) client: lets tools like acp_opencode
// spawn a coding sub-agent as a subprocess and drive it over newline
// delimited JSON-RPC 2.0. See PLAN_ACP_OPENCODE.md for wire format notes.

type acpPendingResult struct {
	Result json.RawMessage
	Err    error
}

type acpClient struct {
	stdin   io.WriteCloser
	reader  *bufio.Reader
	writeMu sync.Mutex

	nextID int64

	mu      sync.Mutex
	pending map[int64]chan acpPendingResult

	textMu    sync.Mutex
	textBuf   strings.Builder
	toolCalls int
}

func newACPClient(stdin io.WriteCloser, stdout io.Reader) *acpClient {
	return &acpClient{
		stdin:   stdin,
		reader:  bufio.NewReaderSize(stdout, 1<<20),
		pending: make(map[int64]chan acpPendingResult),
	}
}

func (c *acpClient) writeMessage(v any) error {
	data, err := json.Marshal(v)
	if err != nil {
		return err
	}
	data = append(data, '\n')
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	_, err = c.stdin.Write(data)
	return err
}

func (c *acpClient) call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	id := atomic.AddInt64(&c.nextID, 1)
	ch := make(chan acpPendingResult, 1)
	c.mu.Lock()
	c.pending[id] = ch
	c.mu.Unlock()
	defer func() {
		c.mu.Lock()
		delete(c.pending, id)
		c.mu.Unlock()
	}()

	if err := c.writeMessage(map[string]any{
		"jsonrpc": "2.0", "id": id, "method": method, "params": params,
	}); err != nil {
		return nil, err
	}

	select {
	case res := <-ch:
		if res.Err != nil {
			return nil, res.Err
		}
		return res.Result, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// readLoop consumes newline-delimited JSON-RPC messages until the stream
// closes. Must run in its own goroutine for the lifetime of the process.
func (c *acpClient) readLoop() {
	for {
		line, err := c.reader.ReadString('\n')
		trimmed := strings.TrimSpace(line)
		if trimmed != "" {
			c.handleLine(trimmed)
		}
		if err != nil {
			c.mu.Lock()
			for id, ch := range c.pending {
				ch <- acpPendingResult{Err: fmt.Errorf("koneksi ACP tertutup: %w", err)}
				delete(c.pending, id)
			}
			c.mu.Unlock()
			return
		}
	}
}

func (c *acpClient) handleLine(line string) {
	var raw struct {
		ID     *int64          `json:"id"`
		Method string          `json:"method"`
		Params json.RawMessage `json:"params"`
		Result json.RawMessage `json:"result"`
		Error  *struct {
			Code    int    `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal([]byte(line), &raw); err != nil {
		return // not JSON (e.g. stray log output on stdout); ignore
	}

	switch {
	case raw.Method == "" && raw.ID != nil:
		c.mu.Lock()
		ch, ok := c.pending[*raw.ID]
		c.mu.Unlock()
		if !ok {
			return
		}
		if raw.Error != nil {
			ch <- acpPendingResult{Err: fmt.Errorf("%s (code %d)", raw.Error.Message, raw.Error.Code)}
		} else {
			ch <- acpPendingResult{Result: raw.Result}
		}
	case raw.Method != "" && raw.ID != nil:
		c.handleAgentRequest(*raw.ID, raw.Method, raw.Params)
	case raw.Method != "" && raw.ID == nil:
		c.handleNotification(raw.Method, raw.Params)
	}
}

// handleAgentRequest answers requests the agent sends back to us. The only
// one we support is session/request_permission (auto-approved, since this
// tool exists specifically to delegate work autonomously); anything else
// gets a method-not-found error so the agent doesn't hang waiting forever.
func (c *acpClient) handleAgentRequest(id int64, method string, params json.RawMessage) {
	if method != "session/request_permission" {
		_ = c.writeMessage(map[string]any{
			"jsonrpc": "2.0", "id": id,
			"error": map[string]any{"code": -32601, "message": "method not supported by this headless ACP client: " + method},
		})
		return
	}
	var p struct {
		Options []struct {
			OptionID string `json:"optionId"`
			Kind     string `json:"kind"`
		} `json:"options"`
	}
	_ = json.Unmarshal(params, &p)
	chosen := ""
	for _, wantKind := range []string{"allow_once", "allow_always"} {
		for _, opt := range p.Options {
			if opt.Kind == wantKind {
				chosen = opt.OptionID
				break
			}
		}
		if chosen != "" {
			break
		}
	}
	var outcome map[string]any
	if chosen != "" {
		outcome = map[string]any{"outcome": "selected", "optionId": chosen}
	} else {
		outcome = map[string]any{"outcome": "cancelled"}
	}
	_ = c.writeMessage(map[string]any{
		"jsonrpc": "2.0", "id": id,
		"result": map[string]any{"outcome": outcome},
	})
}

// handleNotification collects the agent's streamed text response and
// counts tool calls from session/update notifications.
func (c *acpClient) handleNotification(method string, params json.RawMessage) {
	if method != "session/update" {
		return
	}
	var p struct {
		Update struct {
			SessionUpdate string `json:"sessionUpdate"`
			Content       *struct {
				Type string `json:"type"`
				Text string `json:"text"`
			} `json:"content"`
		} `json:"update"`
	}
	if err := json.Unmarshal(params, &p); err != nil {
		return
	}
	switch p.Update.SessionUpdate {
	case "agent_message_chunk":
		if p.Update.Content != nil && p.Update.Content.Type == "text" {
			c.textMu.Lock()
			c.textBuf.WriteString(p.Update.Content.Text)
			c.textMu.Unlock()
		}
	case "tool_call":
		c.textMu.Lock()
		c.toolCalls++
		c.textMu.Unlock()
	}
}

func (c *acpClient) collectedText() string {
	c.textMu.Lock()
	defer c.textMu.Unlock()
	return c.textBuf.String()
}

func (c *acpClient) toolCallCount() int {
	c.textMu.Lock()
	defer c.textMu.Unlock()
	return c.toolCalls
}

type acpAgentSpec struct {
	Command string
	Args    []string
	Cwd     string
	Timeout time.Duration
	Prompt  string
}

// runACPAgent spawns an ACP-speaking coding agent as a subprocess, runs it
// through initialize -> session/new -> session/prompt for a single prompt,
// and returns its collected text response plus a short status line.
func runACPAgent(ctx context.Context, spec acpAgentSpec) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, spec.Timeout)
	defer cancel()

	cmd := exec.CommandContext(ctx, spec.Command, spec.Args...)
	cmd.Dir = spec.Cwd
	cmd.Env = os.Environ()
	var stderrBuf bytes.Buffer
	cmd.Stderr = &stderrBuf

	stdin, err := cmd.StdinPipe()
	if err != nil {
		return "", err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return "", err
	}

	if err := cmd.Start(); err != nil {
		return "", fmt.Errorf("gagal start %q: %w", spec.Command, err)
	}
	defer func() {
		_ = stdin.Close()
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
		_ = cmd.Wait()
	}()

	client := newACPClient(stdin, stdout)
	go client.readLoop()

	if _, err := client.call(ctx, "initialize", map[string]any{
		"protocolVersion": 1,
		"clientCapabilities": map[string]any{
			"fs":       map[string]any{"readTextFile": false, "writeTextFile": false},
			"terminal": false,
		},
		"clientInfo": map[string]any{"name": "ryns-code-mcp", "version": serverVersion},
	}); err != nil {
		return "", fmt.Errorf("initialize gagal: %w (stderr: %s)", err, strings.TrimSpace(stderrBuf.String()))
	}

	sessResp, err := client.call(ctx, "session/new", map[string]any{
		"cwd":        spec.Cwd,
		"mcpServers": []any{},
	})
	if err != nil {
		return "", fmt.Errorf("session/new gagal: %w (stderr: %s)", err, strings.TrimSpace(stderrBuf.String()))
	}
	var sessData struct {
		SessionID string `json:"sessionId"`
	}
	if err := json.Unmarshal(sessResp, &sessData); err != nil || sessData.SessionID == "" {
		return "", fmt.Errorf("session/new response tidak ada sessionId: %s", string(sessResp))
	}

	promptResp, err := client.call(ctx, "session/prompt", map[string]any{
		"sessionId": sessData.SessionID,
		"prompt": []map[string]any{
			{"type": "text", "text": spec.Prompt},
		},
	})
	if err != nil {
		return "", fmt.Errorf("session/prompt gagal: %w (stderr: %s)", err, strings.TrimSpace(stderrBuf.String()))
	}
	var promptData struct {
		StopReason string `json:"stopReason"`
	}
	_ = json.Unmarshal(promptResp, &promptData)

	text := strings.TrimSpace(client.collectedText())
	text = stripACPNoiseTags(text)
	var sb strings.Builder
	if text == "" {
		sb.WriteString("(agent tidak mengembalikan teks respons)\n")
	} else {
		sb.WriteString(text)
		sb.WriteString("\n")
	}
	fmt.Fprintf(&sb, "\n[stop_reason: %s, tool_calls: %d]", promptData.StopReason, client.toolCallCount())
	return sb.String(), nil
}

// stripACPNoiseTags removes known agent-internal housekeeping tags that
// some ACP agents (observed: OpenCode) leak into agent_message_chunk text,
// e.g. <dcp-system-reminder>...</dcp-system-reminder> and <dcp-message-id>
// blocks used for their own context-compression bookkeeping.
var acpNoiseTagPattern = regexp.MustCompile(`(?s)<dcp-system-reminder>.*?</dcp-system-reminder>|<dcp-message-id>.*?</dcp-message-id>`)

func stripACPNoiseTags(s string) string {
	return strings.TrimSpace(acpNoiseTagPattern.ReplaceAllString(s, ""))
}

func runCommand(ctx context.Context, command, cwd string, timeout time.Duration) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, "/bin/bash", "-lc", command)
	cmd.Dir = cwd
	cmd.Env = os.Environ()
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	if errors.Is(ctx.Err(), context.DeadlineExceeded) {
		return fmt.Sprintf("timed_out: true\nstdout:\n%s\nstderr:\n%s", clip(stdout.String()), clip(stderr.String())), nil
	}
	code := 0
	if err != nil {
		var ee *exec.ExitError
		if errors.As(err, &ee) {
			code = ee.ExitCode()
		} else {
			return "", err
		}
	}
	out := fmt.Sprintf("exit_code: %d\nstdout:\n%s", code, clip(stdout.String()))
	if stderr.Len() > 0 {
		out += "\nstderr:\n" + clip(stderr.String())
	}
	return out, nil
}

type proc struct {
	ID        string
	Cmd       *exec.Cmd
	Stdin     io.WriteCloser
	Command   string
	CWD       string
	StartedAt string
	mu        sync.Mutex
	Stdout    strings.Builder
	Stderr    strings.Builder
	ExitCode  *int
	Signal    string
	Done      bool
}

var (
	procMu     sync.Mutex
	processes  = map[string]*proc{}
	nextProcID = 1
)

func startProcess(command, cwd string, interactive bool) (*proc, error) {
	var cmd *exec.Cmd
	if interactive {
		// `script` allocates a real PTY. This is important for TUI/interactive
		// programs such as 9router that refuse to render when stdout/stderr
		// are ordinary pipes. stdin/stdout are still exposed through the MCP
		// process tools so the caller can interact with the program.
		cmd = ptyCommand(command)
	} else {
		cmd = exec.Command("/bin/bash", "-lc", command)
	}
	cmd.Dir = cwd
	cmd.Env = os.Environ()
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	procMu.Lock()
	id := strconv.Itoa(nextProcID)
	nextProcID++
	procMu.Unlock()
	p := &proc{ID: id, Cmd: cmd, Stdin: stdin, Command: command, CWD: cwd, StartedAt: time.Now().UTC().Format(time.RFC3339Nano)}
	procMu.Lock()
	processes[id] = p
	procMu.Unlock()
	go copyOutput(&p.Stdout, stdout, &p.mu)
	go copyOutput(&p.Stderr, stderr, &p.mu)
	go func() {
		err := cmd.Wait()
		p.mu.Lock()
		defer p.mu.Unlock()
		p.Done = true
		if err != nil {
			var ee *exec.ExitError
			if errors.As(err, &ee) {
				code := ee.ExitCode()
				p.ExitCode = &code
				if ws, ok := ee.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
					p.Signal = ws.Signal().String()
				}
			}
		} else {
			code := 0
			p.ExitCode = &code
		}
	}()
	return p, nil
}

func copyOutput(dst *strings.Builder, src io.Reader, mu *sync.Mutex) {
	buf := make([]byte, 4096)
	for {
		n, err := src.Read(buf)
		if n > 0 {
			mu.Lock()
			dst.Write(buf[:n])
			if dst.Len() > maxOutput*2 {
				s := dst.String()
				dst.Reset()
				dst.WriteString(s[len(s)-maxOutput*2:])
			}
			mu.Unlock()
		}
		if err != nil {
			return
		}
	}
}

func getProc(id string) *proc { procMu.Lock(); defer procMu.Unlock(); return processes[id] }

// Tool input types.
type ExecuteInput struct {
	Command   string `json:"command" jsonschema:"command to execute"`
	CWD       string `json:"cwd,omitempty" jsonschema:"working directory relative to workspace"`
	TimeoutMS int    `json:"timeout_ms,omitempty" jsonschema:"timeout in milliseconds"`
}
type StartInput struct {
	Command     string `json:"command" jsonschema:"long-running shell command to start"`
	CWD         string `json:"cwd,omitempty" jsonschema:"working directory relative to workspace"`
	Interactive *bool  `json:"interactive,omitempty" jsonschema:"allocate a real pseudo-terminal for interactive TUI programs; defaults to true"`
}
type ProcessInput struct {
	ProcessID string `json:"process_id"`
	Clear     bool   `json:"clear,omitempty"`
}
type SendInput struct {
	ProcessID string `json:"process_id"`
	Input     string `json:"input"`
	Newline   bool   `json:"newline,omitempty"`
}
type KillInput struct {
	ProcessID string `json:"process_id"`
	Signal    string `json:"signal,omitempty"`
}
type ReadInput struct {
	Path      string `json:"path"`
	StartLine int    `json:"start_line,omitempty"`
	EndLine   int    `json:"end_line,omitempty"`
}
type WriteInput struct {
	Path    string `json:"path"`
	Content string `json:"content"`
}
type EditInput struct {
	Path       string `json:"path"`
	OldText    string `json:"old_text"`
	NewText    string `json:"new_text"`
	ReplaceAll bool   `json:"replace_all,omitempty"`
	DryRun     bool   `json:"dry_run,omitempty"`
}
type EditOp struct {
	OldText    string `json:"old_text"`
	NewText    string `json:"new_text"`
	ReplaceAll bool   `json:"replace_all,omitempty"`
}
type MultiEditInput struct {
	Path   string   `json:"path" jsonschema:"file to edit"`
	Edits  []EditOp `json:"edits" jsonschema:"ordered list of exact-text edits, applied atomically"`
	DryRun bool     `json:"dry_run,omitempty"`
}
type PatchInput struct {
	Patch  string `json:"patch"`
	CWD    string `json:"cwd,omitempty"`
	DryRun bool   `json:"dry_run,omitempty"`
}
type ListInput struct {
	Path string `json:"path,omitempty"`
}
type TreeInput struct {
	Path     string `json:"path,omitempty"`
	MaxDepth int    `json:"max_depth,omitempty"`
}
type SearchInput struct {
	Path       string `json:"path,omitempty"`
	Pattern    string `json:"pattern"`
	MaxResults int    `json:"max_results,omitempty"`
}
type GrepInput struct {
	Pattern    string `json:"pattern"`
	Path       string `json:"path,omitempty"`
	Glob       string `json:"glob,omitempty"`
	IgnoreCase bool   `json:"ignore_case,omitempty"`
	Context    int    `json:"context,omitempty"`
	MaxResults int    `json:"max_results,omitempty"`
}
type MkdirInput struct {
	Path string `json:"path"`
}
type DeleteInput struct {
	Path      string `json:"path"`
	Recursive bool   `json:"recursive,omitempty"`
}
type MoveInput struct {
	Source      string `json:"source"`
	Destination string `json:"destination"`
}
type GitInput struct {
	CWD string `json:"cwd,omitempty"`
}
type ScreenshotInput struct {
	Save   string `json:"save,omitempty" jsonschema:"optional path to also save the PNG (relative paths resolve under workspace)"`
	Window string `json:"window,omitempty" jsonschema:"X window id/name to capture; defaults to the full screen (root window)"`
}
type ClipboardSetInput struct {
	Text string `json:"text"`
}
type ClickInput struct {
	X      int  `json:"x"`
	Y      int  `json:"y"`
	Button int  `json:"button,omitempty" jsonschema:"mouse button: 1 for left, 2 for middle, 3 for right; default is 1"`
	Double bool `json:"double,omitempty" jsonschema:"send a double-click instead of a single click"`
}
type TypeTextInput struct {
	Text    string `json:"text"`
	DelayMs int    `json:"delay_ms,omitempty" jsonschema:"per-character delay in milliseconds; default 12"`
}
type KeyPressInput struct {
	Keys string `json:"keys" jsonschema:"xdotool key syntax, e.g. 'Return', 'ctrl+c', 'alt+Tab', 'super'"`
}
type NotifyInput struct {
	Title   string `json:"title"`
	Body    string `json:"body,omitempty"`
	Urgency string `json:"urgency,omitempty" jsonschema:"low, normal, or critical; default normal"`
}
type OCRInput struct {
	Window string `json:"window,omitempty" jsonschema:"X window id/name to capture; defaults to the full screen"`
}
type CameraInput struct {
	Device string `json:"device,omitempty" jsonschema:"video device path; defaults to /dev/video0"`
	Save   string `json:"save,omitempty" jsonschema:"optional path to also save the JPEG (relative paths resolve under workspace)"`
}
type RecordInput struct {
	Source          string `json:"source,omitempty" jsonschema:"'screen' or 'camera'; default 'screen'"`
	DurationSeconds int    `json:"duration_seconds" jsonschema:"how many seconds to record (1-120)"`
	Device          string `json:"device,omitempty" jsonschema:"camera device path when source=camera; defaults to /dev/video0"`
}
type SheetCreateInput struct {
	ID        string   `json:"id" jsonschema:"unique identifier for the sheet, letters numbers underscore or dash only"`
	Title     string   `json:"title" jsonschema:"human readable title shown in the sheet viewer"`
	Columns   []string `json:"columns,omitempty" jsonschema:"initial column names in order, may be left empty and added later"`
	Overwrite bool     `json:"overwrite,omitempty" jsonschema:"pass true to replace an existing sheet that already has this id instead of failing"`
}
type SheetIDInput struct {
	ID string `json:"id" jsonschema:"identifier of the sheet"`
}
type SheetGetInput struct {
	ID       string `json:"id" jsonschema:"identifier of the sheet"`
	StartRow int    `json:"start_row,omitempty" jsonschema:"first row index to show, zero based, defaults to the beginning"`
	EndRow   int    `json:"end_row,omitempty" jsonschema:"exclusive end row index to show, defaults to the last row"`
}
type SheetRenameInput struct {
	ID       string `json:"id" jsonschema:"identifier of the sheet"`
	NewTitle string `json:"new_title" jsonschema:"new display title for the sheet"`
}
type ColumnAddInput struct {
	ID          string `json:"id" jsonschema:"identifier of the sheet"`
	Name        string `json:"name" jsonschema:"name of the new column"`
	AfterColumn string `json:"after_column,omitempty" jsonschema:"name or zero based index of an existing column to insert the new column right after; leave empty to append at the end"`
}
type ColumnRemoveInput struct {
	ID     string `json:"id" jsonschema:"identifier of the sheet"`
	Column string `json:"column" jsonschema:"name or zero based index of the column to remove"`
}
type ColumnRenameInput struct {
	ID      string `json:"id" jsonschema:"identifier of the sheet"`
	OldName string `json:"old_name" jsonschema:"current name or zero based index of the column"`
	NewName string `json:"new_name" jsonschema:"new name to give the column"`
}
type RowAddInput struct {
	ID       string   `json:"id" jsonschema:"identifier of the sheet"`
	Values   []string `json:"values" jsonschema:"cell values for the new row, in the same order as the sheet's columns"`
	Position *int     `json:"position,omitempty" jsonschema:"zero based row index to insert at; when omitted the row is appended at the end"`
}
type RowRemoveInput struct {
	ID       string `json:"id" jsonschema:"identifier of the sheet"`
	RowIndex int    `json:"row_index" jsonschema:"zero based index of the row to remove"`
}
type CellSetInput struct {
	ID       string `json:"id" jsonschema:"identifier of the sheet"`
	RowIndex int    `json:"row_index" jsonschema:"zero based index of the row"`
	Column   string `json:"column" jsonschema:"name or zero based index of the column"`
	Value    string `json:"value" jsonschema:"new value to store in the cell"`
}
type CellGetInput struct {
	ID       string `json:"id" jsonschema:"identifier of the sheet"`
	RowIndex int    `json:"row_index" jsonschema:"zero based index of the row"`
	Column   string `json:"column" jsonschema:"name or zero based index of the column"`
}

// GoldPriceInput is intentionally empty: check_gold_price always fetches
// today's price, no parameters needed.
type GoldPriceInput struct{}

type goldPriceAPIItem struct {
	Weight       float64 `json:"weight"`
	WeightUnit   string  `json:"weightUnit"`
	SellPrice    int64   `json:"sellPrice"`
	RecordedDate string  `json:"recordedDate"`
}

type goldPriceAPIResponse struct {
	Success bool               `json:"success"`
	Data    []goldPriceAPIItem `json:"data"`
}

// ---- Todo tracker: a single global checklist, replaced wholesale on
// every todo_write call (mirrors Claude Code's TodoWrite tool). See
// PLAN_TODO_TRACKER.md for the design.

type TodoItem struct {
	ID         string `json:"id"`
	Content    string `json:"content"`
	Status     string `json:"status"`
	ActiveForm string `json:"active_form"`
}

type TodoState struct {
	Items     []TodoItem `json:"items"`
	UpdatedAt string     `json:"updated_at"`
}

type TodoItemInput struct {
	Content    string `json:"content" jsonschema:"the task description, written as an imperative, e.g. 'Run the test suite'"`
	Status     string `json:"status,omitempty" jsonschema:"one of pending, in_progress, or completed; defaults to pending when omitted"`
	ActiveForm string `json:"active_form,omitempty" jsonschema:"present continuous phrasing shown while this item is in progress, e.g. 'Running the test suite'; defaults to content when omitted"`
}
type TodoWriteInput struct {
	Items []TodoItemInput `json:"items" jsonschema:"the complete todo list for the current task, replacing whatever list was stored before"`
}
type TodoReadInput struct{}

type LintFormatInput struct {
	Path string `json:"path" jsonschema:"file or directory to check or format; relative paths resolve under workspace"`
	Fix  bool   `json:"fix,omitempty" jsonschema:"when true, apply formatting fixes in place; when false (the default) only report what needs fixing without changing any file"`
}

// SystemStatsInput is intentionally empty: system_stats always reports the
// current machine's live stats, no parameters needed.
type SystemStatsInput struct{}

type AcpOpenCodeInput struct {
	Prompt         string `json:"prompt" jsonschema:"the task or question to hand off to the OpenCode sub-agent"`
	Path           string `json:"path,omitempty" jsonschema:"working directory for the sub-agent, relative paths resolve under workspace; defaults to the workspace root"`
	TimeoutSeconds int    `json:"timeout_seconds,omitempty" jsonschema:"maximum seconds to wait for the sub-agent to finish; defaults to 300, capped at 1800"`
}

var lintIgnoreDirs = map[string]bool{
	".git": true, "node_modules": true, "vendor": true, "target": true,
	"dist": true, "build": true, ".next": true, "__pycache__": true,
}

// classifyLintFile buckets a file by extension into one of the language
// groups lint_format knows how to check/format, or "" if unsupported.
func classifyLintFile(path string) string {
	switch strings.ToLower(filepath.Ext(path)) {
	case ".go":
		return "go"
	case ".rs":
		return "rust"
	case ".js", ".jsx", ".ts", ".tsx", ".json", ".css", ".scss", ".html", ".md", ".yaml", ".yml":
		return "js"
	case ".py":
		return "py"
	default:
		return ""
	}
}

// collectLintFiles walks root (or just returns it if it's a single file)
// grouping supported files by language, skipping common heavy/vendor dirs.
func collectLintFiles(root string) (map[string][]string, error) {
	info, err := os.Stat(root)
	if err != nil {
		return nil, err
	}
	groups := map[string][]string{}
	if !info.IsDir() {
		if cat := classifyLintFile(root); cat != "" {
			groups[cat] = append(groups[cat], root)
		}
		return groups, nil
	}
	err = filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			if p != root && (lintIgnoreDirs[d.Name()] || strings.HasPrefix(d.Name(), ".")) {
				return filepath.SkipDir
			}
			return nil
		}
		if cat := classifyLintFile(p); cat != "" {
			groups[cat] = append(groups[cat], p)
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	return groups, nil
}

// runLintTool runs an external formatter/linter with a bounded timeout and
// returns its combined stdout+stderr plus any exec error.
func runLintTool(ctx context.Context, name string, args []string, dir string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	return string(out), err
}

func todoFilePath() string {
	return filepath.Join(workspace, "assets", "data", "todos.json")
}

func loadTodoState() (TodoState, error) {
	data, err := os.ReadFile(todoFilePath())
	if errors.Is(err, os.ErrNotExist) {
		return TodoState{Items: []TodoItem{}}, nil
	}
	if err != nil {
		return TodoState{}, err
	}
	var st TodoState
	if err := json.Unmarshal(data, &st); err != nil {
		return TodoState{}, err
	}
	if st.Items == nil {
		st.Items = []TodoItem{}
	}
	return st, nil
}

func saveTodoState(st TodoState) error {
	dir := filepath.Join(workspace, "assets", "data")
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}
	st.UpdatedAt = time.Now().UTC().Format(time.RFC3339)
	data, err := json.MarshalIndent(st, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(todoFilePath(), data, 0644)
}

func renderTodoList(st TodoState) string {
	if len(st.Items) == 0 {
		return "Belum ada todo list aktif."
	}
	var sb strings.Builder
	if st.UpdatedAt != "" {
		fmt.Fprintf(&sb, "Todo List (diupdate %s)\n", st.UpdatedAt)
	} else {
		sb.WriteString("Todo List\n")
	}
	inProgressCount := 0
	for i, item := range st.Items {
		mark := " "
		label := item.Content
		switch item.Status {
		case "in_progress":
			mark = "~"
			inProgressCount++
			if item.ActiveForm != "" {
				label = item.ActiveForm
			}
		case "completed":
			mark = "x"
		}
		fmt.Fprintf(&sb, "[%s] %d. %s\n", mark, i+1, label)
	}
	if inProgressCount > 1 {
		sb.WriteString("\nCatatan: lebih dari satu item berstatus in_progress sekaligus; idealnya cuma satu yang dikerjain di satu waktu.\n")
	}
	return sb.String()
}

func main() {
	port := flag.Int("port", 0, "serve Streamable HTTP on this port instead of stdio (0 = stdio)")
	flag.Parse()

	server := mcp.NewServer(&mcp.Implementation{Name: serverName, Version: serverVersion}, nil)
	setupTools(server)

	if *port > 0 {
		ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *port))
		if err != nil {
			log.Fatalf("listen failed on port %d: %v", *port, err)
		}
		log.Printf("%s v%s ready — %s (http://%s/mcp)", serverName, serverVersion, workspace, ln.Addr())
		h := mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return server },
			&mcp.StreamableHTTPOptions{
				// Requests arrive from the cloudflared tunnel with an external
				// Host header; the SDK's localhost DNS-rebinding check would
				// 403 them. Access control belongs at the tunnel edge.
				DisableLocalhostProtection: true,
				Stateless:                  true,
			})

		mux := http.NewServeMux()
		assetsDir := filepath.Join(workspace, "assets")
		if err := os.MkdirAll(assetsDir, 0755); err != nil {
			log.Printf("warning: could not create assets dir: %v", err)
		}
		mux.Handle("/assets/", http.StripPrefix("/assets/", http.FileServer(http.Dir(assetsDir))))
		// /api/save/<name> — lets a static page under /assets/ persist edits
		// back to disk as assets/data/<name>.json, so an editable HTML table
		// (add/remove rows, edit cells) can survive a page reload.
		mux.HandleFunc("/api/save/", func(w http.ResponseWriter, r *http.Request) {
			if r.Method != http.MethodPost {
				http.Error(w, `{"error":"method not allowed"}`, http.StatusMethodNotAllowed)
				return
			}
			rest := strings.Trim(strings.TrimPrefix(r.URL.Path, "/api/save/"), "/")
			if rest == "" {
				http.Error(w, `{"error":"invalid name"}`, http.StatusBadRequest)
				return
			}
			segments := strings.Split(rest, "/")
			for _, seg := range segments {
				if !pathSegmentPattern.MatchString(seg) {
					http.Error(w, `{"error":"invalid path segment"}`, http.StatusBadRequest)
					return
				}
			}
			body, err := io.ReadAll(io.LimitReader(r.Body, 2<<20))
			if err != nil {
				http.Error(w, `{"error":"read failed"}`, http.StatusBadRequest)
				return
			}
			if !json.Valid(body) {
				http.Error(w, `{"error":"body is not valid JSON"}`, http.StatusBadRequest)
				return
			}
			name := segments[len(segments)-1]
			dirParts := append([]string{assetsDir, "data"}, segments[:len(segments)-1]...)
			dataDir := filepath.Join(dirParts...)
			if err := os.MkdirAll(dataDir, 0755); err != nil {
				http.Error(w, `{"error":"server error"}`, http.StatusInternalServerError)
				return
			}
			if err := os.WriteFile(filepath.Join(dataDir, name+".json"), body, 0644); err != nil {
				http.Error(w, `{"error":"write failed"}`, http.StatusInternalServerError)
				return
			}
			w.Header().Set("Content-Type", "application/json")
			w.Write([]byte(`{"ok":true}`))
		})
		mux.Handle("/mcp", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			// log.Printf("MCP REQUEST: %s %s", r.Method, r.URL.String())
			// log.Printf("MCP HEADERS: %+v", r.Header)

			h.ServeHTTP(w, r)
		}))

		if err := http.Serve(ln, mux); err != nil {
			log.Printf("server stopped: %v", err)
		}
		return
	}

	log.SetOutput(os.Stderr)
	log.Printf("%s v%s ready — %s", serverName, serverVersion, workspace)
	if err := server.Run(context.Background(), &mcp.StdioTransport{}); err != nil {
		log.Printf("server stopped: %v", err)
	}
}

func setupTools(server *mcp.Server) {
	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "execute_command", Title: "Execute Command", Description: "Run a one-shot bash command inside RynsWorkspace. Good for git, npm, tests, builds and system inspection."}, func(ctx context.Context, _ *mcp.CallToolRequest, in ExecuteInput) (*mcp.CallToolResult, any, error) {
		if in.Command == "" {
			return textResult("command_error: command is required")
		}
		cwd, err := safePath(defaultString(in.CWD, "."))
		if err != nil {
			return toolError("command_error", err)
		}
		timeout := time.Duration(in.TimeoutMS) * time.Millisecond
		if in.TimeoutMS <= 0 {
			timeout = 30 * time.Second
		}
		out, err := runCommand(ctx, in.Command, cwd, timeout)
		if err != nil {
			return toolError("command_error", err)
		}
		return textResult(fmt.Sprintf("cwd: %s\ncommand: %s\n%s", cwd, in.Command, out))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "start_process", Title: "Start Process", Description: "Start a long-running shell command and keep its process/session alive."}, func(_ context.Context, _ *mcp.CallToolRequest, in StartInput) (*mcp.CallToolResult, any, error) {
		cwd, err := safePath(defaultString(in.CWD, "."))
		if err != nil {
			return toolError("start_error", err)
		}
		interactive := true
		if in.Interactive != nil {
			interactive = *in.Interactive
		}
		p, err := startProcess(in.Command, cwd, interactive)
		if err != nil {
			return toolError("start_error", err)
		}
		return textResult(mustJSON(map[string]any{"process_id": p.ID, "command": in.Command, "cwd": cwd, "started_at": p.StartedAt, "interactive": interactive}))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "process_output", Title: "Process Output", Description: "Read accumulated stdout/stderr and current state from a long-running process."}, func(_ context.Context, _ *mcp.CallToolRequest, in ProcessInput) (*mcp.CallToolResult, any, error) {
		p := getProc(in.ProcessID)
		if p == nil {
			return textResult("process_not_found: " + in.ProcessID)
		}
		p.mu.Lock()
		out := map[string]any{"process_id": p.ID, "command": p.Command, "cwd": p.CWD, "done": p.Done, "exit_code": p.ExitCode, "signal": p.Signal, "stdout": p.Stdout.String(), "stderr": p.Stderr.String()}
		if in.Clear {
			p.Stdout.Reset()
			p.Stderr.Reset()
		}
		p.mu.Unlock()
		return textResult(mustJSON(out))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "send_input", Title: "Send Process Input", Description: "Send stdin to a running process."}, func(_ context.Context, _ *mcp.CallToolRequest, in SendInput) (*mcp.CallToolResult, any, error) {
		p := getProc(in.ProcessID)
		if p == nil {
			return textResult("process_not_found: " + in.ProcessID)
		}
		if p.Done || p.Stdin == nil {
			return textResult("process_not_running: " + in.ProcessID)
		}
		data := in.Input
		if in.Newline {
			data += "\n"
		}
		if _, err := io.WriteString(p.Stdin, data); err != nil {
			return toolError("send_input_error", err)
		}
		return textResult(fmt.Sprintf("sent %d bytes to process %s", len(data), in.ProcessID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "list_processes", Title: "List Processes", Description: "List processes started by this MCP server."}, func(context.Context, *mcp.CallToolRequest, struct{}) (*mcp.CallToolResult, any, error) {
		procMu.Lock()
		defer procMu.Unlock()
		rows := make([]map[string]any, 0, len(processes))
		for _, p := range processes {
			p.mu.Lock()
			rows = append(rows, map[string]any{"process_id": p.ID, "command": p.Command, "cwd": p.CWD, "started_at": p.StartedAt, "done": p.Done, "exit_code": p.ExitCode, "signal": p.Signal})
			p.mu.Unlock()
		}
		return textResult(mustJSON(rows))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "kill_process", Title: "Kill Process", Description: "Terminate a process started by this MCP server."}, func(_ context.Context, _ *mcp.CallToolRequest, in KillInput) (*mcp.CallToolResult, any, error) {
		p := getProc(in.ProcessID)
		if p == nil {
			return textResult("process_not_found: " + in.ProcessID)
		}
		p.mu.Lock()
		done := p.Done
		p.mu.Unlock()
		if done {
			return textResult("process_already_done: " + in.ProcessID)
		}
		sig := in.Signal
		if sig == "" {
			sig = "SIGTERM"
		}
		var s os.Signal
		switch sig {
		case "SIGTERM":
			s = syscall.SIGTERM
		case "SIGKILL":
			s = syscall.SIGKILL
		case "SIGINT":
			s = syscall.SIGINT
		default:
			return textResult("kill_error: invalid signal")
		}
		if err := p.Cmd.Process.Signal(s); err != nil {
			return toolError("kill_error", err)
		}
		return textResult(fmt.Sprintf("sent %s to process %s", sig, in.ProcessID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "read_file", Title: "Read File", Description: "Read a file. Text is returned with cat -n style line numbers (scope it with start_line/end_line for big files; unscoped reads are capped around 2000 lines). Common image formats (png/jpg/jpeg/gif/webp/bmp) are returned as an inline image instead of text."}, func(_ context.Context, _ *mcp.CallToolRequest, in ReadInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("read_error", e)
		}
		if mime, ok := imageExt[strings.ToLower(filepath.Ext(p))]; ok {
			data, e := os.ReadFile(p)
			if e != nil {
				return toolError("read_error", e)
			}
			text := fmt.Sprintf("%s (%s, %d bytes)", p, mime, len(data))
			return &mcp.CallToolResult{Content: []mcp.Content{
				&mcp.TextContent{Text: text},
				&mcp.ImageContent{Data: data, MIMEType: mime},
			}}, ToolOutput{Text: text}, nil
		}
		b, e := os.ReadFile(p)
		if e != nil {
			return toolError("read_error", e)
		}
		raw := strings.ReplaceAll(string(b), "\r\n", "\n")
		lines := strings.Split(raw, "\n")
		if len(lines) > 0 && lines[len(lines)-1] == "" && strings.HasSuffix(raw, "\n") {
			lines = lines[:len(lines)-1]
		}
		total := len(lines)
		if total == 0 {
			return textResult(fmt.Sprintf("%s (empty file)", p))
		}
		start := in.StartLine
		if start < 1 {
			start = 1
		}
		if start > total {
			return textResult(fmt.Sprintf("%s (%d lines total) — start_line %d is past end of file", p, total, start))
		}
		end := in.EndLine
		autoCapped := false
		if end <= 0 || end > total {
			end = total
			if in.EndLine <= 0 && end-start+1 > maxReadLines {
				end = start - 1 + maxReadLines
				autoCapped = true
			}
		}
		var sb strings.Builder
		fmt.Fprintf(&sb, "%s (%d lines total)\n", p, total)
		sb.WriteString(numberLines(lines, start, end))
		if autoCapped {
			fmt.Fprintf(&sb, "...[showing lines %d-%d of %d; pass start_line/end_line to read more]\n", start, end, total)
		}
		return textResult(sb.String())
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "write_file", Title: "Write File", Description: "Create or overwrite a UTF-8 text file. Reports whether it created a new file or overwrote an existing one, with byte/line counts."}, func(_ context.Context, _ *mcp.CallToolRequest, in WriteInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("write_error", e)
		}
		prevInfo, statErr := os.Stat(p)
		existedBefore := statErr == nil
		var prevSize int64
		if existedBefore {
			prevSize = prevInfo.Size()
		}
		if e = os.MkdirAll(filepath.Dir(p), 0755); e != nil {
			return toolError("write_error", e)
		}
		if e = os.WriteFile(p, []byte(in.Content), 0644); e != nil {
			return toolError("write_error", e)
		}
		lineCount := strings.Count(in.Content, "\n") + 1
		if existedBefore {
			return textResult(fmt.Sprintf("overwrote %s: %d -> %d bytes (%d lines)", p, prevSize, len(in.Content), lineCount))
		}
		return textResult(fmt.Sprintf("created %s: %d bytes (%d lines)", p, len(in.Content), lineCount))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "edit_file", Title: "Edit File", Description: "Replace exact text in a file; refuses ambiguous matches unless replace_all is enabled. Returns a numbered context snippet around the change so you can confirm it without re-reading the file."}, func(_ context.Context, _ *mcp.CallToolRequest, in EditInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("edit_error", e)
		}
		b, e := os.ReadFile(p)
		if e != nil {
			return toolError("edit_error", e)
		}
		old := string(b)
		count := strings.Count(old, in.OldText)
		if count == 0 {
			return textResult("edit_error: old_text not found")
		}
		if !in.ReplaceAll && count != 1 {
			return textResult(fmt.Sprintf("edit_error: %d matches; make old_text unique or set replace_all=true", count))
		}
		firstIdx := strings.Index(old, in.OldText)
		lineNum := lineOfOffset(old, firstIdx)
		updated := old
		if in.ReplaceAll {
			updated = strings.ReplaceAll(old, in.OldText, in.NewText)
		} else {
			updated = strings.Replace(old, in.OldText, in.NewText, 1)
		}
		if !in.DryRun {
			if e = os.WriteFile(p, []byte(updated), 0644); e != nil {
				return toolError("edit_error", e)
			}
		}
		n := count
		if !in.ReplaceAll {
			n = 1
		}
		prefix := ""
		if in.DryRun {
			prefix = "dry-run: "
		}
		snippet := contextAroundLine(updated, lineNum, 2)
		return textResult(fmt.Sprintf("%sreplaced %d occurrence(s) in %s (near line %d)\n%s", prefix, n, p, lineNum, snippet))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "multi_edit_file", Title: "Multi Edit File", Description: "Apply several exact-text edits to one file atomically and in order: either every edit matches and the file is written once, or nothing is written and the failing edit is reported."}, func(_ context.Context, _ *mcp.CallToolRequest, in MultiEditInput) (*mcp.CallToolResult, any, error) {
		if len(in.Edits) == 0 {
			return textResult("multi_edit_error: edits is empty")
		}
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("multi_edit_error", e)
		}
		b, e := os.ReadFile(p)
		if e != nil {
			return toolError("multi_edit_error", e)
		}
		cur := string(b)
		for i, op := range in.Edits {
			count := strings.Count(cur, op.OldText)
			if count == 0 {
				return textResult(fmt.Sprintf("multi_edit_error: edit %d of %d — old_text not found (no changes written)", i+1, len(in.Edits)))
			}
			if !op.ReplaceAll && count != 1 {
				return textResult(fmt.Sprintf("multi_edit_error: edit %d of %d — %d matches; make old_text unique or set replace_all=true (no changes written)", i+1, len(in.Edits), count))
			}
			if op.ReplaceAll {
				cur = strings.ReplaceAll(cur, op.OldText, op.NewText)
			} else {
				cur = strings.Replace(cur, op.OldText, op.NewText, 1)
			}
		}
		prefix := ""
		if in.DryRun {
			prefix = "dry-run: "
		} else {
			if e = os.WriteFile(p, []byte(cur), 0644); e != nil {
				return toolError("multi_edit_error", e)
			}
		}
		return textResult(fmt.Sprintf("%sapplied %d edit(s) to %s", prefix, len(in.Edits), p))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "apply_patch", Title: "Apply Patch", Description: "Apply a unified diff patch to workspace files using the system patch utility."}, func(ctx context.Context, _ *mcp.CallToolRequest, in PatchInput) (*mcp.CallToolResult, any, error) {
		dir, e := safePath(defaultString(in.CWD, "."))
		if e != nil {
			return toolError("patch_error", e)
		}
		args := []string{"-p1"}
		if in.DryRun {
			args = append(args, "--dry-run")
		}
		cmd := exec.CommandContext(ctx, "patch", args...)
		cmd.Dir = dir
		cmd.Stdin = strings.NewReader(in.Patch)
		out, e := cmd.CombinedOutput()
		if e != nil {
			if ee, ok := e.(*exec.ExitError); ok {
				return textResult(fmt.Sprintf("exit_code: %d\n%s", ee.ExitCode(), string(out)))
			}
			return toolError("patch_error", e)
		}
		return textResult(fmt.Sprintf("exit_code: 0\n%s", string(out)))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "list_directory", Title: "List Directory", Description: "List directory entries with type and size."}, func(_ context.Context, _ *mcp.CallToolRequest, in ListInput) (*mcp.CallToolResult, any, error) {
		dir, e := safePath(defaultString(in.Path, "."))
		if e != nil {
			return toolError("list_error", e)
		}
		es, e := os.ReadDir(dir)
		if e != nil {
			return toolError("list_error", e)
		}
		sort.Slice(es, func(i, j int) bool { return es[i].Name() < es[j].Name() })
		var rows []string
		for _, x := range es {
			if x.IsDir() {
				rows = append(rows, "[DIR]  "+x.Name())
				continue
			}
			info, _ := x.Info()
			size := int64(0)
			if info != nil {
				size = info.Size()
			}
			rows = append(rows, fmt.Sprintf("[FILE] %s (%d bytes)", x.Name(), size))
		}
		if len(rows) == 0 {
			return textResult("(empty)")
		}
		return textResult(strings.Join(rows, "\n"))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "directory_tree", Title: "Directory Tree", Description: "Return a recursive workspace tree."}, func(_ context.Context, _ *mcp.CallToolRequest, in TreeInput) (*mcp.CallToolResult, any, error) {
		root, e := safePath(defaultString(in.Path, "."))
		if e != nil {
			return toolError("tree_error", e)
		}
		depth := in.MaxDepth
		if depth <= 0 {
			depth = 6
		}
		var walk func(string, int) ([]string, error)
		walk = func(d string, n int) ([]string, error) {
			es, e := os.ReadDir(d)
			if e != nil {
				return nil, e
			}
			sort.Slice(es, func(i, j int) bool { return es[i].Name() < es[j].Name() })
			var rows []string
			for _, x := range es {
				prefix := strings.Repeat("  ", n)
				if x.IsDir() {
					rows = append(rows, prefix+"[DIR] "+x.Name())
					if n < depth {
						sub, e := walk(filepath.Join(d, x.Name()), n+1)
						if e != nil {
							return nil, e
						}
						rows = append(rows, sub...)
					}
				} else {
					rows = append(rows, prefix+"[FILE]"+x.Name())
				}
			}
			return rows, nil
		}
		rows, e := walk(root, 0)
		if e != nil {
			return toolError("tree_error", e)
		}
		return textResult(root + "\n" + strings.Join(rows, "\n"))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "search_files", Title: "Search Files", Description: "Find files using ripgrep glob patterns."}, func(ctx context.Context, _ *mcp.CallToolRequest, in SearchInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(defaultString(in.Path, "."))
		if e != nil {
			return toolError("search_error", e)
		}
		max := in.MaxResults
		if max <= 0 {
			max = 500
		}
		cmd := exec.CommandContext(ctx, "rg", "--files", "--glob", in.Pattern, p)
		out, e := cmd.Output()
		if e != nil {
			if ee, ok := e.(*exec.ExitError); ok && ee.ExitCode() == 1 {
				return textResult("(no matches)")
			}
			return toolError("search_error", e)
		}
		lines := strings.Split(strings.TrimSpace(string(out)), "\n")
		if len(lines) > max {
			lines = lines[:max]
		}
		if len(lines) == 1 && lines[0] == "" {
			return textResult("(no matches)")
		}
		return textResult(strings.Join(lines, "\n"))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "grep", Title: "Grep", Description: "Search file contents recursively with ripgrep regex, glob, case and context support."}, func(ctx context.Context, _ *mcp.CallToolRequest, in GrepInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(defaultString(in.Path, "."))
		if e != nil {
			return toolError("grep_error", e)
		}
		max := in.MaxResults
		if max <= 0 {
			max = 500
		}
		args := []string{"-n", "--color=never", fmt.Sprintf("--max-count=%d", max)}
		if in.IgnoreCase {
			args = append(args, "-i")
		}
		if in.Context > 0 {
			args = append(args, fmt.Sprintf("-C%d", in.Context))
		}
		if in.Glob != "" {
			args = append(args, "--glob", in.Glob)
		}
		args = append(args, in.Pattern, p)
		cmd := exec.CommandContext(ctx, "rg", args...)
		out, e := cmd.Output()
		if e != nil {
			if ee, ok := e.(*exec.ExitError); ok && ee.ExitCode() == 1 {
				return textResult("(no matches)")
			}
			return toolError("grep_error", e)
		}
		return textResult(string(out))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "create_directory", Title: "Create Directory", Description: "Create a directory recursively."}, func(_ context.Context, _ *mcp.CallToolRequest, in MkdirInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("mkdir_error", e)
		}
		if e = os.MkdirAll(p, 0755); e != nil {
			return toolError("mkdir_error", e)
		}
		return textResult("created/exists: " + p)
	})
	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "delete_file", Title: "Delete File", Description: "Delete a file or directory; recursive deletion must be explicit."}, func(_ context.Context, _ *mcp.CallToolRequest, in DeleteInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("delete_error", e)
		}
		info, e := os.Stat(p)
		if e != nil {
			return toolError("delete_error", e)
		}
		if info.IsDir() && !in.Recursive {
			return textResult("delete_error: directory requires recursive=true")
		}
		if e = os.RemoveAll(p); e != nil {
			return toolError("delete_error", e)
		}
		return textResult("deleted: " + p)
	})
	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "move_file", Title: "Move File", Description: "Move or rename a file/directory inside the workspace."}, func(_ context.Context, _ *mcp.CallToolRequest, in MoveInput) (*mcp.CallToolResult, any, error) {
		a, e := safePath(in.Source)
		if e != nil {
			return toolError("move_error", e)
		}
		b, e := safePath(in.Destination)
		if e != nil {
			return toolError("move_error", e)
		}
		if e = os.Rename(a, b); e != nil {
			return toolError("move_error", e)
		}
		return textResult(fmt.Sprintf("moved %s -> %s", a, b))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "get_file_info", Title: "Get File Info", Description: "Return file/directory metadata."}, func(_ context.Context, _ *mcp.CallToolRequest, in ListInput) (*mcp.CallToolResult, any, error) {
		p, e := safePath(in.Path)
		if e != nil {
			return toolError("stat_error", e)
		}
		s, e := os.Stat(p)
		if e != nil {
			return toolError("stat_error", e)
		}
		kind := "file"
		if s.IsDir() {
			kind = "directory"
		}
		return textResult(mustJSON(map[string]any{"path": p, "type": kind, "size": s.Size(), "mode": fmt.Sprintf("%o", s.Mode().Perm()), "created": s.ModTime().UTC().Format(time.RFC3339Nano), "modified": s.ModTime().UTC().Format(time.RFC3339Nano)}))
	})

	mcp.AddTool(server, &mcp.Tool{Name: "screenshot", Title: "Screenshot", Description: "Capture the current screen (or a specific X window) as a PNG image, returned inline. Optionally also saves a copy to disk."}, func(ctx context.Context, _ *mcp.CallToolRequest, in ScreenshotInput) (*mcp.CallToolResult, any, error) {
		tmp, err := os.CreateTemp("", "ryns-screenshot-*.png")
		if err != nil {
			return toolError("screenshot_error", err)
		}
		tmpPath := tmp.Name()
		tmp.Close()
		defer os.Remove(tmpPath)

		out, err := captureScreen(ctx, in.Window, tmpPath)
		if err != nil {
			return textResult(fmt.Sprintf("screenshot_error: %v\n%s", err, string(out)))
		}

		data, err := os.ReadFile(tmpPath)
		if err != nil {
			return toolError("screenshot_error", err)
		}

		savedNote := ""
		if in.Save != "" {
			dest, e := safePath(in.Save)
			if e != nil {
				return toolError("screenshot_error", e)
			}
			if e = os.MkdirAll(filepath.Dir(dest), 0755); e != nil {
				return toolError("screenshot_error", e)
			}
			if e = os.WriteFile(dest, data, 0644); e != nil {
				return toolError("screenshot_error", e)
			}
			savedNote = fmt.Sprintf(" (saved to %s)", dest)
		}

		text := fmt.Sprintf("screenshot captured, %d bytes%s", len(data), savedNote)
		return &mcp.CallToolResult{Content: []mcp.Content{
			&mcp.TextContent{Text: text},
			&mcp.ImageContent{Data: data, MIMEType: "image/png"},
		}}, ToolOutput{Text: text}, nil
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "clipboard_get", Title: "Clipboard Get", Description: "Read the current text on the X clipboard."}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		out, err := clipboardRead(ctx)
		if err != nil {
			return textResult("(clipboard is empty or unavailable)")
		}
		return textResult(string(out))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "clipboard_set", Title: "Clipboard Set", Description: "Set the X clipboard to the given text."}, func(ctx context.Context, _ *mcp.CallToolRequest, in ClipboardSetInput) (*mcp.CallToolResult, any, error) {
		out, err := clipboardWrite(ctx, in.Text)
		if err != nil {
			return textResult(fmt.Sprintf("clipboard_error: %v\n%s", err, string(out)))
		}
		return textResult(fmt.Sprintf("clipboard set (%d bytes)", len(in.Text)))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "mouse_click", Title: "Mouse Click", Description: "Move the mouse to (x,y) and click. button: 1=left, 2=middle, 3=right."}, func(ctx context.Context, _ *mcp.CallToolRequest, in ClickInput) (*mcp.CallToolResult, any, error) {
		button := in.Button
		if button == 0 {
			button = 1
		}
		if isDarwin {
			res, err := macMouseClick(ctx, in.X, in.Y, button, in.Double)
			if err != nil {
				return textResult(fmt.Sprintf("mouse_click_error: %v", err))
			}
			return textResult(res)
		}
		args := []string{"mousemove", "--sync", strconv.Itoa(in.X), strconv.Itoa(in.Y), "click"}
		if in.Double {
			args = append(args, "--repeat", "2", "--delay", "120")
		}
		args = append(args, strconv.Itoa(button))
		out, err := runXTool(ctx, 5*time.Second, "xdotool", args...)
		if err != nil {
			return textResult(fmt.Sprintf("mouse_click_error: %v\n%s", err, string(out)))
		}
		kind := "click"
		if in.Double {
			kind = "double-click"
		}
		return textResult(fmt.Sprintf("%s at (%d,%d) button %d", kind, in.X, in.Y, button))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "keyboard_type", Title: "Keyboard Type", Description: "Type literal text at the current input focus, as if typed on the keyboard."}, func(ctx context.Context, _ *mcp.CallToolRequest, in TypeTextInput) (*mcp.CallToolResult, any, error) {
		delay := in.DelayMs
		if delay <= 0 {
			delay = 12
		}
		if isDarwin {
			if out, err := macTypeText(ctx, in.Text); err != nil {
				return textResult(fmt.Sprintf("keyboard_type_error: %v\n%s", err, string(out)))
			}
			return textResult(fmt.Sprintf("typed %d character(s)", len(in.Text)))
		}
		out, err := runXTool(ctx, 30*time.Second, "xdotool", "type", "--delay", strconv.Itoa(delay), "--", in.Text)
		if err != nil {
			return textResult(fmt.Sprintf("keyboard_type_error: %v\n%s", err, string(out)))
		}
		return textResult(fmt.Sprintf("typed %d character(s)", len(in.Text)))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "key_press", Title: "Key Press", Description: "Send a key or key combo using xdotool key syntax, e.g. 'Return', 'ctrl+c', 'alt+Tab', 'super'."}, func(ctx context.Context, _ *mcp.CallToolRequest, in KeyPressInput) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			if err := macPressKeys(ctx, in.Keys); err != nil {
				return textResult(fmt.Sprintf("key_press_error: %v", err))
			}
			return textResult(fmt.Sprintf("sent key(s): %s", in.Keys))
		}
		out, err := runXTool(ctx, 5*time.Second, "xdotool", "key", "--", in.Keys)
		if err != nil {
			return textResult(fmt.Sprintf("key_press_error: %v\n%s", err, string(out)))
		}
		return textResult(fmt.Sprintf("sent key(s): %s", in.Keys))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "get_active_window", Title: "Get Active Window", Description: "Return the id, title, and window class of the currently focused window."}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			return textResult(macActiveWindow(ctx))
		}
		idOut, err := runXTool(ctx, 5*time.Second, "xdotool", "getactivewindow")
		if err != nil {
			return textResult(fmt.Sprintf("get_active_window_error: %v\n%s", err, string(idOut)))
		}
		id := strings.TrimSpace(string(idOut))
		nameOut, _ := runXTool(ctx, 5*time.Second, "xdotool", "getwindowname", id)
		classOut, _ := runXTool(ctx, 5*time.Second, "xdotool", "getwindowclassname", id)
		return textResult(fmt.Sprintf("id: %s\ntitle: %s\nclass: %s", id, strings.TrimSpace(string(nameOut)), strings.TrimSpace(string(classOut))))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "list_windows", Title: "List Windows", Description: "List visible windows with their id and title."}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			return textResult(macListWindows(ctx))
		}
		out, err := runXTool(ctx, 5*time.Second, "xdotool", "search", "--onlyvisible", "--name", ".*")
		if err != nil {
			return textResult(fmt.Sprintf("list_windows_error: %v\n%s", err, string(out)))
		}
		ids := strings.Fields(string(out))
		var sb strings.Builder
		for _, id := range ids {
			nameOut, e := runXTool(ctx, 3*time.Second, "xdotool", "getwindowname", id)
			if e != nil {
				continue
			}
			fmt.Fprintf(&sb, "%s\t%s\n", id, strings.TrimSpace(string(nameOut)))
		}
		if sb.Len() == 0 {
			return textResult("(no windows found)")
		}
		return textResult(sb.String())
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "notify_send", Title: "Send Notification", Description: "Show a desktop notification popup."}, func(ctx context.Context, _ *mcp.CallToolRequest, in NotifyInput) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			if out, err := macNotify(ctx, in.Title, in.Body); err != nil {
				return textResult(fmt.Sprintf("notify_error: %v\n%s", err, string(out)))
			}
			return textResult("notification sent")
		}
		urgency := in.Urgency
		if urgency == "" {
			urgency = "normal"
		}
		args := []string{"-u", urgency, in.Title}
		if in.Body != "" {
			args = append(args, in.Body)
		}
		out, err := runXTool(ctx, 5*time.Second, "notify-send", args...)
		if err != nil {
			return textResult(fmt.Sprintf("notify_error: %v\n%s", err, string(out)))
		}
		return textResult("notification sent")
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "ocr_screenshot", Title: "OCR Screenshot", Description: "Capture the screen (or a specific window) and run OCR to extract any visible text."}, func(ctx context.Context, _ *mcp.CallToolRequest, in OCRInput) (*mcp.CallToolResult, any, error) {
		tmpImg, err := os.CreateTemp("", "ryns-ocr-*.png")
		if err != nil {
			return toolError("ocr_error", err)
		}
		imgPath := tmpImg.Name()
		tmpImg.Close()
		defer os.Remove(imgPath)

		capOut, err := captureScreen(ctx, in.Window, imgPath)
		if err != nil {
			return textResult(fmt.Sprintf("ocr_error: screenshot failed: %v\n%s", err, string(capOut)))
		}

		outBase := strings.TrimSuffix(imgPath, ".png")
		out, err := runXTool(ctx, 20*time.Second, "tesseract", imgPath, outBase, "-l", "eng")
		defer os.Remove(outBase + ".txt")
		if err != nil {
			return textResult(fmt.Sprintf("ocr_error: %v\n%s", err, string(out)))
		}
		textBytes, err := os.ReadFile(outBase + ".txt")
		if err != nil {
			return toolError("ocr_error", err)
		}
		text := strings.TrimSpace(string(textBytes))
		if text == "" {
			text = "(no text detected)"
		}
		return textResult(text)
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "camera_capture", Title: "Camera Capture", Description: "Capture a single JPEG photo from the webcam and return it inline. Optionally also saves a copy to disk."}, func(ctx context.Context, _ *mcp.CallToolRequest, in CameraInput) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			return textResult("camera_error: belum didukung di macOS (butuh ffmpeg avfoundation); saat ini khusus Linux")
		}
		device := in.Device
		if device == "" {
			device = "/dev/video0"
		}
		tmp, err := os.CreateTemp("", "ryns-camera-*.jpg")
		if err != nil {
			return toolError("camera_error", err)
		}
		tmpPath := tmp.Name()
		tmp.Close()
		defer os.Remove(tmpPath)

		cctx, cancel := context.WithTimeout(ctx, 15*time.Second)
		defer cancel()
		cmd := exec.CommandContext(cctx, "ffmpeg", "-y", "-f", "v4l2", "-input_format", "mjpeg", "-video_size", "1280x720", "-i", device, "-frames:v", "1", "-update", "1", tmpPath)
		out, err := cmd.CombinedOutput()
		if err != nil {
			return textResult(fmt.Sprintf("camera_error: %v\n%s", err, string(out)))
		}

		data, err := os.ReadFile(tmpPath)
		if err != nil {
			return toolError("camera_error", err)
		}
		if len(data) == 0 {
			return textResult("camera_error: capture produced an empty file — camera may be busy or unavailable")
		}

		savedNote := ""
		if in.Save != "" {
			dest, e := safePath(in.Save)
			if e != nil {
				return toolError("camera_error", e)
			}
			if e = os.MkdirAll(filepath.Dir(dest), 0755); e != nil {
				return toolError("camera_error", e)
			}
			if e = os.WriteFile(dest, data, 0644); e != nil {
				return toolError("camera_error", e)
			}
			savedNote = fmt.Sprintf(" (saved to %s)", dest)
		}

		text := fmt.Sprintf("photo captured from %s, %d bytes%s", device, len(data), savedNote)
		return &mcp.CallToolResult{Content: []mcp.Content{
			&mcp.TextContent{Text: text},
			&mcp.ImageContent{Data: data, MIMEType: "image/jpeg"},
		}}, ToolOutput{Text: text}, nil
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "record_video", Title: "Record Video", Description: "Record the screen or the webcam for a set number of seconds (max 120), save it as an MP4 under the public assets/ folder, and return its public URL — no video bytes are sent back through the tool result, just the link."}, func(ctx context.Context, _ *mcp.CallToolRequest, in RecordInput) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			return textResult("record_error: belum didukung di macOS (butuh ffmpeg avfoundation); saat ini khusus Linux")
		}
		source := in.Source
		if source == "" {
			source = "screen"
		}
		if source != "screen" && source != "camera" {
			return textResult("record_error: source must be 'screen' or 'camera'")
		}
		secs := in.DurationSeconds
		if secs <= 0 {
			return textResult("record_error: duration_seconds must be greater than 0")
		}
		if secs > maxRecordSecs {
			return textResult(fmt.Sprintf("record_error: duration_seconds capped at %d", maxRecordSecs))
		}

		assetsDir := filepath.Join(workspace, "assets")
		if e := os.MkdirAll(assetsDir, 0755); e != nil {
			return toolError("record_error", e)
		}
		filename := fmt.Sprintf("record-%s-%s.mp4", source, time.Now().Format("20060102-150405"))
		outPath := filepath.Join(assetsDir, filename)

		var cmd *exec.Cmd
		cctx, cancel := context.WithTimeout(ctx, time.Duration(secs+15)*time.Second)
		defer cancel()
		if source == "screen" {
			geomOut, e := runXTool(ctx, 5*time.Second, "xdotool", "getdisplaygeometry")
			size := "1920x1080"
			if e == nil {
				size = strings.ReplaceAll(strings.TrimSpace(string(geomOut)), " ", "x")
			}
			cmd = exec.CommandContext(cctx, "ffmpeg", "-y", "-f", "x11grab", "-video_size", size, "-framerate", "30", "-i", ":0.0", "-t", strconv.Itoa(secs), "-pix_fmt", "yuv420p", "-vcodec", "libx264", "-preset", "veryfast", outPath)
			cmd.Env = xEnv()
		} else {
			device := in.Device
			if device == "" {
				device = "/dev/video0"
			}
			cmd = exec.CommandContext(cctx, "ffmpeg", "-y", "-f", "v4l2", "-input_format", "mjpeg", "-video_size", "1280x720", "-framerate", "30", "-i", device, "-t", strconv.Itoa(secs), "-pix_fmt", "yuv420p", "-vcodec", "libx264", "-preset", "veryfast", outPath)
		}
		out, err := cmd.CombinedOutput()
		if err != nil {
			return textResult(fmt.Sprintf("record_error: %v\n%s", err, string(out)))
		}

		info, e := os.Stat(outPath)
		if e != nil {
			return textResult("record_error: recording finished but the output file is missing")
		}

		url := fmt.Sprintf("%s/assets/%s", publicBaseURL, filename)
		return textResult(fmt.Sprintf("recorded %d second(s) from %s: %d bytes\n%s", secs, source, info.Size(), url))
	})

	registerGit(server, "git_status", []string{"status", "--short", "--branch"}, "Show git status and branch.")
	registerGit(server, "git_diff", []string{"diff", "--no-ext-diff"}, "Show working-tree diff.")
	registerGit(server, "git_log", []string{"log", "--oneline", "--decorate", "-20"}, "Show recent commits.")
	registerGit(server, "git_branch", []string{"branch", "-vv"}, "Show branches and tracking info.")

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "workspace_info", Title: "Workspace Info", Description: "Show workspace and runtime information."}, func(context.Context, *mcp.CallToolRequest, struct{}) (*mcp.CallToolResult, any, error) {
		procMu.Lock()
		ids := make([]string, 0, len(processes))
		for id := range processes {
			ids = append(ids, id)
		}
		procMu.Unlock()
		return textResult(mustJSON(map[string]any{"workspace": workspace, "hostname": hostname(), "platform": runtime.GOOS, "arch": runtime.GOARCH, "node": "not used (Go)", "go": runtime.Version(), "cpu_count": runtime.NumCPU(), "active_processes": ids}))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_create", Title: "Sheet Create", Description: "Create a new local sheet and register it in the sheet index. Fails if a sheet with the same id already exists unless overwrite is set."}, func(_ context.Context, _ *mcp.CallToolRequest, in SheetCreateInput) (*mcp.CallToolResult, any, error) {
		if err := validSheetID(in.ID); err != nil {
			return toolError("sheet_create failed", err)
		}
		if strings.TrimSpace(in.Title) == "" {
			return toolError("sheet_create failed", fmt.Errorf("title tidak boleh kosong"))
		}
		if _, err := os.Stat(sheetFilePath(in.ID)); err == nil && !in.Overwrite {
			return toolError("sheet_create failed", fmt.Errorf("sheet %q sudah ada; set overwrite=true untuk menimpa", in.ID))
		}
		columns := in.Columns
		if columns == nil {
			columns = []string{}
		}
		now := time.Now().UTC().Format(time.RFC3339)
		s := Sheet{ID: in.ID, Title: in.Title, Columns: columns, Rows: [][]string{}, CreatedAt: now, UpdatedAt: now}
		if err := saveSheet(s); err != nil {
			return toolError("sheet_create failed", err)
		}
		if err := upsertSheetIndexEntry(in.ID, in.Title); err != nil {
			return toolError("sheet_create failed", err)
		}
		return textResult(fmt.Sprintf("Sheet %q (%s) dibuat dengan %d kolom.", in.ID, in.Title, len(columns)))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_list", Title: "Sheet List", Description: "List every sheet in the index with its id, title, row count, and column count."}, func(_ context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		idx, err := loadSheetIndex()
		if err != nil {
			return toolError("sheet_list failed", err)
		}
		if len(idx.Sheets) == 0 {
			return textResult("Belum ada sheet.")
		}
		var sb strings.Builder
		for _, e := range idx.Sheets {
			s, err := loadSheet(e.ID)
			if err != nil {
				fmt.Fprintf(&sb, "%s — %s (gagal dibaca: %v)\n", e.ID, e.Title, err)
				continue
			}
			fmt.Fprintf(&sb, "%s — %s (%d baris, %d kolom)\n", s.ID, s.Title, len(s.Rows), len(s.Columns))
		}
		return textResult(sb.String())
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_get", Title: "Sheet Get", Description: "Show the contents of a sheet as a readable text table, optionally scoped to a row range for large sheets."}, func(_ context.Context, _ *mcp.CallToolRequest, in SheetGetInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("sheet_get failed", err)
		}
		return textResult(renderSheetTable(s, in.StartRow, in.EndRow))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_delete", Title: "Sheet Delete", Description: "Delete a sheet's underlying file and remove its entry from the index."}, func(_ context.Context, _ *mcp.CallToolRequest, in SheetIDInput) (*mcp.CallToolResult, any, error) {
		if err := os.Remove(sheetFilePath(in.ID)); err != nil && !errors.Is(err, os.ErrNotExist) {
			return toolError("sheet_delete failed", err)
		}
		if err := removeSheetIndexEntry(in.ID); err != nil {
			return toolError("sheet_delete failed", err)
		}
		return textResult(fmt.Sprintf("Sheet %q dihapus.", in.ID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_rename", Title: "Sheet Rename", Description: "Change a sheet's display title while keeping its id and data unchanged."}, func(_ context.Context, _ *mcp.CallToolRequest, in SheetRenameInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("sheet_rename failed", err)
		}
		s.Title = in.NewTitle
		if err := saveSheet(s); err != nil {
			return toolError("sheet_rename failed", err)
		}
		if err := upsertSheetIndexEntry(in.ID, in.NewTitle); err != nil {
			return toolError("sheet_rename failed", err)
		}
		return textResult(fmt.Sprintf("Sheet %q diganti judulnya jadi %q.", in.ID, in.NewTitle))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "column_add", Title: "Column Add", Description: "Add a new column to a sheet. When after_column is given, the new column is inserted right after that column instead of at the end, and every existing row gains an empty value in the new position."}, func(_ context.Context, _ *mcp.CallToolRequest, in ColumnAddInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("column_add failed", err)
		}
		if strings.TrimSpace(in.Name) == "" {
			return toolError("column_add failed", fmt.Errorf("nama kolom tidak boleh kosong"))
		}
		insertAt := len(s.Columns)
		if in.AfterColumn != "" {
			idx, err := resolveColumnIndex(s.Columns, in.AfterColumn)
			if err != nil {
				return toolError("column_add failed", err)
			}
			insertAt = idx + 1
		}
		newColumns := make([]string, 0, len(s.Columns)+1)
		newColumns = append(newColumns, s.Columns[:insertAt]...)
		newColumns = append(newColumns, in.Name)
		newColumns = append(newColumns, s.Columns[insertAt:]...)
		s.Columns = newColumns
		for i, row := range s.Rows {
			cut := insertAt
			if cut > len(row) {
				cut = len(row)
			}
			newRow := make([]string, 0, len(row)+1)
			newRow = append(newRow, row[:cut]...)
			newRow = append(newRow, "")
			if cut < len(row) {
				newRow = append(newRow, row[cut:]...)
			}
			s.Rows[i] = newRow
		}
		if err := saveSheet(s); err != nil {
			return toolError("column_add failed", err)
		}
		return textResult(fmt.Sprintf("Kolom %q ditambahkan ke sheet %q pada posisi %d.", in.Name, in.ID, insertAt))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "column_remove", Title: "Column Remove", Description: "Remove a column from a sheet by name or index, dropping that value from every row."}, func(_ context.Context, _ *mcp.CallToolRequest, in ColumnRemoveInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("column_remove failed", err)
		}
		idx, err := resolveColumnIndex(s.Columns, in.Column)
		if err != nil {
			return toolError("column_remove failed", err)
		}
		removedName := s.Columns[idx]
		s.Columns = append(s.Columns[:idx], s.Columns[idx+1:]...)
		for i, row := range s.Rows {
			if idx < len(row) {
				s.Rows[i] = append(row[:idx], row[idx+1:]...)
			}
		}
		if err := saveSheet(s); err != nil {
			return toolError("column_remove failed", err)
		}
		return textResult(fmt.Sprintf("Kolom %q dihapus dari sheet %q.", removedName, in.ID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "column_rename", Title: "Column Rename", Description: "Rename a column in a sheet, identified by its current name or index."}, func(_ context.Context, _ *mcp.CallToolRequest, in ColumnRenameInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("column_rename failed", err)
		}
		idx, err := resolveColumnIndex(s.Columns, in.OldName)
		if err != nil {
			return toolError("column_rename failed", err)
		}
		old := s.Columns[idx]
		s.Columns[idx] = in.NewName
		if err := saveSheet(s); err != nil {
			return toolError("column_rename failed", err)
		}
		return textResult(fmt.Sprintf("Kolom %q di sheet %q diganti jadi %q.", old, in.ID, in.NewName))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "row_add", Title: "Row Add", Description: "Add a new row of values to a sheet, appended at the end by default or inserted at a given zero based position."}, func(_ context.Context, _ *mcp.CallToolRequest, in RowAddInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("row_add failed", err)
		}
		pos := len(s.Rows)
		if in.Position != nil {
			p := *in.Position
			if p < 0 || p > len(s.Rows) {
				return toolError("row_add failed", fmt.Errorf("position %d di luar jangkauan (0..%d)", p, len(s.Rows)))
			}
			pos = p
		}
		newRows := make([][]string, 0, len(s.Rows)+1)
		newRows = append(newRows, s.Rows[:pos]...)
		newRows = append(newRows, in.Values)
		newRows = append(newRows, s.Rows[pos:]...)
		s.Rows = newRows
		if err := saveSheet(s); err != nil {
			return toolError("row_add failed", err)
		}
		return textResult(fmt.Sprintf("Baris baru ditambahkan ke sheet %q pada posisi %d.", in.ID, pos))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "row_remove", Title: "Row Remove", Description: "Remove a row from a sheet by its zero based index."}, func(_ context.Context, _ *mcp.CallToolRequest, in RowRemoveInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("row_remove failed", err)
		}
		if in.RowIndex < 0 || in.RowIndex >= len(s.Rows) {
			return toolError("row_remove failed", fmt.Errorf("row_index %d di luar jangkauan (0..%d)", in.RowIndex, len(s.Rows)-1))
		}
		s.Rows = append(s.Rows[:in.RowIndex], s.Rows[in.RowIndex+1:]...)
		if err := saveSheet(s); err != nil {
			return toolError("row_remove failed", err)
		}
		return textResult(fmt.Sprintf("Baris %d dihapus dari sheet %q.", in.RowIndex, in.ID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "cell_set", Title: "Cell Set", Description: "Set the value of a single cell in a sheet, identified by row index and column name or index."}, func(_ context.Context, _ *mcp.CallToolRequest, in CellSetInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("cell_set failed", err)
		}
		if in.RowIndex < 0 || in.RowIndex >= len(s.Rows) {
			return toolError("cell_set failed", fmt.Errorf("row_index %d di luar jangkauan (0..%d)", in.RowIndex, len(s.Rows)-1))
		}
		colIdx, err := resolveColumnIndex(s.Columns, in.Column)
		if err != nil {
			return toolError("cell_set failed", err)
		}
		row := s.Rows[in.RowIndex]
		for len(row) <= colIdx {
			row = append(row, "")
		}
		row[colIdx] = in.Value
		s.Rows[in.RowIndex] = row
		if err := saveSheet(s); err != nil {
			return toolError("cell_set failed", err)
		}
		return textResult(fmt.Sprintf("Sel [%d, %s] di sheet %q diset jadi %q.", in.RowIndex, s.Columns[colIdx], in.ID, in.Value))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "cell_get", Title: "Cell Get", Description: "Read the value of a single cell in a sheet, identified by row index and column name or index."}, func(_ context.Context, _ *mcp.CallToolRequest, in CellGetInput) (*mcp.CallToolResult, any, error) {
		s, err := loadSheet(in.ID)
		if err != nil {
			return toolError("cell_get failed", err)
		}
		if in.RowIndex < 0 || in.RowIndex >= len(s.Rows) {
			return toolError("cell_get failed", fmt.Errorf("row_index %d di luar jangkauan (0..%d)", in.RowIndex, len(s.Rows)-1))
		}
		colIdx, err := resolveColumnIndex(s.Columns, in.Column)
		if err != nil {
			return toolError("cell_get failed", err)
		}
		row := s.Rows[in.RowIndex]
		val := ""
		if colIdx < len(row) {
			val = row[colIdx]
		}
		return textResult(val)
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "sheet_url", Title: "Sheet URL", Description: "Return the public URL where a sheet can be opened and edited in a browser."}, func(_ context.Context, _ *mcp.CallToolRequest, in SheetIDInput) (*mcp.CallToolResult, any, error) {
		if _, err := loadSheet(in.ID); err != nil {
			return toolError("sheet_url failed", err)
		}
		return textResult(fmt.Sprintf("%s/assets/sheet.html?id=%s", publicBaseURL, in.ID))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "check_gold_price", Title: "Check Gold Price", Description: "Fetch today's official Antam Logam Mulia gold price (24K) plus an estimated 22K conversion, sourced live from logammulia.com."}, func(ctx context.Context, _ *mcp.CallToolRequest, _ GoldPriceInput) (*mcp.CallToolResult, any, error) {
		ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, "https://logam-mulia-api.iamutaki.workers.dev/api/prices/logammulia", nil)
		if err != nil {
			return toolError("gold_price_error", err)
		}
		client := &http.Client{Timeout: 10 * time.Second}
		resp, err := client.Do(req)
		if err != nil {
			return toolError("gold_price_error", fmt.Errorf("gagal menghubungi API harga emas: %w", err))
		}
		defer resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			return toolError("gold_price_error", fmt.Errorf("API harga emas balas status %d", resp.StatusCode))
		}
		body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		if err != nil {
			return toolError("gold_price_error", err)
		}
		var parsed goldPriceAPIResponse
		if err := json.Unmarshal(body, &parsed); err != nil {
			return toolError("gold_price_error", fmt.Errorf("gagal parse response API harga emas: %w", err))
		}
		if !parsed.Success {
			return toolError("gold_price_error", fmt.Errorf("API harga emas mengembalikan success=false"))
		}
		var gram1 *goldPriceAPIItem
		for i := range parsed.Data {
			if parsed.Data[i].Weight == 1 {
				gram1 = &parsed.Data[i]
				break
			}
		}
		if gram1 == nil {
			return toolError("gold_price_error", fmt.Errorf("data harga emas untuk pecahan 1 gram tidak ditemukan di response API"))
		}
		price24k := gram1.SellPrice
		price22k := int64(float64(price24k) * 22 / 24)
		dateLabel := formatIndonesianDate(gram1.RecordedDate)
		out := fmt.Sprintf(
			"Harga Emas Hari Ini (%s)\nSumber: Logam Mulia (Antam resmi) via logammulia.com\n\n24K (Logam Mulia, kadar 99,99%%): Rp %s / gram\n22K (estimasi kadar 91,7%%):       Rp %s / gram\n\nCatatan: harga 22K adalah estimasi rasio kadar dari harga 24K resmi, bukan harga jual resmi Antam (Antam hanya menjual produk 24K).",
			dateLabel, formatRupiahThousands(price24k), formatRupiahThousands(price22k),
		)
		return textResult(out)
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "todo_write", Title: "Todo Write", Description: "Replace the entire todo checklist with the given list of items. Call this with the complete, updated list every time progress changes, mirroring how Claude Code's TodoWrite tracks multi-step work."}, func(_ context.Context, _ *mcp.CallToolRequest, in TodoWriteInput) (*mcp.CallToolResult, any, error) {
		items := make([]TodoItem, 0, len(in.Items))
		for i, it := range in.Items {
			content := strings.TrimSpace(it.Content)
			if content == "" {
				return toolError("todo_write failed", fmt.Errorf("item ke-%d: content tidak boleh kosong", i+1))
			}
			status := it.Status
			if status == "" {
				status = "pending"
			}
			if status != "pending" && status != "in_progress" && status != "completed" {
				return toolError("todo_write failed", fmt.Errorf("item ke-%d: status %q tidak valid, harus pending/in_progress/completed", i+1, it.Status))
			}
			activeForm := it.ActiveForm
			if activeForm == "" {
				activeForm = content
			}
			items = append(items, TodoItem{
				ID:         strconv.Itoa(i + 1),
				Content:    content,
				Status:     status,
				ActiveForm: activeForm,
			})
		}
		st := TodoState{Items: items}
		if err := saveTodoState(st); err != nil {
			return toolError("todo_write failed", err)
		}
		saved, err := loadTodoState()
		if err != nil {
			return toolError("todo_write failed", err)
		}
		return textResult(renderTodoList(saved))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "todo_read", Title: "Todo Read", Description: "Show the current todo checklist without modifying it."}, func(_ context.Context, _ *mcp.CallToolRequest, _ TodoReadInput) (*mcp.CallToolResult, any, error) {
		st, err := loadTodoState()
		if err != nil {
			return toolError("todo_read failed", err)
		}
		return textResult(renderTodoList(st))
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "lint_format", Title: "Lint Format", Description: "Check or auto-fix code formatting for a file or directory. Supports Go via gofmt, Rust via rustfmt, JS/TS/JSON/CSS/Markdown via prettier (only if the project already has it installed), and a Python syntax check (formatting not supported since no Python formatter is installed). Set fix=true to apply changes in place; default only reports what would change."}, func(ctx context.Context, _ *mcp.CallToolRequest, in LintFormatInput) (*mcp.CallToolResult, any, error) {
		root, err := safePath(defaultString(in.Path, "."))
		if err != nil {
			return toolError("lint_format failed", err)
		}
		groups, err := collectLintFiles(root)
		if err != nil {
			return toolError("lint_format failed", err)
		}
		if len(groups) == 0 {
			return textResult(fmt.Sprintf("Tidak ada file yang didukung lint_format (go/rust/js-ts-json-css-md/py) di %s.", root))
		}

		var sb strings.Builder
		if in.Fix {
			sb.WriteString("Mode: FIX (perubahan langsung diterapkan)\n\n")
		} else {
			sb.WriteString("Mode: CHECK (cuma laporan, belum ada file yang diubah)\n\n")
		}

		if files, ok := groups["go"]; ok {
			sb.WriteString("== Go (gofmt) ==\n")
			checkOut, checkErr := runLintTool(ctx, "gofmt", append([]string{"-l"}, files...), root)
			needsFormat := strings.TrimSpace(checkOut) != ""
			if checkErr != nil {
				sb.WriteString("error saat cek format:\n" + checkOut + "\n")
			} else if in.Fix {
				_, applyErr := runLintTool(ctx, "gofmt", append([]string{"-w"}, files...), root)
				if applyErr != nil {
					sb.WriteString("error saat apply format:\n" + checkOut + "\n")
				} else if needsFormat {
					sb.WriteString("File yang diformat ulang:\n" + checkOut)
				} else {
					sb.WriteString("Semua file Go udah rapi, tidak ada perubahan.\n")
				}
			} else if needsFormat {
				sb.WriteString("File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n" + checkOut)
			} else {
				sb.WriteString("Semua file Go udah sesuai format gofmt.\n")
			}
			sb.WriteString("\n")
		}

		if files, ok := groups["rust"]; ok {
			sb.WriteString("== Rust (rustfmt) ==\n")
			checkOut, checkErr := runLintTool(ctx, "rustfmt", append([]string{"--check"}, files...), root)
			needsFormat := checkErr != nil
			if in.Fix {
				_, applyErr := runLintTool(ctx, "rustfmt", files, root)
				if applyErr != nil {
					sb.WriteString("error saat apply format:\n" + checkOut + "\n")
				} else if needsFormat {
					sb.WriteString("File yang diformat ulang:\n" + checkOut)
				} else {
					sb.WriteString("Semua file Rust udah rapi, tidak ada perubahan.\n")
				}
			} else if needsFormat {
				sb.WriteString("File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n" + checkOut)
			} else {
				sb.WriteString("Semua file Rust udah sesuai format rustfmt.\n")
			}
			sb.WriteString("\n")
		}

		if files, ok := groups["js"]; ok {
			sb.WriteString("== JS/TS/JSON/CSS/MD (prettier) ==\n")
			checkOut, checkErr := runLintTool(ctx, "npx", append([]string{"--no-install", "prettier", "--check"}, files...), root)
			lower := strings.ToLower(checkOut)
			notInstalled := checkErr != nil && (strings.Contains(lower, "could not determine executable to run") || strings.Contains(lower, "not found") || strings.Contains(lower, "command not found"))
			needsFormat := checkErr != nil && !notInstalled
			if notInstalled {
				sb.WriteString("Prettier belum terinstall di project ini (npx --no-install gagal). Install dulu: npm install --save-dev prettier\n")
			} else if in.Fix {
				_, applyErr := runLintTool(ctx, "npx", append([]string{"--no-install", "prettier", "--write"}, files...), root)
				if applyErr != nil {
					sb.WriteString("error saat apply format:\n" + checkOut + "\n")
				} else if needsFormat {
					sb.WriteString("File yang diformat ulang:\n" + checkOut)
				} else {
					sb.WriteString("Semua file udah rapi, tidak ada perubahan.\n")
				}
			} else if needsFormat {
				sb.WriteString("File yang butuh diformat (jalankan dengan fix=true untuk beresin):\n" + checkOut)
			} else {
				sb.WriteString("Semua file udah sesuai format prettier.\n")
			}
			sb.WriteString("\n")
		}

		if files, ok := groups["py"]; ok {
			sb.WriteString("== Python (syntax check, formatter belum ada) ==\n")
			var problems []string
			for _, f := range files {
				out, err := runLintTool(ctx, "python3", []string{"-m", "py_compile", f}, root)
				if err != nil {
					problems = append(problems, fmt.Sprintf("%s:\n%s", f, strings.TrimSpace(out)))
				}
			}
			if len(problems) == 0 {
				sb.WriteString(fmt.Sprintf("Semua %d file Python valid secara sintaks.\n", len(files)))
			} else {
				sb.WriteString(fmt.Sprintf("%d file bermasalah:\n%s\n", len(problems), strings.Join(problems, "\n")))
			}
			sb.WriteString("Catatan: belum ada black/ruff terinstall, jadi cuma cek sintaks, bukan auto-format.\n\n")
		}

		return textResult(strings.TrimSpace(sb.String()) + "\n")
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "system_stats", Title: "System Stats", Description: "Show a snapshot of the machine's health: CPU core count and load average, RAM and swap usage, root disk usage, battery status/health, and temperatures when available."}, func(ctx context.Context, _ *mcp.CallToolRequest, _ SystemStatsInput) (*mcp.CallToolResult, any, error) {
		if isDarwin {
			return textResult(darwinSystemStats(ctx))
		}
		var sb strings.Builder

		if out, err := runRawCommand(ctx, "nproc", 5*time.Second); err == nil {
			fmt.Fprintf(&sb, "CPU: %s core\n", out)
		}
		if out, err := runRawCommand(ctx, "uptime -p", 5*time.Second); err == nil {
			fmt.Fprintf(&sb, "Uptime: %s\n", strings.TrimPrefix(out, "up "))
		}
		if out, err := runRawCommand(ctx, "cat /proc/loadavg", 5*time.Second); err == nil {
			if fields := strings.Fields(out); len(fields) >= 3 {
				fmt.Fprintf(&sb, "Load average (1/5/15 menit): %s / %s / %s\n", fields[0], fields[1], fields[2])
			}
		}
		sb.WriteString("\n")

		if out, err := runRawCommand(ctx, "free -h", 5*time.Second); err == nil {
			for _, line := range strings.Split(out, "\n") {
				fields := strings.Fields(line)
				if len(fields) >= 7 && strings.HasPrefix(fields[0], "Mem") {
					fmt.Fprintf(&sb, "RAM: total %s, terpakai %s, tersedia %s\n", fields[1], fields[2], fields[6])
				} else if len(fields) >= 3 && strings.HasPrefix(fields[0], "Swap") {
					fmt.Fprintf(&sb, "Swap: total %s, terpakai %s\n", fields[1], fields[2])
				}
			}
		}
		sb.WriteString("\n")

		if out, err := runRawCommand(ctx, "df -h /", 5*time.Second); err == nil {
			lines := strings.Split(out, "\n")
			if len(lines) >= 2 {
				fields := strings.Fields(lines[len(lines)-1])
				if len(fields) >= 5 {
					fmt.Fprintf(&sb, "Disk (/): total %s, terpakai %s (%s), tersisa %s\n", fields[1], fields[2], fields[4], fields[3])
				}
			}
		}
		sb.WriteString("\n")

		battOut, battErr := runRawCommand(ctx, "upower -i $(upower -e | grep -i BAT) 2>/dev/null", 5*time.Second)
		if battErr == nil && strings.TrimSpace(battOut) != "" {
			getField := func(key string) string {
				re := regexp.MustCompile(`(?m)^\s*` + regexp.QuoteMeta(key) + `:\s+(.+)$`)
				if m := re.FindStringSubmatch(battOut); m != nil {
					return strings.TrimSpace(m[1])
				}
				return ""
			}
			vendor := getField("vendor")
			model := getField("model")
			pct := getField("percentage")
			state := getField("state")
			capacity := getField("capacity")
			fmt.Fprintf(&sb, "Baterai: %s %s, %s, status %s", vendor, model, pct, state)
			if capacity != "" {
				fmt.Fprintf(&sb, ", kesehatan %s dari kapasitas awal", capacity)
			}
			sb.WriteString("\n")
		} else {
			sb.WriteString("Baterai: tidak terdeteksi (mungkin desktop / tanpa baterai)\n")
		}

		if out, err := runRawCommand(ctx, "sensors 2>/dev/null", 5*time.Second); err == nil {
			var temps []string
			for _, line := range strings.Split(out, "\n") {
				if strings.Contains(line, "°C") {
					temps = append(temps, strings.TrimSpace(line))
				}
			}
			if len(temps) > 0 {
				sb.WriteString("\nSuhu:\n")
				for _, t := range temps {
					fmt.Fprintf(&sb, "  %s\n", t)
				}
			}
		}

		return textResult(strings.TrimSpace(sb.String()) + "\n")
	})

	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: "acp_opencode", Title: "ACP OpenCode", Description: "Delegate a coding task to OpenCode as a sub-agent via the Agent Client Protocol (opencode acp). Spawns opencode, sends one prompt, waits for it to finish (auto-approving any permission prompts, since this is meant for autonomous delegation), and returns its final text response."}, func(ctx context.Context, _ *mcp.CallToolRequest, in AcpOpenCodeInput) (*mcp.CallToolResult, any, error) {
		if strings.TrimSpace(in.Prompt) == "" {
			return toolError("acp_opencode failed", fmt.Errorf("prompt tidak boleh kosong"))
		}
		cwd, err := safePath(defaultString(in.Path, "."))
		if err != nil {
			return toolError("acp_opencode failed", err)
		}
		timeout := 300 * time.Second
		if in.TimeoutSeconds > 0 {
			timeout = time.Duration(in.TimeoutSeconds) * time.Second
		}
		if timeout > 30*time.Minute {
			timeout = 30 * time.Minute
		}

		result, err := runACPAgent(ctx, acpAgentSpec{
			Command: "opencode",
			Args:    []string{"acp"},
			Cwd:     cwd,
			Timeout: timeout,
			Prompt:  in.Prompt,
		})
		if err != nil {
			return toolError("acp_opencode failed", err)
		}
		return textResult(result)
	})
}

// formatIndonesianDate turns "2026-08-30" into "30 Agustus 2026". Falls
// back to returning the input unchanged if it doesn't parse.
func formatIndonesianDate(s string) string {
	t, err := time.Parse("2006-01-02", s)
	if err != nil {
		return s
	}
	months := [...]string{"Januari", "Februari", "Maret", "April", "Mei", "Juni", "Juli", "Agustus", "September", "Oktober", "November", "Desember"}
	return fmt.Sprintf("%d %s %d", t.Day(), months[t.Month()-1], t.Year())
}

// formatRupiahThousands formats an integer with "." as the thousands
// separator, Indonesian style (e.g. 2670000 -> "2.670.000").
func formatRupiahThousands(n int64) string {
	neg := n < 0
	if neg {
		n = -n
	}
	s := strconv.FormatInt(n, 10)
	var out []byte
	for i, c := range []byte(s) {
		if i > 0 && (len(s)-i)%3 == 0 {
			out = append(out, '.')
		}
		out = append(out, c)
	}
	if neg {
		return "-" + string(out)
	}
	return string(out)
}

func registerGit(server *mcp.Server, name string, args []string, description string) {
	mcp.AddTool(server, &mcp.Tool{OutputSchema: toolOutputSchema, Name: name, Title: strings.ReplaceAll(name, "_", " "), Description: description}, func(ctx context.Context, _ *mcp.CallToolRequest, in GitInput) (*mcp.CallToolResult, any, error) {
		cwd, e := safePath(defaultString(in.CWD, "."))
		if e != nil {
			return toolError("git_error", e)
		}
		cmd := exec.CommandContext(ctx, "git", args...)
		cmd.Dir = cwd
		out, e := cmd.CombinedOutput()
		if e != nil {
			return textResult("git_error: " + string(out))
		}
		if len(out) == 0 {
			return textResult("(no output)")
		}
		return textResult(string(out))
	})
}

func defaultString(v, d string) string {
	if v == "" {
		return d
	}
	return v
}
func mustJSON(v any) string { b, _ := json.MarshalIndent(v, "", "  "); return string(b) }
func hostname() string      { h, _ := os.Hostname(); return h }

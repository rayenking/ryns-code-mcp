package main

// Platform helpers. Linux uses X11 tools (xdotool, xclip, import, notify-send)
// and /proc-style utilities. On macOS the equivalent built-ins are used:
// screencapture, pbcopy/pbpaste, osascript, sysctl, vm_stat, pmset.

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"runtime"
	"strconv"
	"strings"
	"time"
)

var isDarwin = runtime.GOOS == "darwin"

// isWindows selects the Windows code paths. Core tools (files, shell via
// PowerShell, git, sheets, todos) work there; platform-specific desktop/media
// tools degrade with clear errors (see windowsUnsupported).
var isWindows = runtime.GOOS == "windows"

// shellCommand resolves how to run a shell command line on the current
// platform: a login bash on unix, and an end-user's native shell (PowerShell)
// on Windows, since cmd.exe parsing is too weak for the AI's command strings.
func shellCommand(command string) (name string, args []string) {
	if isWindows {
		return "powershell.exe", []string{"-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", command}
	}
	return "/bin/bash", []string{"-lc", command}
}

// normalizeLF converts Windows-style CRLF line endings to LF so output
// parsing (sheet data, gold price JSON, git output, ...) behaves the same
// when commands run under PowerShell on Windows.
func normalizeLF(s string) string {
	return strings.ReplaceAll(s, "\r\n", "\n")
}

func init() {
	// GUI-launched MCP clients often start with a minimal PATH that lacks
	// Homebrew, so tools such as rg, tesseract and cliclick are not found.
	if isDarwin {
		path := os.Getenv("PATH")
		for _, d := range []string{"/usr/local/bin", "/opt/homebrew/bin"} {
			if !strings.Contains(":"+path+":", ":"+d+":") {
				path = d + ":" + path
			}
		}
		_ = os.Setenv("PATH", path)
	}
}

// runPlain runs a command with a timeout and optional stdin, returning
// combined output. It does not apply the X11 environment.
func runPlain(ctx context.Context, timeout time.Duration, stdin, name string, args ...string) ([]byte, error) {
	cctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(cctx, name, args...)
	cmd.Env = childEnv()
	if stdin != "" {
		cmd.Stdin = strings.NewReader(stdin)
	}
	return cmd.CombinedOutput()
}

// asQuote returns s as an AppleScript string literal.
func asQuote(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return `"` + s + `"`
}

func osascript(ctx context.Context, timeout time.Duration, script string) ([]byte, error) {
	return runPlain(ctx, timeout, "", "osascript", "-e", script)
}

// ---- screenshot / clipboard -------------------------------------------------

// captureScreen writes a PNG of the screen (or one window) to out. On Linux
// window is an X window id or "root"; on macOS it is a CGWindowID (screencapture -l).
func captureScreen(ctx context.Context, window, out string) ([]byte, error) {
	if isDarwin {
		args := []string{"-x"}
		if window != "" && window != "root" {
			args = append(args, "-l", window)
		}
		args = append(args, out)
		return runPlain(ctx, 15*time.Second, "", "screencapture", args...)
	}
	w := "root"
	if window != "" {
		w = window
	}
	return runXTool(ctx, 15*time.Second, "import", "-window", w, out)
}

func clipboardRead(ctx context.Context) ([]byte, error) {
	if isDarwin {
		return runPlain(ctx, 5*time.Second, "", "pbpaste")
	}
	return runXTool(ctx, 5*time.Second, "xclip", "-selection", "clipboard", "-o")
}

func clipboardWrite(ctx context.Context, text string) ([]byte, error) {
	if isDarwin {
		return runPlain(ctx, 5*time.Second, text, "pbcopy")
	}
	// xclip forks a background process that keeps serving the selection and
	// inherits stdout/stderr. With pipes (CombinedOutput) Wait would block until
	// another program takes over the clipboard, so send both to /dev/null and
	// rely on the exit status.
	cctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	cmd := exec.CommandContext(cctx, "xclip", "-selection", "clipboard")
	cmd.Env = xEnv()
	cmd.Stdin = strings.NewReader(text)
	cmd.Stdout, cmd.Stderr = nil, nil
	return nil, cmd.Run()
}

// ---- keyboard / mouse / windows / notifications (macOS) ---------------------
// These need Accessibility permission for the app that launched the server.

func macTypeText(ctx context.Context, text string) ([]byte, error) {
	script := `tell application "System Events" to keystroke ` + asQuote(text)
	return osascript(ctx, 30*time.Second, script)
}

var macKeyCodes = map[string]int{
	"return": 36, "enter": 76, "tab": 48, "space": 49, "escape": 53, "esc": 53,
	"backspace": 51, "delete": 51, "forwarddelete": 117,
	"up": 126, "down": 125, "left": 123, "right": 124,
	"home": 115, "end": 119, "pageup": 116, "page_up": 116, "prior": 116,
	"pagedown": 121, "page_down": 121, "next": 121,
	"f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97,
	"f7": 98, "f8": 100, "f9": 101, "f10": 109, "f11": 103, "f12": 111,
}

// macKeyScript converts xdotool-style key syntax ("ctrl+c", "cmd+shift+t",
// "Return", space-separated sequences) into an AppleScript.
func macKeyScript(keys string) (string, error) {
	combos := strings.Fields(keys)
	if len(combos) == 0 {
		return "", fmt.Errorf("no keys given")
	}
	var sb strings.Builder
	sb.WriteString("tell application \"System Events\"\n")
	for _, combo := range combos {
		parts := strings.Split(combo, "+")
		key := strings.ToLower(parts[len(parts)-1])
		var mods []string
		for _, m := range parts[:len(parts)-1] {
			switch strings.ToLower(m) {
			case "ctrl", "control":
				mods = append(mods, "control down")
			case "alt", "option", "opt":
				mods = append(mods, "option down")
			case "shift":
				mods = append(mods, "shift down")
			case "cmd", "command", "super", "meta", "win":
				mods = append(mods, "command down")
			default:
				return "", fmt.Errorf("unknown modifier %q", m)
			}
		}
		using := ""
		if len(mods) > 0 {
			using = " using {" + strings.Join(mods, ", ") + "}"
		}
		if code, ok := macKeyCodes[key]; ok {
			fmt.Fprintf(&sb, "key code %d%s\n", code, using)
		} else if len([]rune(key)) == 1 {
			fmt.Fprintf(&sb, "keystroke %s%s\n", asQuote(key), using)
		} else {
			return "", fmt.Errorf("unsupported key %q on macOS", combo)
		}
	}
	sb.WriteString("end tell")
	return sb.String(), nil
}

func macPressKeys(ctx context.Context, keys string) error {
	script, err := macKeyScript(keys)
	if err != nil {
		return err
	}
	if out, err := osascript(ctx, 10*time.Second, script); err != nil {
		return fmt.Errorf("%v: %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

// macMouseClick needs cliclick (brew install cliclick); macOS has no built-in CLI for this.
func macMouseClick(ctx context.Context, x, y, button int, double bool) (string, error) {
	if _, err := exec.LookPath("cliclick"); err != nil {
		return "", fmt.Errorf("cliclick tidak ditemukan; install dengan: brew install cliclick")
	}
	verb := "c"
	switch {
	case button == 2:
		return "", fmt.Errorf("middle click tidak didukung di macOS")
	case button == 3:
		verb = "rc"
	case double:
		verb = "dc"
	}
	out, err := runPlain(ctx, 5*time.Second, "", "cliclick", fmt.Sprintf("%s:%d,%d", verb, x, y))
	if err != nil {
		return "", fmt.Errorf("%v: %s", err, strings.TrimSpace(string(out)))
	}
	return fmt.Sprintf("%s at (%d,%d) button %d", verb, x, y, button), nil
}

func macActiveWindow(ctx context.Context) string {
	script := `tell application "System Events"
	set p to first application process whose frontmost is true
	set pn to name of p
	set pid to unix id of p
	set wt to ""
	try
		set wt to name of front window of p
	end try
	return (pid as text) & linefeed & wt & linefeed & pn
end tell`
	out, err := osascript(ctx, 5*time.Second, script)
	if err != nil {
		return fmt.Sprintf("get_active_window_error: %v\n%s", err, strings.TrimSpace(string(out)))
	}
	lines := strings.Split(strings.TrimSuffix(string(out), "\n"), "\n")
	for len(lines) < 3 {
		lines = append(lines, "")
	}
	return fmt.Sprintf("id: %s (pid)\ntitle: %s\nclass: %s", lines[0], lines[1], lines[2])
}

func macListWindows(ctx context.Context) string {
	script := `set out to ""
tell application "System Events"
	repeat with p in (every application process whose background only is false)
		set pn to name of p
		try
			repeat with w in (windows of p)
				set out to out & pn & tab & (name of w) & linefeed
			end repeat
		end try
	end repeat
end tell
return out`
	out, err := osascript(ctx, 10*time.Second, script)
	if err != nil {
		return fmt.Sprintf("list_windows_error: %v\n%s", err, strings.TrimSpace(string(out)))
	}
	if strings.TrimSpace(string(out)) == "" {
		return "(no windows found)"
	}
	return string(out)
}

func macNotify(ctx context.Context, title, body string) ([]byte, error) {
	script := "display notification " + asQuote(body) + " with title " + asQuote(title)
	return osascript(ctx, 5*time.Second, script)
}

// windowsUnsupported reports whether the tool has no Windows implementation,
// returning the tool's error line when it does. Desktop and media tools are
// deliberate X11/Linux pairs with no Windows equivalents wired up yet.
func windowsUnsupported(tool string) (bool, string) {
	if !isWindows {
		return false, ""
	}
	return true, fmt.Sprintf("%s: not supported on windows", tool)
}

// ptyCommand starts command under a real PTY via script(1), whose syntax
// differs between util-linux and BSD.
func ptyCommand(command string) *exec.Cmd {
	var cmd *exec.Cmd
	if isDarwin {
		_, shellArgs := shellCommand(command)
		cmd = exec.Command("script", append([]string{"-q", "/dev/null"}, shellArgs...)...)
	} else {
		cmd = exec.Command("script", "-qefc", command, "/dev/null")
	}
	cmd.Env = childEnv()
	return cmd
}

// ---- system stats (macOS) ---------------------------------------------------

var (
	reVMPage = regexp.MustCompile(`page size of (\d+) bytes`)
	reSwap   = regexp.MustCompile(`total = (\S+)\s+used = (\S+)`)
	reBatt   = regexp.MustCompile(`(\d+)%;\s*([^;]+);(?:\s*(\d+:\d+) remaining)?`)
	reBoot   = regexp.MustCompile(`sec\s*=\s*(\d+)`)
)

func vmStatPages(out, label string) int64 {
	re := regexp.MustCompile(`(?m)^` + regexp.QuoteMeta(label) + `:\s+(\d+)\.`)
	m := re.FindStringSubmatch(out)
	if m == nil {
		return 0
	}
	n, _ := strconv.ParseInt(m[1], 10, 64)
	return n
}

// parseVMStat estimates used and available memory (bytes) from vm_stat output.
func parseVMStat(out string) (used, avail int64) {
	ps := int64(4096)
	if m := reVMPage.FindStringSubmatch(out); m != nil {
		if n, err := strconv.ParseInt(m[1], 10, 64); err == nil && n > 0 {
			ps = n
		}
	}
	used = (vmStatPages(out, "Pages active") + vmStatPages(out, "Pages wired down") + vmStatPages(out, "Pages occupied by compressor")) * ps
	avail = (vmStatPages(out, "Pages free") + vmStatPages(out, "Pages inactive") + vmStatPages(out, "Pages speculative")) * ps
	return
}

func parseBattery(pmset string) (pct, state, remaining string, ok bool) {
	if !strings.Contains(pmset, "InternalBattery") {
		return
	}
	m := reBatt.FindStringSubmatch(pmset)
	if m == nil {
		return
	}
	return m[1] + "%", strings.TrimSpace(m[2]), m[3], true
}

func humanBytes(n int64) string {
	f := float64(n)
	units := []string{"B", "Ki", "Mi", "Gi", "Ti"}
	i := 0
	for f >= 1024 && i < len(units)-1 {
		f /= 1024
		i++
	}
	return fmt.Sprintf("%.1f%s", f, units[i])
}

func formatUptime(d time.Duration) string {
	m := int(d.Minutes())
	days, hours, mins := m/1440, (m%1440)/60, m%60
	switch {
	case days > 0:
		return fmt.Sprintf("%d hari, %d jam, %d menit", days, hours, mins)
	case hours > 0:
		return fmt.Sprintf("%d jam, %d menit", hours, mins)
	}
	return fmt.Sprintf("%d menit", mins)
}

func darwinSystemStats(ctx context.Context) string {
	var sb strings.Builder
	sh := func(cmd string) (string, error) {
		out, err := runRawCommand(ctx, cmd, 8*time.Second)
		return strings.TrimSpace(out), err
	}

	if cores, err := sh("sysctl -n hw.ncpu"); err == nil && cores != "" {
		fmt.Fprintf(&sb, "CPU: %s core", cores)
		if brand, e := sh("sysctl -n machdep.cpu.brand_string"); e == nil && brand != "" {
			fmt.Fprintf(&sb, " (%s)", brand)
		}
		sb.WriteString("\n")
	}
	if bt, err := sh("sysctl -n kern.boottime"); err == nil {
		if m := reBoot.FindStringSubmatch(bt); m != nil {
			if sec, e := strconv.ParseInt(m[1], 10, 64); e == nil {
				fmt.Fprintf(&sb, "Uptime: %s\n", formatUptime(time.Since(time.Unix(sec, 0))))
			}
		}
	}
	if la, err := sh("sysctl -n vm.loadavg"); err == nil {
		f := strings.Fields(strings.Trim(la, "{} "))
		if len(f) >= 3 {
			fmt.Fprintf(&sb, "Load average (1/5/15 menit): %s / %s / %s\n", f[0], f[1], f[2])
		}
	}
	sb.WriteString("\n")

	if memStr, err := sh("sysctl -n hw.memsize"); err == nil {
		if total, e := strconv.ParseInt(memStr, 10, 64); e == nil {
			if vm, e2 := sh("vm_stat"); e2 == nil {
				used, avail := parseVMStat(vm)
				fmt.Fprintf(&sb, "RAM: total %s, terpakai %s, tersedia %s\n", humanBytes(total), humanBytes(used), humanBytes(avail))
			}
		}
	}
	if sw, err := sh("sysctl -n vm.swapusage"); err == nil {
		if m := reSwap.FindStringSubmatch(sw); m != nil {
			fmt.Fprintf(&sb, "Swap: total %s, terpakai %s\n", m[1], m[2])
		}
	}
	sb.WriteString("\n")

	// On modern macOS "/" is a small sealed system volume; user data lives on /System/Volumes/Data.
	diskPath := "/"
	if _, err := os.Stat("/System/Volumes/Data"); err == nil {
		diskPath = "/System/Volumes/Data"
	}
	if out, err := sh("df -h " + diskPath); err == nil {
		lines := strings.Split(out, "\n")
		if len(lines) >= 2 {
			if f := strings.Fields(lines[len(lines)-1]); len(f) >= 5 {
				fmt.Fprintf(&sb, "Disk (%s): total %s, terpakai %s (%s), tersisa %s\n", diskPath, f[1], f[2], f[4], f[3])
			}
		}
	}
	sb.WriteString("\n")

	if out, err := sh("pmset -g batt"); err == nil {
		if pct, state, remaining, ok := parseBattery(out); ok {
			fmt.Fprintf(&sb, "Baterai: %s, status %s", pct, state)
			if remaining != "" && remaining != "0:00" {
				fmt.Fprintf(&sb, ", sisa waktu %s", remaining)
			}
			if prof, e := sh("system_profiler SPPowerDataType"); e == nil {
				if m := regexp.MustCompile(`Maximum Capacity:\s*(\S+)`).FindStringSubmatch(prof); m != nil {
					fmt.Fprintf(&sb, ", kesehatan %s dari kapasitas awal", m[1])
				}
				if m := regexp.MustCompile(`Cycle Count:\s*(\d+)`).FindStringSubmatch(prof); m != nil {
					fmt.Fprintf(&sb, ", %s siklus", m[1])
				}
			}
			sb.WriteString("\n")
		} else {
			sb.WriteString("Baterai: tidak terdeteksi (mungkin desktop / tanpa baterai)\n")
		}
	}

	if _, err := exec.LookPath("osx-cpu-temp"); err == nil {
		if t, e := sh("osx-cpu-temp"); e == nil && t != "" {
			fmt.Fprintf(&sb, "\nSuhu CPU: %s\n", t)
		}
	} else {
		sb.WriteString("\nSuhu: tidak tersedia (macOS butuh sudo/powermetrics atau tool tambahan seperti osx-cpu-temp)\n")
	}
	return strings.TrimSpace(sb.String()) + "\n"
}

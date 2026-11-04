package main

import (
	"strings"
	"testing"
	"time"
)

const sampleVMStat = `Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               10000.
Pages active:                             200000.
Pages inactive:                           150000.
Pages speculative:                        5000.
Pages throttled:                          0.
Pages wired down:                         100000.
Pages occupied by compressor:             50000.
`

func TestParseVMStat(t *testing.T) {
	used, avail := parseVMStat(sampleVMStat)
	if want := int64((200000 + 100000 + 50000) * 16384); used != want {
		t.Errorf("used = %d, want %d", used, want)
	}
	if want := int64((10000 + 150000 + 5000) * 16384); avail != want {
		t.Errorf("avail = %d, want %d", avail, want)
	}
}

func TestParseBattery(t *testing.T) {
	out := "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t85%; discharging; 3:20 remaining present: true\n"
	pct, state, rem, ok := parseBattery(out)
	if !ok || pct != "85%" || state != "discharging" || rem != "3:20" {
		t.Errorf("got %q %q %q %v", pct, state, rem, ok)
	}
	out = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n"
	if pct, state, _, ok := parseBattery(out); !ok || pct != "100%" || state != "charged" {
		t.Errorf("charged: got %q %q %v", pct, state, ok)
	}
	if _, _, _, ok := parseBattery("Now drawing from 'AC Power'\n"); ok {
		t.Error("desktop without battery should not parse")
	}
}

func TestAsQuote(t *testing.T) {
	if got, want := asQuote(`say "hi" \ ok`), `"say \"hi\" \\ ok"`; got != want {
		t.Errorf("got %s want %s", got, want)
	}
}

func TestMacKeyScript(t *testing.T) {
	cases := []struct{ in, want string }{
		{"Return", "key code 36"},
		{"ctrl+c", `keystroke "c" using {control down}`},
		{"cmd+shift+t", `keystroke "t" using {command down, shift down}`},
		{"alt+Tab", "key code 48 using {option down}"},
		{"Page_Up", "key code 116"},
		{"ctrl+c ctrl+v", `keystroke "v" using {control down}`},
	}
	for _, c := range cases {
		got, err := macKeyScript(c.in)
		if err != nil || !strings.Contains(got, c.want) {
			t.Errorf("%q: got %q err %v, want to contain %q", c.in, got, err, c.want)
		}
	}
	for _, bad := range []string{"", "hyper+x", "ctrl+SuperLongKey"} {
		if _, err := macKeyScript(bad); err == nil {
			t.Errorf("%q should fail", bad)
		}
	}
}

func TestFormatAndBytes(t *testing.T) {
	if got := formatUptime(90*time.Hour + 5*time.Minute); got != "3 hari, 18 jam, 5 menit" {
		t.Errorf("uptime: %s", got)
	}
	if got := humanBytes(16 << 30); got != "16.0Gi" {
		t.Errorf("humanBytes: %s", got)
	}
}

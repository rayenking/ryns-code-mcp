//go:build windows

package main

// Windows-only process-signal helpers. Windows has no POSIX signal
// semantics (kill_process uses Process.Kill() there and never consults
// mapSignal), so these are thin placeholders that keep the build green.

import (
	"errors"
	"os"
	"os/exec"
)

// exitSignal returns "" on Windows: the exit code is available via
// ExitCode(), but there is no signal information to report.
func exitSignal(ee *exec.ExitError) string {
	_ = ee
	return ""
}

// mapSignal is unreachable on Windows (kill_process short-circuits to
// Process.Kill() before calling it).
func mapSignal(name string) (os.Signal, error) {
	_ = name
	return nil, errors.New("invalid signal")
}

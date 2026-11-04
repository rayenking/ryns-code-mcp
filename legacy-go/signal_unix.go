//go:build !windows

package main

// Unix-only process-signal helpers. The windows counterparts live in
// signal_windows.go so the syscall.WaitStatus / SIGKILL code below never
// breaks a GOOS=windows build.

import (
	"errors"
	"os"
	"os/exec"
	"syscall"
)

// exitSignal returns the signal name that killed the process, or "" when the
// process exited normally (or the platform does not expose the information).
func exitSignal(ee *exec.ExitError) string {
	if ws, ok := ee.Sys().(syscall.WaitStatus); ok && ws.Signaled() {
		return ws.Signal().String()
	}
	return ""
}

// mapSignal turns a signal name into a killable os.Signal, replicating the
// signal whitelist kill_process has always accepted.
func mapSignal(name string) (os.Signal, error) {
	switch name {
	case "SIGTERM":
		return syscall.SIGTERM, nil
	case "SIGKILL":
		return syscall.SIGKILL, nil
	case "SIGINT":
		return syscall.SIGINT, nil
	default:
		return nil, errors.New("invalid signal")
	}
}

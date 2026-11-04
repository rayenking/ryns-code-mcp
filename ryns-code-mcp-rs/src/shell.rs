//! Shell invocation platform resolution and output-normalization helpers.
//! Mirrors Go `platform.go` `shellCommand` (~29) and `normalizeLF` (~39).

/// Resolves how to run a shell command line on the current platform:
/// a login bash on unix, and an end-user's native shell (PowerShell) on
/// Windows, since cmd.exe parsing is too weak for the AI's command strings.
pub fn shell_command(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        return (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                command.to_string(),
            ],
        );
    }
    (
        "/bin/bash".to_string(),
        vec!["-lc".to_string(), command.to_string()],
    )
}

/// Converts Windows-style CRLF line endings to LF so output parsing behaves
/// the same when commands run under PowerShell on Windows.
pub fn normalize_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_shell_command_is_login_bash() {
        let (name, args) = shell_command("echo hi");
        assert_eq!(name, "/bin/bash");
        assert_eq!(args, vec!["-lc".to_string(), "echo hi".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_command_is_powershell() {
        let (name, args) = shell_command("echo hi");
        assert_eq!(name, "powershell.exe");
        assert_eq!(
            args,
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                "echo hi".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_lf_replaces_crlf() {
        assert_eq!(normalize_lf("a\r\nb\r\n\r\nc\r\n"), "a\nb\n\nc\n");
        assert_eq!(normalize_lf("no crlf"), "no crlf");
        assert_eq!(normalize_lf("lone\n lf"), "lone\n lf");
        assert_eq!(normalize_lf("\r"), "\r"); // bare CR untouched
    }
}

#[cfg(test)]
mod cross_platform {
    use super::shell_command;

    /// Both branches in one table so windows semantics are exercised on every
    /// target (the fn is pure; the runtime dispatch is `cfg!`).
    #[test]
    fn shell_command_matrix() {
        if cfg!(windows) {
            let (name, args) = shell_command("echo hi");
            assert_eq!(name, "powershell.exe");
            assert_eq!(
                args,
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "echo hi"
                ]
            );
        } else {
            let (name, args) = shell_command("echo hi");
            assert_eq!(name, "/bin/bash");
            assert_eq!(args, vec!["-lc", "echo hi"]);
        }
    }
}

//! ryns-code-mcp — Rust port of the Go MCP server.
//!
//! Behavior (current truth, post control-panel §A1):
//! - default: MCP over stdio (stdio never checks auth)
//! - `-port N` (or `-port=N`, and the `N` value form): Streamable HTTP on
//!   `127.0.0.1:N`; bearer auth engages ONLY when `RYNS_AUTH_TOKEN` is set
//!   to a non-empty value (comma-separated list), otherwise no middleware
//!
//! P1 routes `/mcp` only are long superseded: HTTP mode now serves `/mcp`,
//! `/assets/`, `/api/save/`, `/api/safety/*` and the control-panel
//! `/api/config` + `/api/tools*` routes, all behind the (optional) auth
//! layer.

mod auth;
mod mcp;
mod platform;
mod policy;
mod safety;
mod shell;
mod tools;

use std::process::ExitCode;

const SERVER_NAME: &str = "ryns-code-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stdio,
    Http(u16),
}

/// Parses `-port N` / `-port=N` / `--port N` / `--port=N` (Go's flag package
/// accepts both dash forms), mirroring Go's `flag.Int` behavior, and keeps
/// scanning after a matched flag so a LATER bad flag is still reported (Go
/// parses left-to-right):
/// - `-port` without a value → "flag needs an argument: -port"
/// - non-number → "invalid value "abc" for flag -port: parse error"
/// - number above 65535 → "invalid value "70000" for flag -port: value out of range"
/// - any other flag → "flag provided but not defined: -x"
///
/// The LAST occurrence wins (Go: repeated flags overwrite). Port 0 (absent,
/// or any negative value — Go parses those fine and the `port > 0` check
/// selects stdio) means stdio. Loose divergence: Go's strconv base-0 also
/// accepts `0x…` / `0…` / `0b…` forms; here only signed decimal is accepted.
fn parse_port(args: &[String]) -> Result<u16, String> {
    let mut port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-port" || arg == "--port" {
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| "flag needs an argument: -port".to_string())?;
            port = Some(parse_flag_int(value)?);
            i += 1;
            continue;
        }
        let mut assigned = false;
        for prefix in ["-port=", "--port="] {
            if let Some(rest) = arg.strip_prefix(prefix) {
                port = Some(parse_flag_int(rest)?);
                assigned = true;
                break;
            }
        }
        if assigned {
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!(
                "flag provided but not defined: {}",
                arg.split('=').next().unwrap_or(arg)
            ));
        }
        i += 1;
    }
    Ok(port.unwrap_or(0)) // Go: port default 0 → stdio
}

/// Parses one flag value like Go `flag.Int` (reduced to signed decimal):
/// non-numbers are a "parse error"; numbers are fine until they exceed the
/// 65535 port ceiling, which surfaces Go's "value out of range" wording; a
/// negative number is accepted (Go then treats port <= 0 as stdio).
fn parse_flag_int(value: &str) -> Result<u16, String> {
    let bad = |k: &str| format!("invalid value \"{value}\" for flag -port: {k}");
    let (negative, digits) = match value.strip_prefix('-') {
        Some(d) => (true, d),
        None => match value.strip_prefix('+') {
            Some(d) => (false, d),
            None => (false, value),
        },
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("parse error"));
    }
    let magnitude: u64 = digits.parse().map_err(|_| bad("value out of range"))?;
    let n: i128 = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    if n > 65535 {
        return Err(bad("value out of range"));
    }
    Ok(n.clamp(0, 65535) as u16)
}

fn usage_line() -> String {
    format!(
        "Usage: {} [-port N]\n  -port N\n    \tserve Streamable HTTP on this port instead of stdio (0 = stdio)",
        SERVER_NAME
    )
}

fn main() -> ExitCode {
    // Mirrors platform.go's init(): GUI-launched clients may start with a
    // minimal PATH on macOS; prepend Homebrew paths BEFORE the tokio runtime
    // is built — edition-2024 marks `env::set_var` unsafe and mutating the
    // environment while a multithreaded runtime is live would be a race.
    platform::init_path();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = match parse_port(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("{}", usage_line());
            return ExitCode::from(2);
        }
    };
    let mode = if port > 0 {
        Mode::Http(port)
    } else {
        Mode::Stdio
    };

    // Workspace root mirrors Go's `workspace` package var.
    let workspace: std::sync::Arc<str> = platform::workspace_root().into();

    // Phase 3: safety config (all modes audit; fail on malformed env values,
    // mirroring Go's fail-on-bad-env style for required-process settings).
    let safety_config = match safety::config_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let engine = std::sync::Arc::new(safety::Engine::new(&safety_config, &workspace));
    // §A2: first-run creation of .state/tool-policy.json with defaults.
    policy::ensure_defaults(&workspace);

    // Manual runtime creation (not #[tokio::main]) so `init_path()` runs
    // strictly before the async machinery exists (see the comment above).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    runtime.block_on(serve(mode, workspace, engine))
}

/// The async body of `main`, split off so the tokio runtime exists only
/// after the one-time startup environment fixups have completed.
async fn serve(
    mode: Mode,
    workspace: std::sync::Arc<str>,
    engine: std::sync::Arc<safety::Engine>,
) -> ExitCode {
    match mode {
        Mode::Stdio => {
            eprintln!(
                "{SERVER_NAME} v{SERVER_VERSION} ready — {workspace} (safety: {})",
                engine.mode().label()
            );
            if let Err(e) = mcp::serve_stdio(workspace, engine).await {
                eprintln!("server stopped: {e:#}");
            }
            ExitCode::SUCCESS
        }
        Mode::Http(port) => {
            // §A1: auth is OPTIONAL — unset/empty RYNS_AUTH_TOKEN means no
            // middleware (bind stays 127.0.0.1); set means the middleware
            // arms exactly as before for every route.
            let auth_state = auth::AuthState::from_env();
            let auth_enabled = !auth_state.config.tokens.is_empty();
            let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("listen failed on port {port}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!(
                "{SERVER_NAME} v{SERVER_VERSION} ready — {workspace} (http://127.0.0.1:{port}/mcp, safety: {}, auth: {})",
                engine.mode().label(),
                if auth_enabled { "on" } else { "off" }
            );
            if let Err(e) = mcp::serve_http(listener, workspace, engine, auth_state, port).await {
                eprintln!("server stopped: {e:#}");
            }
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn both_dashes_and_equals_forms() {
        assert_eq!(parse_port(&[]).unwrap(), 0);
        assert_eq!(parse_port(&args(&["-port", "1"])).unwrap(), 1);
        assert_eq!(parse_port(&args(&["-port=1"])).unwrap(), 1);
        assert_eq!(parse_port(&args(&["--port", "1"])).unwrap(), 1);
        assert_eq!(parse_port(&args(&["--port=1"])).unwrap(), 1);
        // The last occurrence wins (Go: repeated flags overwrite).
        assert_eq!(parse_port(&args(&["-port=1", "--port=2"])).unwrap(), 2);
        assert_eq!(parse_port(&args(&["-port", "77"])).unwrap(), 77);
    }

    #[test]
    fn go_error_wording() {
        assert_eq!(
            parse_port(&args(&["-port"])).unwrap_err(),
            "flag needs an argument: -port"
        );
        assert_eq!(
            parse_port(&args(&["-port", "abc"])).unwrap_err(),
            "invalid value \"abc\" for flag -port: parse error"
        );
        assert_eq!(
            parse_port(&args(&["--port=70000"])).unwrap_err(),
            "invalid value \"70000\" for flag -port: value out of range"
        );
        assert_eq!(
            parse_port(&args(&["-port", "99999999999999999999"])).unwrap_err(),
            "invalid value \"99999999999999999999\" for flag -port: value out of range"
        );
        assert_eq!(
            parse_port(&args(&["-x"])).unwrap_err(),
            "flag provided but not defined: -x"
        );
        assert_eq!(
            parse_port(&args(&["-x=y"])).unwrap_err(),
            "flag provided but not defined: -x"
        );
    }

    #[test]
    fn scanning_continues_after_a_match() {
        // A later bad flag is still reported (Go parses left-to-right).
        assert_eq!(
            parse_port(&args(&["-port=1", "-x"])).unwrap_err(),
            "flag provided but not defined: -x"
        );
        // A "-x" sitting in the value slot is the flag's value, not a flag:
        // Go tries to parse it as a number → parse error.
        assert_eq!(
            parse_port(&args(&["-port", "-x"])).unwrap_err(),
            "invalid value \"-x\" for flag -port: parse error"
        );
    }

    #[test]
    fn negative_port_is_accepted_like_go() {
        // Go's flag.Int accepts negatives; the port > 0 check selects stdio.
        assert_eq!(parse_port(&args(&["-port", "-1"])).unwrap(), 0);
    }
}

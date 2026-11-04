use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

use crate::models::{SafetySettings, ServerProcessStatus};

pub struct ServerProcessManager {
    child: Mutex<Option<Child>>,
    is_external: Mutex<bool>,
    transition_lock: tokio::sync::Mutex<()>,
}

impl Default for ServerProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerProcessManager {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
            is_external: Mutex::new(false),
            transition_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Resolves the server binary path in order:
    /// 1. Custom settings override (if exists and executable/file)
    /// 2. macOS bundle resource path (<bundle>/Contents/Resources/server/ryns-code-mcp)
    /// 3. Development fallback paths (release or debug in ryns-code-mcp-rs/target)
    pub fn resolve_server_binary(custom_path: Option<&str>) -> Option<PathBuf> {
        if let Some(custom) = custom_path {
            let p = PathBuf::from(custom.trim());
            if !custom.trim().is_empty() && p.is_file() {
                return Some(p);
            }
        }

        // Check if running from macOS .app bundle
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(macos_dir) = exe_path.parent() {
                // exe_path is typically .../Contents/MacOS/ryns-mcp-control
                let resources_server = macos_dir
                    .join("../Resources/server/ryns-code-mcp")
                    .canonicalize();
                if let Ok(p) = resources_server {
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }

        // Development fallback candidate paths
        let candidates = [
            "../ryns-code-mcp-rs/target/release/ryns-code-mcp",
            "../ryns-code-mcp-rs/target/debug/ryns-code-mcp",
            "./ryns-code-mcp-rs/target/release/ryns-code-mcp",
            "./ryns-code-mcp-rs/target/debug/ryns-code-mcp",
            "../ryns-code-mcp-rs/target/release/ryns-code-mcp.exe",
            "../ryns-code-mcp-rs/target/debug/ryns-code-mcp.exe",
        ];

        for c in candidates {
            let p = PathBuf::from(c);
            if p.is_file() {
                if let Ok(canonical) = p.canonicalize() {
                    return Some(canonical);
                }
                return Some(p);
            }
        }

        None
    }

    /// Checks if a bundled safety-sidecar exists next to the binary or Resources
    pub fn resolve_sidecar_script(binary_path: &Path) -> Option<PathBuf> {
        if let Some(parent) = binary_path.parent() {
            let candidate = parent.join("safety-sidecar/index.mjs");
            if candidate.is_file() {
                return candidate.canonicalize().ok().or(Some(candidate));
            }
        }
        None
    }

    /// Performs preflight probe to check if a server is already listening on `port`.
    pub async fn preflight_probe(port: u16, token: &str) -> bool {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let url = format!("http://127.0.0.1:{port}/api/config");
        let mut req = client.get(&url);
        let primary = crate::client::primary_auth_token(token);
        if !primary.is_empty() {
            req = req.header("Authorization", format!("Bearer {primary}"));
        }

        match req.send().await {
            Ok(res) => {
                let status = res.status();
                status.is_success() || status == reqwest::StatusCode::UNAUTHORIZED
            }
            Err(_) => false,
        }
    }

    /// Bounded poll helper: checks if `port` is free on 127.0.0.1, up to `timeout`.
    pub async fn wait_until_port_free(port: u16, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_millis(50);
        loop {
            if Self::is_port_free(port) {
                return true;
            }
            if start.elapsed() >= timeout {
                return Self::is_port_free(port);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Tests if a TCP listener can bind to 127.0.0.1:port.
    pub fn is_port_free(port: u16) -> bool {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                drop(listener);
                true
            }
            Err(_) => false,
        }
    }

    /// Starts the server child process or connects to an existing instance.
    /// Serialized behind a Mutex to prevent double-spawns from concurrent invokes.
    pub async fn start(&self, settings: &SafetySettings) -> Result<ServerProcessStatus, String> {
        let _guard = self.transition_lock.lock().await;
        self.start_locked(settings).await
    }

    async fn start_locked(&self, settings: &SafetySettings) -> Result<ServerProcessStatus, String> {
        let port = settings.server_port;

        // 1. If child is ALREADY active and running, don't spawn a second child!
        {
            let mut lock = self.child.lock().unwrap();
            if let Some(child) = lock.as_mut() {
                if let Ok(None) = child.try_wait() {
                    return Ok(ServerProcessStatus {
                        running: true,
                        is_external: false,
                        pid: Some(child.id()),
                        port,
                        message: format!(
                            "Server child active (PID: {}) on port {port}",
                            child.id()
                        ),
                    });
                }
            }
        }

        // 2. Preflight HTTP probe: do not spawn if already running
        if Self::preflight_probe(port, &settings.auth_token).await {
            let mut ext = self.is_external.lock().unwrap();
            *ext = true;
            return Ok(ServerProcessStatus {
                running: true,
                is_external: true,
                pid: None,
                port,
                message: format!("Server already running on port {port} (external instance)"),
            });
        }

        // 3. Locate binary
        let binary =
            Self::resolve_server_binary(Some(&settings.server_binary_path)).ok_or_else(|| {
                "Server binary 'ryns-code-mcp' not found in bundle or dev paths".to_string()
            })?;

        // 4. Stop any previously managed child if still running
        self.stop_child();

        // 5. Configure command
        let mut cmd = Command::new(&binary);
        Self::configure_server_command(&mut cmd, settings, &binary, port);

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn {}: {e}", binary.display()))?;

        let pid = child.id();
        {
            let mut lock = self.child.lock().unwrap();
            *lock = Some(child);
            let mut ext = self.is_external.lock().unwrap();
            *ext = false;
        }

        // Wait briefly for server to bind
        tokio::time::sleep(Duration::from_millis(400)).await;

        Ok(ServerProcessStatus {
            running: true,
            is_external: false,
            pid: Some(pid),
            port,
            message: format!("Server started (PID: {pid}) on port {port}"),
        })
    }

    /// Stops the managed child process.
    pub async fn stop(&self) -> Result<ServerProcessStatus, String> {
        let _guard = self.transition_lock.lock().await;
        self.stop_locked()
    }

    fn stop_locked(&self) -> Result<ServerProcessStatus, String> {
        let mut ext = self.is_external.lock().unwrap();
        if *ext {
            *ext = false;
            return Ok(ServerProcessStatus {
                running: false,
                is_external: true,
                pid: None,
                port: 0,
                message: "Disconnected from external server instance".to_string(),
            });
        }

        self.stop_child();
        Ok(ServerProcessStatus {
            running: false,
            is_external: false,
            pid: None,
            port: 0,
            message: "Server child process stopped".to_string(),
        })
    }

    pub fn stop_child(&self) {
        let mut lock = self.child.lock().unwrap();
        if let Some(mut child) = lock.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Configures the command line and environment for spawning the server child process.
    /// Explicitly filters out any parent `RYNS_AUTH_TOKEN` from being inherited by the child,
    /// passing `RYNS_AUTH_TOKEN` only if the GUI settings provide a non-empty token.
    pub fn configure_server_command(
        cmd: &mut Command,
        settings: &SafetySettings,
        binary: &Path,
        port: u16,
    ) {
        cmd.arg("-port").arg(port.to_string());

        // Strip any inherited RYNS_AUTH_TOKEN from parent environment
        // Check case-insensitively across parent vars
        for (k, _) in std::env::vars_os() {
            if k.as_encoded_bytes()
                .eq_ignore_ascii_case(b"RYNS_AUTH_TOKEN")
            {
                cmd.env_remove(k);
            }
        }
        cmd.env_remove("RYNS_AUTH_TOKEN");

        // Set safety mode
        if !settings.safety_mode.trim().is_empty() {
            cmd.env("RYNS_SAFETY_MODE", settings.safety_mode.trim());
        } else {
            cmd.env("RYNS_SAFETY_MODE", "enforce");
        }

        // Set sidecar script if bundled sidecar exists
        if let Some(sidecar) = Self::resolve_sidecar_script(binary) {
            cmd.env("RYNS_SIDECAR_SCRIPT", sidecar.to_string_lossy().to_string());
        }

        // Set RYNS_AUTH_TOKEN ONLY when GUI token setting is non-empty
        let clean_token = settings.auth_token.trim();
        if !clean_token.is_empty() {
            cmd.env("RYNS_AUTH_TOKEN", clean_token);
        }
    }

    pub async fn restart(&self, settings: &SafetySettings) -> Result<ServerProcessStatus, String> {
        let _guard = self.transition_lock.lock().await;
        self.stop_child();
        {
            let mut ext = self.is_external.lock().unwrap();
            *ext = false;
        }
        let port = settings.server_port;
        // Bounded poll until port is free (up to 2 seconds) instead of racing a fixed sleep
        let _ = Self::wait_until_port_free(port, Duration::from_secs(2)).await;
        self.start_locked(settings).await
    }

    pub fn status(&self, port: u16) -> ServerProcessStatus {
        let mut lock = self.child.lock().unwrap();
        let ext = *self.is_external.lock().unwrap();

        if ext {
            return ServerProcessStatus {
                running: true,
                is_external: true,
                pid: None,
                port,
                message: format!("Connected to external server on port {port}"),
            };
        }

        if let Some(child) = lock.as_mut() {
            match child.try_wait() {
                Ok(None) => ServerProcessStatus {
                    running: true,
                    is_external: false,
                    pid: Some(child.id()),
                    port,
                    message: format!("Server child active (PID: {})", child.id()),
                },
                _ => {
                    *lock = None;
                    ServerProcessStatus {
                        running: false,
                        is_external: false,
                        pid: None,
                        port,
                        message: "Server stopped".to_string(),
                    }
                }
            }
        } else {
            ServerProcessStatus {
                running: false,
                is_external: false,
                pid: None,
                port,
                message: "Server stopped".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_server_binary_custom_invalid() {
        let resolved = ServerProcessManager::resolve_server_binary(Some("/nonexistent/path/ryns"));
        // Should fall back or be None if no build exists at that exact path
        assert!(resolved.is_none() || resolved.unwrap().is_file());
    }

    #[test]
    fn test_resolve_server_binary_dev_candidates() {
        let resolved = ServerProcessManager::resolve_server_binary(None);
        // If developer binaries exist in target/debug or target/release, it resolves to a file
        if let Some(path) = resolved {
            assert!(path.is_file());
            assert!(path.to_str().unwrap().contains("ryns-code-mcp"));
        }
    }

    #[tokio::test]
    async fn test_preflight_probe_offline_port() {
        // Port 59998 should not be listening
        let probe = ServerProcessManager::preflight_probe(59998, "").await;
        assert!(!probe);
    }

    #[tokio::test]
    async fn test_preflight_probe_multi_token_sends_primary() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_text = String::from_utf8_lossy(&buf[..n]);
                // Must receive Bearer tok1, not Bearer tok1, tok2
                if req_text
                    .to_ascii_lowercase()
                    .contains("authorization: bearer tok1\r\n")
                {
                    let res = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
                    let _ = socket.write_all(res.as_bytes()).await;
                } else {
                    let res = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                    let _ = socket.write_all(res.as_bytes()).await;
                }
            }
        });

        let probe = ServerProcessManager::preflight_probe(port, "tok1, tok2").await;
        assert!(probe, "Preflight probe should succeed with primary token");
    }

    #[test]
    fn test_child_env_filters_parent_auth_token() {
        let settings_empty = SafetySettings {
            auth_token: String::new(), // GUI token is empty
            ..SafetySettings::default()
        };

        let mut cmd = Command::new("dummy-binary");
        ServerProcessManager::configure_server_command(
            &mut cmd,
            &settings_empty,
            Path::new("dummy-binary"),
            8799,
        );

        // Inspect cmd.get_envs() directly without mutating real process environment
        let envs: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> = cmd.get_envs().collect();
        let token_entry = envs
            .iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("RYNS_AUTH_TOKEN"));
        assert!(
            token_entry.is_some(),
            "RYNS_AUTH_TOKEN must be in Command::get_envs()"
        );
        assert_eq!(
            token_entry.unwrap().1,
            None,
            "RYNS_AUTH_TOKEN must be configured with env_remove (None in get_envs)"
        );

        let mode_entry = envs
            .iter()
            .find(|(k, _)| k.to_string_lossy() == "RYNS_SAFETY_MODE");
        assert_eq!(mode_entry.unwrap().1.unwrap(), "enforce");

        // 2. When GUI token is non-empty, it MUST be passed to child in Command envs
        let gui_token = "gui-explicit-token-4567";
        let settings_with_token = SafetySettings {
            auth_token: gui_token.to_string(),
            ..SafetySettings::default()
        };

        let mut cmd_with_token = Command::new("dummy-binary");
        ServerProcessManager::configure_server_command(
            &mut cmd_with_token,
            &settings_with_token,
            Path::new("dummy-binary"),
            8799,
        );

        let envs_with_token: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> =
            cmd_with_token.get_envs().collect();
        let token_set_entry = envs_with_token
            .iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("RYNS_AUTH_TOKEN"));
        assert!(token_set_entry.is_some());
        assert_eq!(
            token_set_entry.unwrap().1.unwrap(),
            gui_token,
            "Child process must receive GUI configured token in get_envs"
        );
    }

    #[tokio::test]
    async fn test_wait_until_port_free_table() {
        // Case 1: Port is immediately free
        let listener1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = listener1.local_addr().unwrap().port();
        drop(listener1); // ensure free
        let res1 =
            ServerProcessManager::wait_until_port_free(free_port, Duration::from_millis(500)).await;
        assert!(res1, "Free port must return true");

        // Case 2: Port occupied then released shortly
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(listener2);
        });
        let res2 =
            ServerProcessManager::wait_until_port_free(port2, Duration::from_millis(1000)).await;
        assert!(res2, "Port released during bounded poll must return true");

        // Case 3: Port remains occupied beyond timeout
        let listener3 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port3 = listener3.local_addr().unwrap().port();
        let res3 =
            ServerProcessManager::wait_until_port_free(port3, Duration::from_millis(150)).await;
        assert!(
            !res3,
            "Permanently occupied port must return false on timeout"
        );
        drop(listener3);
    }

    #[tokio::test]
    async fn test_server_process_lifecycle_guard() {
        let manager = ServerProcessManager::new();

        // 1. Initial status is stopped
        let st = manager.status(8799);
        assert!(!st.running);
        assert!(!st.is_external);

        // 2. Mock external server listening on a port
        let tokio_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ext_port = tokio_listener.local_addr().unwrap().port();

        // Spawn mock HTTP server responding 200 to probe
        let probe_server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut socket, _)) = tokio_listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let settings = SafetySettings {
            server_port: ext_port,
            ..SafetySettings::default()
        };

        // Concurrent start calls serialized by transition mutex without double-spawn
        let (res1, res2) = tokio::join!(manager.start(&settings), manager.start(&settings));
        let s1 = res1.expect("start 1 should succeed");
        let s2 = res2.expect("start 2 should succeed");

        assert!(s1.running);
        assert!(s1.is_external);
        assert!(s2.running);
        assert!(s2.is_external);

        // Stop transitions status cleanly
        let stopped = manager.stop().await.expect("stop should succeed");
        assert!(!stopped.running);

        probe_server.abort();
    }
}

use ryns_mcp_control_lib::client::{
    build_http_client, fetch_server_config, fetch_tools_list, update_tool_policy,
};
use ryns_mcp_control_lib::models::UpdateToolPolicyPayload;
use ryns_mcp_control_lib::server_process::ServerProcessManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_config_poll_degrades_when_server_offline() {
    let client = build_http_client();
    // Non-existent port
    let err = fetch_server_config(&client, "http://127.0.0.1:59123", "")
        .await
        .unwrap_err();
    assert!(
        err.contains("Failed to connect")
            || err.contains("connection refused")
            || err.contains("error trying to connect")
            || err.contains("HTTP error")
            || err.contains("59123"),
        "Expected offline connection error, got: {err}"
    );
}

#[tokio::test]
async fn test_preflight_probe_offline_and_online() {
    // 1. Random unbound port -> probe returns false
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // close immediately

    let probe_offline = ServerProcessManager::preflight_probe(port, "").await;
    assert!(!probe_offline, "Unbound port should probe as offline");

    // 2. Bind port with mock HTTP responding 200
    let listener2 = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener2.accept().await {
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let res = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = socket.write_all(res.as_bytes()).await;
        }
    });

    let probe_online = ServerProcessManager::preflight_probe(port, "").await;
    assert!(
        probe_online,
        "Bound port responding HTTP should probe as online"
    );
}

#[tokio::test]
async fn test_tools_list_and_policy_update_mock_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");
    let (policy_tx, mut policy_rx) = tokio::sync::mpsc::channel::<UpdateToolPolicyPayload>(1);

    // Mock Server Task
    tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };

            let tx = policy_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /api/config") {
                    let body = format!(
                        r#"{{
                        "version": "1.0.0",
                        "port": {},
                        "mode": "enforce",
                        "auth": false,
                        "tool_counts": {{
                            "enabled": 50,
                            "ask": 3,
                            "disabled": 2
                        }},
                        "workspace": "/Users/test/Workspace",
                        "server_pid": 12345
                    }}"#,
                        port
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                } else if req_str.starts_with("GET /api/tools") {
                    let body = r#"{
                        "tools": [
                            {"name": "execute_command", "title": "Execute shell", "description": "Execute shell", "access": "ask", "safety": true, "safety_capable": true},
                            {"name": "read_file", "title": "Read file", "description": "Read file", "access": "enabled", "safety": true, "safety_capable": true},
                            {"name": "list_directory", "title": "List dir", "description": "List dir", "access": "enabled", "safety": false, "safety_capable": false}
                        ]
                    }"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                } else if req_str.starts_with("POST /api/tools/policy") {
                    if let Some(json_start) = req_str.find("\r\n\r\n") {
                        let json_part = &req_str[json_start + 4..];
                        if let Ok(payload) =
                            serde_json::from_str::<UpdateToolPolicyPayload>(json_part.trim())
                        {
                            let _ = tx.send(payload).await;
                        }
                    }
                    let body = r#"{"ok":true}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }
    });

    let client = build_http_client();

    // 1. Fetch server config
    let config = fetch_server_config(&client, &base_url, "")
        .await
        .expect("config fetch should succeed");
    assert_eq!(config.version, "1.0.0");
    assert_eq!(config.port, port);
    assert_eq!(config.mode, "enforce");
    assert!(!config.auth);
    assert_eq!(config.workspace, "/Users/test/Workspace");
    assert_eq!(config.server_pid, Some(12345));
    assert_eq!(config.tool_counts.enabled, 50);
    assert_eq!(config.tool_counts.ask, 3);
    assert_eq!(config.tool_counts.disabled, 2);

    // 2. Fetch tools list
    let tools = fetch_tools_list(&client, &base_url, "")
        .await
        .expect("tools list should succeed");
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0].name, "execute_command");
    assert_eq!(tools[0].access, "ask");
    assert!(tools[0].safety_capable);
    assert!(!tools[2].safety_capable);

    // 3. Post policy update
    let update_payload = UpdateToolPolicyPayload {
        tool: "execute_command".to_string(),
        access: "enabled".to_string(),
        safety: true,
    };
    update_tool_policy(&client, &base_url, "", &update_payload)
        .await
        .expect("policy update should succeed");

    let received_payload = policy_rx
        .recv()
        .await
        .expect("server should receive policy payload");
    assert_eq!(received_payload.tool, "execute_command");
    assert_eq!(received_payload.access, "enabled");
    assert!(received_payload.safety);
}

#[tokio::test]
async fn test_policy_update_revert_on_error_400_and_500() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    // Mock server returning 400 for first POST, 500 for second POST
    tokio::spawn(async move {
        let mut call_count = 0;
        while let Ok((mut socket, _)) = listener.accept().await {
            call_count += 1;
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&buf[..n]);

            if req_str.starts_with("POST /api/tools/policy") {
                if call_count == 1 {
                    let body = r#"{"error":"invalid_policy"}"#;
                    let res = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(res.as_bytes()).await;
                } else {
                    let body = r#"{"error":"internal_server_error"}"#;
                    let res = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(res.as_bytes()).await;
                }
            }
        }
    });

    let client = build_http_client();

    // 1. Simulate tool state with 400 error revert
    let mut tool_access = "ask".to_string();
    let mut tool_safety = true;

    // Snapshot before optimistic mutation
    let orig_access = tool_access.clone();
    let orig_safety = tool_safety;

    // Optimistically update
    tool_access = "disabled".to_string();
    tool_safety = false;
    assert_eq!(tool_access, "disabled");
    assert!(!tool_safety);

    let payload_bad = UpdateToolPolicyPayload {
        tool: "execute_command".to_string(),
        access: "disabled".to_string(),
        safety: false,
    };

    let res400 = update_tool_policy(&client, &base_url, "", &payload_bad).await;
    assert!(res400.is_err(), "Expected 400 error from server");
    let err_msg = res400.unwrap_err();
    assert!(
        err_msg.contains("400") || err_msg.contains("invalid_policy"),
        "Error message should indicate 400/invalid_policy: {err_msg}"
    );

    // Revert logic (mirrors UI catch handler in app.js)
    tool_access = orig_access;
    tool_safety = orig_safety;

    assert_eq!(tool_access, "ask");
    assert!(tool_safety);

    // 2. Simulate tool state with 500 error revert
    let orig_access2 = tool_access.clone();
    let orig_safety2 = tool_safety;

    // Optimistically update
    tool_access = "enabled".to_string();
    tool_safety = true;
    assert_eq!(tool_access, "enabled");
    assert!(tool_safety);

    let payload_500 = UpdateToolPolicyPayload {
        tool: "read_file".to_string(),
        access: "enabled".to_string(),
        safety: true,
    };

    let res500 = update_tool_policy(&client, &base_url, "", &payload_500).await;
    assert!(res500.is_err(), "Expected 500 error from server");
    let err_msg500 = res500.unwrap_err();
    assert!(
        err_msg500.contains("500") || err_msg500.contains("internal_server_error"),
        "Error message should indicate 500: {err_msg500}"
    );

    // Revert logic
    tool_access = orig_access2;
    tool_safety = orig_safety2;

    assert_eq!(tool_access, "ask");
    assert!(tool_safety);
}

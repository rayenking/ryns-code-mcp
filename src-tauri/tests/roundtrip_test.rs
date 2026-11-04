use ryns_mcp_control_lib::client::{build_http_client, fetch_pending, post_decision};
use ryns_mcp_control_lib::models::{AuditRecord, PendingRequest, SafetySettings};
use ryns_mcp_control_lib::sse::parse_sse_block;
use ryns_mcp_control_lib::state::AppState;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_offline_list_pending_returns_empty() {
    let client = build_http_client();
    // Non-existent server
    let result = fetch_pending(&client, "http://127.0.0.1:59999", "dummy").await;
    assert!(result.is_err());

    // When offline, AppState cached list is empty
    let state = AppState::new(None, SafetySettings::default());
    let pending = state.pending.read().await;
    assert_eq!(*pending, Vec::<PendingRequest>::new());
}

#[tokio::test]
async fn test_full_roundtrip_sse_event_to_decision_post() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");
    let token = "test-token-roundtrip";

    let (decide_done_tx, mut decide_done_rx) = tokio::sync::mpsc::channel::<String>(1);

    // Mock server task
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };

            let decide_tx = decide_done_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.starts_with("GET /api/safety/events") {
                    let sse_header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                    let _ = socket.write_all(sse_header.as_bytes()).await;

                    // Send one pending event
                    let event_payload = "event: pending\ndata: {\"id\":\"rt-101\",\"tool\":\"execute_command\",\"command\":\"git push origin main --force\",\"cwd\":\"/Users/test\",\"reason\":\"force push denied\",\"ruleId\":\"git.force\",\"created_at\":1000,\"expires_at\":2000}\n\n";
                    let _ = socket.write_all(event_payload.as_bytes()).await;

                    // Keep socket open briefly
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                } else if req_str.starts_with("POST /api/safety/decide/rt-101")
                    && req_str.contains(r#""decision":"allow""#)
                {
                    let _ = decide_tx.send("allow".to_string()).await;
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
    let state = Arc::new(AppState::new(
        None,
        SafetySettings {
            server_url: base_url.clone(),
            auth_token: token.to_string(),
            ..SafetySettings::default()
        },
    ));

    // Simulate receiving the SSE chunk & parsing
    let sse_raw_block = "event: pending\ndata: {\"id\":\"rt-101\",\"tool\":\"execute_command\",\"command\":\"git push origin main --force\",\"cwd\":\"/Users/test\",\"reason\":\"force push denied\",\"ruleId\":\"git.force\",\"created_at\":1000,\"expires_at\":2000}\n";
    let (event_name, event_data) = parse_sse_block(sse_raw_block).expect("must parse SSE");
    assert_eq!(event_name, "pending");

    let pending_req: PendingRequest =
        serde_json::from_str(&event_data).expect("must parse pending request");
    assert_eq!(pending_req.id, "rt-101");
    assert_eq!(pending_req.command, "git push origin main --force");

    // Add to AppState
    {
        let mut p = state.pending.write().await;
        p.push(pending_req.clone());
    }

    // Verify pending state before decision
    {
        let p = state.pending.read().await;
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "rt-101");
    }

    // Execute decision POST
    let decide_result = post_decision(&client, &base_url, token, "rt-101", "allow").await;
    assert!(
        decide_result.is_ok(),
        "Post decision should succeed: {:?}",
        decide_result
    );

    // Verify server received decision "allow"
    let received_decision = decide_done_rx
        .recv()
        .await
        .expect("mock server should receive decision");
    assert_eq!(received_decision, "allow");

    // Update state upon resolution (as done by Tauri decide command)
    {
        let mut p = state.pending.write().await;
        p.retain(|item| item.id != "rt-101");

        let mut audit = state.audit.write().await;
        audit.push(AuditRecord {
            id: pending_req.id,
            tool: pending_req.tool,
            command: pending_req.command,
            cwd: pending_req.cwd,
            reason: pending_req.reason,
            rule_id: pending_req.rule_id,
            decision: "allow".to_string(),
            decided_at: "1700000000".to_string(),
        });
    }

    // Verify state after decision
    {
        let p = state.pending.read().await;
        assert_eq!(p.len(), 0, "Pending queue must now be empty");

        let audit = state.audit.read().await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].id, "rt-101");
        assert_eq!(audit[0].decision, "allow");
    }
}

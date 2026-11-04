use ryns_mcp_control_lib::client::{
    build_http_client, fetch_pending, post_decision, test_connection,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_mock_safety_api_flow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");
    let test_token = "secret-token-123";

    // Spawn mock server
    tokio::spawn(async move {
        // Handle 3 requests: test_connection, fetch_pending, post_decision
        for _ in 0..3 {
            let (mut socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };

            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req_str = String::from_utf8_lossy(&buf[..n]);

            // Verify auth header
            assert!(
                req_str.contains("authorization: Bearer secret-token-123")
                    || req_str.contains("Authorization: Bearer secret-token-123")
                    || req_str.contains("X-RYNS-Auth: secret-token-123")
                    || req_str.contains("x-ryns-auth: secret-token-123"),
                "Request must carry auth header: {}",
                req_str
            );

            if req_str.starts_with("GET /api/safety/pending") {
                let body = r#"{"pending":[{"id":"mock-req-1","tool":"execute_command","command":"rm -rf /","cwd":"/root","reason":"destructive","ruleId":"custom.rm","created_at":1000,"expires_at":2000}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            } else if req_str.starts_with("POST /api/safety/decide/mock-req-1") {
                assert!(req_str.contains(r#""decision":"allow""#));
                let body = r#"{"ok":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            } else {
                let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await;
            }
        }
    });

    let client = build_http_client();

    // 1. Test connection
    let test_res = test_connection(&client, &base_url, test_token)
        .await
        .expect("test connection failed");
    assert!(test_res.ok);
    assert_eq!(test_res.pending_count, 1);

    // 2. Fetch pending
    let pending = fetch_pending(&client, &base_url, test_token)
        .await
        .expect("fetch pending failed");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "mock-req-1");
    assert_eq!(pending[0].tool, "execute_command");
    assert_eq!(pending[0].command, "rm -rf /");

    // 3. Post decision
    let decide_res = post_decision(&client, &base_url, test_token, "mock-req-1", "allow").await;
    assert!(
        decide_res.is_ok(),
        "Decision submission failed: {:?}",
        decide_res
    );
}

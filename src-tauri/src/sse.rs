use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;

use crate::models::{PendingRequest, SafetyEvent, SafetySettings};

pub fn parse_sse_block(block: &str) -> Option<(String, String)> {
    let mut event = "message".to_string();
    let mut data_lines = Vec::new();
    let mut has_data = false;

    for line in block.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            has_data = true;
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    if has_data {
        Some((event, data_lines.join("\n")))
    } else {
        None
    }
}

fn auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let clean = crate::client::primary_auth_token(token);
    if !clean.is_empty() {
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {clean}")) {
            headers.insert(AUTHORIZATION, val);
        }
        if let Ok(val) = HeaderValue::from_str(clean) {
            headers.insert("X-RYNS-Auth", val);
        }
    }
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers
}

pub struct SseWorker {
    app_handle: AppHandle,
    client: Client,
    settings: Arc<RwLock<SafetySettings>>,
    pending: Arc<RwLock<Vec<PendingRequest>>>,
}

impl SseWorker {
    pub fn new(
        app_handle: AppHandle,
        client: Client,
        settings: Arc<RwLock<SafetySettings>>,
        pending: Arc<RwLock<Vec<PendingRequest>>>,
    ) -> Self {
        Self {
            app_handle,
            client,
            settings,
            pending,
        }
    }

    pub async fn run_loop(self, mut cancel_rx: tokio::sync::watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);

        loop {
            if *cancel_rx.borrow() {
                break;
            }

            let (url, token) = {
                let s = self.settings.read().await;
                (s.server_url.clone(), s.auth_token.clone())
            };

            let base = url.trim().trim_end_matches('/');
            let sse_url = format!("{base}/api/safety/events");

            let _ = self.app_handle.emit(
                "connection-status",
                SafetyEvent::Status {
                    connected: false,
                    message: format!("Connecting to {sse_url}..."),
                },
            );

            match self
                .client
                .get(&sse_url)
                .headers(auth_headers(&token))
                .send()
                .await
            {
                Ok(response) => {
                    if !response.status().is_success() {
                        let status = response.status();
                        let msg = if status == reqwest::StatusCode::UNAUTHORIZED {
                            "401 Unauthorized: Invalid or missing token".to_string()
                        } else {
                            format!("HTTP error {status}")
                        };
                        let _ = self.app_handle.emit(
                            "connection-status",
                            SafetyEvent::Status {
                                connected: false,
                                message: msg,
                            },
                        );
                    } else {
                        // Successfully connected to SSE stream
                        let _ = self.app_handle.emit(
                            "connection-status",
                            SafetyEvent::Status {
                                connected: true,
                                message: format!("Connected to {base}"),
                            },
                        );
                        backoff = Duration::from_secs(1); // Reset backoff on success

                        // Fetch initial pending queue to sync state
                        if let Ok(initial_pending) =
                            crate::client::fetch_pending(&self.client, &url, &token).await
                        {
                            let mut p = self.pending.write().await;
                            *p = initial_pending.clone();
                            for item in initial_pending {
                                let _ = self
                                    .app_handle
                                    .emit("safety-event", SafetyEvent::Pending { request: item });
                            }
                        }

                        let mut stream = response.bytes_stream();
                        let mut buffer = String::new();

                        'stream: loop {
                            tokio::select! {
                                _ = cancel_rx.changed() => {
                                    if *cancel_rx.borrow() {
                                        return;
                                    }
                                }
                                chunk_opt = stream.next() => {
                                    match chunk_opt {
                                        Some(Ok(bytes)) => {
                                            if let Ok(text) = std::str::from_utf8(&bytes) {
                                                buffer.push_str(text);

                                                while let Some(pos) = buffer.find("\n\n") {
                                                    let block = buffer[..pos].to_string();
                                                    buffer = buffer[pos + 2..].to_string();

                                                    if let Some((event, data)) = parse_sse_block(&block) {
                                                        self.handle_event(&event, &data).await;
                                                    }
                                                }
                                            }
                                        }
                                        Some(Err(err)) => {
                                            let _ = self.app_handle.emit(
                                                "connection-status",
                                                SafetyEvent::Status {
                                                    connected: false,
                                                    message: format!("Stream interrupted: {err}"),
                                                },
                                            );
                                            break 'stream;
                                        }
                                        None => {
                                            let _ = self.app_handle.emit(
                                                "connection-status",
                                                SafetyEvent::Status {
                                                    connected: false,
                                                    message: "Stream closed by server".to_string(),
                                                },
                                            );
                                            break 'stream;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = self.app_handle.emit(
                        "connection-status",
                        SafetyEvent::Status {
                            connected: false,
                            message: format!("Connection error: {err}"),
                        },
                    );
                }
            }

            // Sleep with backoff before reconnecting
            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    async fn handle_event(&self, event: &str, data: &str) {
        match event {
            "pending" => {
                if let Ok(req) = serde_json::from_str::<PendingRequest>(data) {
                    {
                        let mut p = self.pending.write().await;
                        if !p.iter().any(|item| item.id == req.id) {
                            p.push(req.clone());
                        }
                    }
                    let _ = self
                        .app_handle
                        .emit("safety-event", SafetyEvent::Pending { request: req });
                }
            }
            "resolved" => {
                #[derive(serde::Deserialize)]
                struct ResolvedData {
                    #[serde(deserialize_with = "crate::models::deserialize_id")]
                    id: String,
                    decision: Option<String>,
                }
                if let Ok(res) = serde_json::from_str::<ResolvedData>(data) {
                    {
                        let mut p = self.pending.write().await;
                        p.retain(|item| item.id != res.id);
                    }
                    let _ = self.app_handle.emit(
                        "safety-event",
                        SafetyEvent::Resolved {
                            id: res.id,
                            decision: res.decision,
                        },
                    );
                }
            }
            _ => {
                // Other events or ignore
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sse_block_pending() {
        let block = "event: pending\ndata: {\"id\":\"1\",\"command\":\"ls\"}\n";
        let parsed = parse_sse_block(block).expect("should parse");
        assert_eq!(parsed.0, "pending");
        assert_eq!(parsed.1, "{\"id\":\"1\",\"command\":\"ls\"}");
    }

    #[test]
    fn test_parse_sse_block_resolved() {
        let block = "event: resolved\ndata: {\"id\":\"1\",\"decision\":\"allow\"}\n";
        let parsed = parse_sse_block(block).expect("should parse");
        assert_eq!(parsed.0, "resolved");
        assert_eq!(parsed.1, "{\"id\":\"1\",\"decision\":\"allow\"}");
    }

    #[test]
    fn test_parse_sse_block_keepalive_ignored() {
        let block = ":keepalive\n";
        let parsed = parse_sse_block(block);
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_sse_multiline_data() {
        let block = "event: pending\ndata: {\"id\":\"1\",\ndata: \"command\":\"echo hi\"}\n";
        let parsed = parse_sse_block(block).expect("should parse");
        assert_eq!(parsed.0, "pending");
        assert_eq!(parsed.1, "{\"id\":\"1\",\n\"command\":\"echo hi\"}");
    }

    #[test]
    fn test_sse_auth_headers() {
        let headers = auth_headers("tokA, tokB");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer tokA"
        );
        assert_eq!(
            headers.get("X-RYNS-Auth").unwrap().to_str().unwrap(),
            "tokA"
        );
        assert_eq!(
            headers
                .get(reqwest::header::ACCEPT)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );

        let headers_empty = auth_headers("   ");
        assert!(headers_empty.get(AUTHORIZATION).is_none());
        assert!(headers_empty.get("X-RYNS-Auth").is_none());
        assert_eq!(
            headers_empty
                .get(reqwest::header::ACCEPT)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
    }
}

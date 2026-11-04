use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::Client;
use std::time::Duration;

use crate::models::{
    ConnectionTestResult, DecidePayload, DecideResponse, PendingListResponse, PendingRequest,
    ServerConfigResponse, ToolInfo, ToolsListResponse, UpdateToolPolicyPayload,
};

pub fn build_http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Extracts the primary auth token from a potentially comma-separated token list.
///
/// Returns the first non-empty comma-separated entry after trimming whitespace,
/// mirroring `config.rs` load semantics.
/// - Single token unchanged: `"tok1"` -> `"tok1"`, `" tok1 "` -> `"tok1"`
/// - Multi-token: `"tok1, tok2"` -> `"tok1"`
/// - Leading empty entries: `",tok2"` -> `"tok2"`, `", , tok3"` -> `"tok3"`
/// - Empty / whitespace-only: `""` -> `""`, `"  "` -> `""`, `",  , "` -> `""`
pub fn primary_auth_token(token: &str) -> &str {
    token
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

fn auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let clean_token = primary_auth_token(token);
    if !clean_token.is_empty() {
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {clean_token}")) {
            headers.insert(AUTHORIZATION, val);
        }
        if let Ok(val) = HeaderValue::from_str(clean_token) {
            headers.insert("X-RYNS-Auth", val);
        }
    }
    headers
}

fn clean_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "http://127.0.0.1:8799".to_string()
    } else {
        trimmed.to_string()
    }
}

pub async fn test_connection(
    client: &Client,
    base_url: &str,
    token: &str,
) -> Result<ConnectionTestResult, String> {
    let base = clean_base_url(base_url);
    let target = format!("{base}/api/safety/pending");
    let req = client.get(&target).headers(auth_headers(token));

    let res = req
        .send()
        .await
        .map_err(|e| format!("Connection failed to {target}: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(ConnectionTestResult {
            ok: false,
            message: "HTTP 401 Unauthorized: Invalid or missing RYNS_AUTH_TOKEN".to_string(),
            pending_count: 0,
        });
    }

    if !status.is_success() {
        let text = res.text().await.unwrap_or_default();
        return Ok(ConnectionTestResult {
            ok: false,
            message: format!("HTTP {status}: {text}"),
            pending_count: 0,
        });
    }

    let parsed: PendingListResponse = res
        .json()
        .await
        .map_err(|e| format!("Failed to parse pending response: {e}"))?;

    Ok(ConnectionTestResult {
        ok: true,
        message: format!(
            "Connected successfully. {} pending request(s).",
            parsed.pending.len()
        ),
        pending_count: parsed.pending.len(),
    })
}

pub async fn fetch_pending(
    client: &Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<PendingRequest>, String> {
    let base = clean_base_url(base_url);
    let target = format!("{base}/api/safety/pending");
    let req = client.get(&target).headers(auth_headers(token));

    let res = req
        .send()
        .await
        .map_err(|e| format!("Request to {target} failed: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("HTTP 401 Unauthorized: token missing or invalid".to_string());
    }
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_text}"));
    }

    let parsed: PendingListResponse = res
        .json()
        .await
        .map_err(|e| format!("Failed to parse response body: {e}"))?;

    Ok(parsed.pending)
}

pub fn percent_encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for b in segment.as_bytes() {
        match *b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*b as char);
            }
            byte => {
                use std::fmt::Write;
                let _ = write!(encoded, "%{:02X}", byte);
            }
        }
    }
    encoded
}

pub async fn post_decision(
    client: &Client,
    base_url: &str,
    token: &str,
    id: &str,
    decision: &str,
) -> Result<(), String> {
    let base = clean_base_url(base_url);
    let encoded_id = percent_encode_path_segment(id);
    let target = format!("{base}/api/safety/decide/{encoded_id}");
    let payload = DecidePayload {
        decision: decision.to_string(),
    };

    let req = client
        .post(&target)
        .headers(auth_headers(token))
        .json(&payload);

    let res = req
        .send()
        .await
        .map_err(|e| format!("Request to {target} failed: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("HTTP 401 Unauthorized: invalid auth token".to_string());
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("Request {id} not found or expired (404)"));
    }
    if status == reqwest::StatusCode::CONFLICT {
        return Err(format!("Request {id} already decided (409)"));
    }
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_text}"));
    }

    let body: DecideResponse = res.json().await.unwrap_or_default();
    if body.ok == Some(true) || body.error.is_none() {
        Ok(())
    } else {
        Err(body
            .error
            .unwrap_or_else(|| "Unknown decision error".into()))
    }
}

pub async fn fetch_server_config(
    client: &Client,
    base_url: &str,
    token: &str,
) -> Result<ServerConfigResponse, String> {
    let base = clean_base_url(base_url);
    let target = format!("{base}/api/config");
    let req = client.get(&target).headers(auth_headers(token));

    let res = req
        .send()
        .await
        .map_err(|e| format!("Server unreachable at {target}: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("HTTP 401 Unauthorized: token missing or invalid".to_string());
    }
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_text}"));
    }

    let parsed: ServerConfigResponse = res
        .json()
        .await
        .map_err(|e| format!("Failed to parse config response: {e}"))?;

    Ok(parsed)
}

pub async fn fetch_tools_list(
    client: &Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<ToolInfo>, String> {
    let base = clean_base_url(base_url);
    let target = format!("{base}/api/tools");
    let req = client.get(&target).headers(auth_headers(token));

    let res = req
        .send()
        .await
        .map_err(|e| format!("Failed to fetch tools from {target}: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("HTTP 401 Unauthorized: token missing or invalid".to_string());
    }
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_text}"));
    }

    let parsed: ToolsListResponse = res
        .json()
        .await
        .map_err(|e| format!("Failed to parse tools list response: {e}"))?;

    Ok(parsed.into_tools())
}

pub async fn update_tool_policy(
    client: &Client,
    base_url: &str,
    token: &str,
    payload: &UpdateToolPolicyPayload,
) -> Result<serde_json::Value, String> {
    let base = clean_base_url(base_url);
    let target = format!("{base}/api/tools/policy");

    let req = client
        .post(&target)
        .headers(auth_headers(token))
        .json(payload);

    let res = req
        .send()
        .await
        .map_err(|e| format!("Request to {target} failed: {e}"))?;

    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("HTTP 401 Unauthorized: token missing or invalid".to_string());
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("Unknown tool '{}' (404)", payload.tool));
    }
    if status == reqwest::StatusCode::BAD_REQUEST {
        let err_body: serde_json::Value = res.json().await.unwrap_or_default();
        let msg = err_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Invalid policy values");
        return Err(format!("HTTP 400 Bad Request: {msg}"));
    }
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {err_text}"));
    }

    let val: serde_json::Value = res
        .json()
        .await
        .map_err(|e| format!("Failed to parse policy response: {e}"))?;

    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode_path_segment() {
        assert_eq!(
            percent_encode_path_segment("req-123_test.1~"),
            "req-123_test.1~"
        );
        assert_eq!(percent_encode_path_segment("req 1/2"), "req%201%2F2");
        assert_eq!(
            percent_encode_path_segment("cmd#hash?query=1"),
            "cmd%23hash%3Fquery%3D1"
        );
        assert_eq!(
            percent_encode_path_segment("tool:exec+now"),
            "tool%3Aexec%2Bnow"
        );
    }

    #[test]
    fn test_primary_auth_token() {
        // Single token unchanged (with and without surrounding whitespace)
        assert_eq!(primary_auth_token("secret_token_123"), "secret_token_123");
        assert_eq!(
            primary_auth_token("  secret_token_123  "),
            "secret_token_123"
        );

        // Multi-token: takes the first entry
        assert_eq!(primary_auth_token("tok1, tok2"), "tok1");
        assert_eq!(primary_auth_token("tok1,tok2,tok3"), "tok1");

        // Leading empty comma entries: skips empty tokens to find first non-empty
        assert_eq!(primary_auth_token(",tok2"), "tok2");
        assert_eq!(primary_auth_token(", , tok3"), "tok3");

        // Empty semantics: empty or whitespace-only inputs return empty string
        // (which prevents sending Authorization/X-RYNS-Auth headers)
        assert_eq!(primary_auth_token(""), "");
        assert_eq!(primary_auth_token("  "), "");
        assert_eq!(primary_auth_token(" \t \n "), "");
        assert_eq!(primary_auth_token(","), "");
        assert_eq!(primary_auth_token(" ,  , "), "");
    }

    #[test]
    fn test_auth_headers_single_and_multi_token() {
        // Multi-token should use first token for Bearer and X-RYNS-Auth headers
        let headers = auth_headers("tok1, tok2");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer tok1"
        );
        assert_eq!(
            headers.get("X-RYNS-Auth").unwrap().to_str().unwrap(),
            "tok1"
        );

        // Leading comma in multi-token
        let headers_leading_comma = auth_headers(",tok2");
        assert_eq!(
            headers_leading_comma
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok2"
        );

        // Empty/whitespace produces no auth headers
        let headers_empty = auth_headers("   ");
        assert!(headers_empty.get(AUTHORIZATION).is_none());
        assert!(headers_empty.get("X-RYNS-Auth").is_none());
    }
}

//! Bearer-token extraction, child-environment filtering, and the auth
//! middleware. Mirrors Go `main.go` `parseAuthTokens` (line ~118),
//! `authMiddleware` (~135), `childEnv`/`filterAuthEnv` (~89-111).
//!
//! Semantics that must not drift:
//! - `RYNS_AUTH_TOKEN`: comma-separated, entries trimmed, empties dropped.
//! - Candidates come from BOTH `Authorization: Bearer <token>` (scheme
//!   compared case-insensitively) and `X-RYNS-Auth: <token>`; a bad Bearer
//!   value never cancels a valid `X-RYNS-Auth` value.
//! - The comparison runs constant-time over EVERY candidate × EVERY token,
//!   with no early exit on a match.
//! - Fail-closed: zero tokens, zero candidates, or zero matches ⇒ 401.
//! - Children never inherit `RYNS_AUTH_TOKEN` (exact key, case-insensitive).

use std::ffi::OsStr;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

/// Pure version of Go's `parseAuthTokens`.
pub fn parse_auth_tokens(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Entry-form filter mirroring Go `filterAuthEnv` exactly: split each
/// `KEY=value` entry on the first `=`, drop entries whose key equals
/// `RYNS_AUTH_TOKEN` case-insensitively (bare keys and `KEY=` entries are
/// dropped too), keep everything else — including compound keys like
/// `RYNS_AUTH_TOKEN_FOO`. Pure so the matrix can be tested independently of
/// the live process environment. In-crate it is exercised through the test
// matrix below only; `child_env_filter` is the pair-form used at runtime.
#[allow(dead_code)]
pub fn filter_auth_env<'a, I>(env: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    env.into_iter()
        .filter(|kv| {
            let bytes = kv.as_bytes();
            let key = match bytes.iter().position(|b| *b == b'=') {
                Some(i) => &bytes[..i],
                None => bytes,
            };
            !key.eq_ignore_ascii_case(b"RYNS_AUTH_TOKEN")
        })
        .map(str::to_string)
        .collect()
}

/// Builds the environment for child processes from an explicit list of
/// `(key, value)` pairs, dropping `RYNS_AUTH_TOKEN` (exact key,
/// case-insensitive) — the pair-based counterpart of `child_env`, injectable
/// so tests never touch the real environment.
pub fn child_env_filter<I, K, V>(vars: I) -> Vec<(K, V)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
{
    vars.into_iter()
        .filter(|(k, _)| {
            !k.as_ref()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b"RYNS_AUTH_TOKEN")
        })
        .collect()
}

/// Mirrors Go `childEnv()`: the process environment minus
/// `RYNS_AUTH_TOKEN`. Callers MUST set this explicitly on every spawned
/// child (tokio commands inherit the parent environment implicitly, exactly
/// like Go's `exec.Cmd` with a nil Env).
pub fn child_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    child_env_filter(std::env::vars_os())
}

/// Pure decision core of Go `authMiddleware` (everything except HTTP).
#[derive(Clone, Debug, Default)]
pub struct AuthConfig {
    pub tokens: Vec<String>,
}

impl AuthConfig {
    pub fn from_raw(raw: &str) -> Self {
        Self {
            tokens: parse_auth_tokens(raw),
        }
    }

    pub fn from_env() -> Self {
        Self::from_raw(&std::env::var("RYNS_AUTH_TOKEN").unwrap_or_default())
    }

    /// Constant-time check over every candidate × every token, no early
    /// exit; fails closed when there are no tokens, no candidates, or no
    /// match.
    pub fn authorize(&self, authorization: Option<&str>, x_ryns_auth: Option<&str>) -> bool {
        let presented = candidates(authorization, x_ryns_auth);
        let mut matches = 0usize;
        for p in &presented {
            for tok in &self.tokens {
                if tok.as_bytes().ct_eq(p.as_bytes()).into() {
                    matches += 1;
                }
            }
        }
        !self.tokens.is_empty() && !presented.is_empty() && matches > 0
    }
}

/// Candidate extraction mirroring Go exactly:
/// - `Authorization` counts only when it splits into exactly two parts on
///   the first space, the scheme compares equal-fold to "Bearer", and the
///   remainder (trimmed) is non-empty.
/// - `X-RYNS-Auth` counts when its trimmed value is non-empty.
fn candidates<'h>(authorization: Option<&'h str>, x_ryns_auth: Option<&'h str>) -> Vec<&'h str> {
    let mut presented = Vec::new();
    if let Some(h) = authorization {
        let mut parts = h.splitn(2, ' ');
        let scheme = parts.next().unwrap_or("");
        if let Some(rest) = parts
            .next()
            .filter(|_| scheme.eq_ignore_ascii_case("Bearer"))
        {
            let v = rest.trim();
            if !v.is_empty() {
                presented.push(v);
            }
        }
    }
    if let Some(v) = x_ryns_auth {
        let v = v.trim();
        if !v.is_empty() {
            presented.push(v);
        }
    }
    presented
}

/// Shared HTTP-mode auth state for axum's from_fn_with_state middleware.
#[derive(Clone)]
pub struct AuthState {
    pub config: std::sync::Arc<AuthConfig>,
}

impl AuthState {
    pub fn from_env() -> Self {
        Self {
            config: std::sync::Arc::new(AuthConfig::from_env()),
        }
    }
}

/// The single access gate for every HTTP route, mirroring Go
/// `authMiddleware(buildMux(...))`: the layer wraps the whole router.
pub async fn auth_middleware(State(state): State<AuthState>, req: Request, next: Next) -> Response {
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let x_ryns_auth = req
        .headers()
        .get("X-RYNS-Auth")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if !state
        .config
        .authorize(authorization.as_deref(), x_ryns_auth.as_deref())
    {
        return unauthorized();
    }
    next.run(req).await
}

/// 401 with body `{"error":"unauthorized"}` and `WWW-Authenticate: Bearer`,
/// byte-identical to Go's rejection response.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer")),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
        ],
        "{\"error\":\"unauthorized\"}",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorize(tokens: &[&str], authorization: Option<&str>, x: Option<&str>) -> bool {
        AuthConfig {
            tokens: tokens.iter().map(|s| s.to_string()).collect(),
        }
        .authorize(authorization, x)
    }

    #[test]
    fn parse_auth_tokens_matrix() {
        assert_eq!(parse_auth_tokens(""), Vec::<String>::new());
        assert_eq!(parse_auth_tokens("a"), vec!["a"]);
        assert_eq!(parse_auth_tokens("a,b"), vec!["a", "b"]);
        assert_eq!(parse_auth_tokens(" a , b,,c "), vec!["a", "b", "c"]);
        assert_eq!(parse_auth_tokens(",,,"), Vec::<String>::new());
        assert_eq!(parse_auth_tokens("a,"), vec!["a"]);
    }

    // Direct port of Go TestFilterAuthEnv (6 cases).
    #[test]
    fn filter_auth_env_matrix() {
        let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
            (
                "exact key with value dropped",
                vec!["HOME=/x", "RYNS_AUTH_TOKEN=secret", "OTHER=y"],
                vec!["HOME=/x", "OTHER=y"],
            ),
            ("bare key dropped", vec!["RYNS_AUTH_TOKEN"], vec![]),
            (
                "empty value KEY= dropped",
                vec!["RYNS_AUTH_TOKEN=", "OTHER=y"],
                vec!["OTHER=y"],
            ),
            (
                "lowercase variant dropped (Windows is case-insensitive)",
                vec!["ryns_auth_token=secret", "OTHER=y"],
                vec!["OTHER=y"],
            ),
            (
                "compound key RYNS_AUTH_TOKEN_FOO kept",
                vec!["RYNS_AUTH_TOKEN_FOO=x"],
                vec!["RYNS_AUTH_TOKEN_FOO=x"],
            ),
            (
                "other RYNS_* keys kept",
                vec!["RYNS_WORKSPACE_ROOT=/w"],
                vec!["RYNS_WORKSPACE_ROOT=/w"],
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(filter_auth_env(input.iter().copied()), want, "case: {name}");
        }
    }

    #[test]
    fn child_env_filter_matrix() {
        let pairs = vec![
            ("HOME".to_string(), "/x".to_string()),
            ("ryns_auth_token".to_string(), "s".to_string()),
            ("Ryns_Auth_Token".to_string(), String::new()),
            ("RYNS_AUTH_TOKEN_FOO".to_string(), "x".to_string()),
        ];
        let got = child_env_filter(pairs);
        let keys: Vec<String> = got.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec!["HOME", "RYNS_AUTH_TOKEN_FOO"]);
    }

    #[test]
    fn authorize_matrix() {
        // No tokens configured → fail closed.
        assert!(!authorize(&[], Some("Bearer tok"), None));
        // No candidates → fail closed.
        assert!(!authorize(&["tok"], None, None));
        // Wrong bearer.
        assert!(!authorize(&["tok"], Some("Bearer wrong"), None));
        // Wrong X-RYNS-Auth.
        assert!(!authorize(&["tok"], None, Some("wrong")));
        // Exact match via Bearer.
        assert!(authorize(&["tok"], Some("Bearer tok"), None));
        // Case-insensitive scheme.
        assert!(authorize(&["tok"], Some("bearer tok"), None));
        assert!(authorize(&["tok"], Some("BEARER tok"), None));
        // Match via X-RYNS-Auth fallback.
        assert!(authorize(&["tok"], None, Some("tok")));
        // Both headers, one bad one good — good one must win.
        assert!(authorize(&["tok"], Some("Bearer nope"), Some("tok")));
        // Extra whitespace trimmed.
        assert!(authorize(&["tok"], Some("Bearer  tok "), None));
        // "Bearer" alone has no second part → no candidate → fail closed.
        assert!(!authorize(&["tok"], Some("Bearer"), None));
        // Bare value in Authorization (no scheme) → no candidate.
        assert!(!authorize(&["tok"], Some("tok"), None));
        // Second token configured also matches.
        assert!(authorize(&["tok", "tok2"], Some("Bearer tok2"), None));
        // Non-empty value after trim is required.
        assert!(!authorize(&["tok"], Some("Bearer    "), None));
    }

    #[test]
    fn parse_keeps_exact_token_bytes() {
        let cfg = AuthConfig::from_raw("ab, cd ,,e f");
        assert_eq!(cfg.tokens, vec!["ab", "cd", "e f"]);
    }
}

#[cfg(test)]
mod middleware_matrix {
    use super::*;

    fn cfg(tokens: &[&str]) -> AuthConfig {
        AuthConfig::from_raw(tokens.join(",").as_str())
    }

    /// Go authMiddleware matrix: candidates from both headers validate
    /// together; a bad Bearer does not cancel a valid X-RYNS-Auth.
    #[test]
    fn both_headers_validate_together() {
        let c = cfg(&["t1", "t2"]);
        // valid via each channel alone
        assert!(c.authorize(Some("Bearer t1"), None));
        assert!(c.authorize(None, Some("t2")));
        // bad Bearer + good X-RYNS-Auth → allowed; inverse → allowed
        assert!(c.authorize(Some("Bearer nope"), Some("t1")));
        assert!(c.authorize(Some("Bearer t1"), Some("nope")));
        // both bad → rejected
        assert!(!c.authorize(Some("Bearer nope"), Some("alsono")));
    }

    #[test]
    fn scheme_case_insensitive_and_trimmed() {
        let c = cfg(&["t1"]);
        assert!(c.authorize(Some("bearer  t1 "), None));
        // Go splits candidates on ' ' only — a tab after the scheme is still
        // part of the scheme token, so "BEARER\tt1" never matches Bearer.
        assert!(!c.authorize(Some("BEARER\tt1"), None));
    }

    #[test]
    fn fail_closed_without_tokens_or_candidates() {
        let c = cfg(&[]);
        assert!(!c.authorize(Some("Bearer t1"), Some("t1")));
        let c = cfg(&["t1"]);
        assert!(!c.authorize(None, None));
        assert!(!c.authorize(Some("Bearer "), Some("   ")));
    }
}

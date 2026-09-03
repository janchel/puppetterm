//! Optional HTTP basic auth over the whole app (static files, API, WS).
//!
//! Enabled by setting `PUPPETTERM_BASIC_AUTH=user:pass`. Browsers cache the
//! credentials after the first 401 challenge and replay them on subsequent
//! requests AND WebSocket handshakes, so no frontend changes are needed.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

#[derive(Clone)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

/// Read `PUPPETTERM_BASIC_AUTH` ("user:pass") if configured.
pub fn configured() -> Option<Credentials> {
    let raw = std::env::var("PUPPETTERM_BASIC_AUTH").ok()?;
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let (user, pass) = raw.split_once(':')?;
    Some(Credentials { user: user.to_string(), pass: pass.to_string() })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"puppetterm\", charset=\"UTF-8\"")],
        "authentication required",
    )
        .into_response()
}

/// Length-independent comparison (no early exit on content mismatch).
fn matches(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub async fn require_basic_auth(
    State(creds): State<Credentials>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // The OAuth callback is hit by the provider's redirect (no auth header),
    // and only completes a CSRF-protected login — exempt it from basic auth.
    if req.uri().path() == "/oauth/callback" {
        return Ok(next.run(req).await);
    }
    let expected = format!("{}:{}", creds.user, creds.pass);
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| B64.decode(b.trim()).ok())
        .map(|decoded| matches(&decoded, expected.as_bytes()))
        .unwrap_or(false);

    if ok {
        Ok(next.run(req).await)
    } else {
        Err(unauthorized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        std::env::set_var("PUPPETTERM_BASIC_AUTH", "admin:s3cret");
        let c = configured().unwrap();
        assert_eq!(c.user, "admin");
        assert_eq!(c.pass, "s3cret");

        std::env::set_var("PUPPETTERM_BASIC_AUTH", "");
        assert!(configured().is_none());
        std::env::remove_var("PUPPETTERM_BASIC_AUTH");
        assert!(configured().is_none());

        // no colon → not valid basic auth config
        std::env::set_var("PUPPETTERM_BASIC_AUTH", "justatoken");
        assert!(configured().is_none());
        std::env::remove_var("PUPPETTERM_BASIC_AUTH");
    }

    #[test]
    fn constant_time_match() {
        assert!(matches(b"abc", b"abc"));
        assert!(!matches(b"abc", b"abd"));
        assert!(!matches(b"abc", b"abcd"));
        assert!(matches(b"", b""));
    }
}

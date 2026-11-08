//! Bearer-token auth for the REST server.
//!
//! The REST server used to bind `0.0.0.0` with no auth at all, so anyone who
//! could reach the port could read every memory and delete facts and users.
//! It now binds `127.0.0.1` by default and, like the Python Engram server:
//!
//! * with `ENGRAM_AUTH_TOKEN` set, every route but `/health` needs
//!   `Authorization: Bearer <token>`;
//! * without one, it starts only on a loopback address, or with an explicit
//!   `--allow-unauthenticated` (`ENGRAM_ALLOW_UNAUTHENTICATED=1`);
//! * off loopback, a token shorter than 16 characters, or one that looks
//!   like a placeholder, is refused.
//!
//! The token is one shared secret: it is not bound to an org, so whoever
//! holds it can act in every org. Run one server per tenant to separate them.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// The shortest token accepted off loopback.
pub const MIN_REMOTE_TOKEN_LEN: usize = 16;

/// What the REST server enforces, decided at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestAuth {
    /// Every route but `/health` needs this bearer token.
    Token(String),
    /// No auth: loopback only, or explicitly allowed.
    Open { warning: String },
}

/// Decide [`RestAuth`] for a server on `host`, or refuse to start.
pub fn rest_auth(
    host: &str,
    token: Option<&str>,
    allow_unauthenticated: bool,
) -> Result<RestAuth, String> {
    let loopback = is_loopback_literal(host);
    match token.map(str::trim) {
        Some(t) if !t.is_empty() => {
            if !t.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                return Err("ENGRAM_AUTH_TOKEN must be printable ASCII with no spaces".into());
            }
            if !loopback {
                if t.len() < MIN_REMOTE_TOKEN_LEN {
                    return Err(format!(
                        "ENGRAM_AUTH_TOKEN is {} characters; off loopback it must be at \
                         least {MIN_REMOTE_TOKEN_LEN} (32 or more is better)",
                        t.len()
                    ));
                }
                if looks_like_placeholder(t) {
                    return Err("ENGRAM_AUTH_TOKEN looks like a placeholder; generate one \
                         (for example `openssl rand -hex 32`)"
                        .into());
                }
            }
            Ok(RestAuth::Token(t.to_string()))
        }
        _ if loopback => Ok(RestAuth::Open {
            warning: format!(
                "no ENGRAM_AUTH_TOKEN: the REST API on {host} has no auth. Only \
                 processes on this machine can reach it."
            ),
        }),
        _ if allow_unauthenticated => Ok(RestAuth::Open {
            warning: format!(
                "no ENGRAM_AUTH_TOKEN and --allow-unauthenticated: the REST API on {host} \
                 has NO AUTH. Anyone who can reach it can read and delete every memory."
            ),
        }),
        _ => Err(format!(
            "refusing to serve the REST API on {host} without auth. Set ENGRAM_AUTH_TOKEN, \
             bind 127.0.0.1 (ENGRAM_HOST), or pass --allow-unauthenticated \
             (ENGRAM_ALLOW_UNAUTHENTICATED=1) if exposure is really what you want"
        )),
    }
}

/// A loopback IP literal. The name `localhost` is not trusted: it can
/// resolve to something else.
pub fn is_loopback_literal(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn looks_like_placeholder(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    let first = t.as_bytes()[0];
    [
        "changeme",
        "change-me",
        "placeholder",
        "example",
        "your-token",
        "yourtoken",
        "secret",
        "password",
        "token",
    ]
    .iter()
    .any(|p| t.contains(p))
        || t.bytes().all(|b| b == first)
}

/// Constant-time equality on the bytes, so the comparison does not leak how
/// much of a guess was right. The length is not secret.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Require `Authorization: Bearer <token>` on every route but `/health`.
pub async fn require_bearer(token: Arc<String>, req: Request<Body>, next: Next) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| rest.trim().to_string())
        });
    match presented {
        Some(p) if same(p.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"engram\"")],
            axum::Json(serde_json::json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "3f9a1c0e7b2d4a6f8e1c3b5d7f9a2c4e";

    #[test]
    fn a_token_is_required_off_loopback() {
        assert!(rest_auth("0.0.0.0", None, false).is_err());
        assert!(rest_auth("192.168.1.5", Some(""), false).is_err());
        assert!(matches!(
            rest_auth("0.0.0.0", None, true),
            Ok(RestAuth::Open { .. })
        ));
        assert!(matches!(
            rest_auth("127.0.0.1", None, false),
            Ok(RestAuth::Open { .. })
        ));
        assert!(matches!(
            rest_auth("::1", None, false),
            Ok(RestAuth::Open { .. })
        ));
        // The name is not trusted.
        assert!(rest_auth("localhost", None, false).is_err());
    }

    #[test]
    fn a_weak_token_is_refused_off_loopback() {
        for weak in [
            "short",
            "changeme-changeme-changeme",
            "aaaaaaaaaaaaaaaaaaaa",
            "has space in it 1234",
        ] {
            assert!(rest_auth("0.0.0.0", Some(weak), false).is_err(), "{weak}");
        }
        assert_eq!(
            rest_auth("0.0.0.0", Some(GOOD), false),
            Ok(RestAuth::Token(GOOD.into()))
        );
        // On loopback a short token is the operator's call.
        assert_eq!(
            rest_auth("127.0.0.1", Some("short"), false),
            Ok(RestAuth::Token("short".into()))
        );
    }

    #[test]
    fn equality_is_exact() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }
}

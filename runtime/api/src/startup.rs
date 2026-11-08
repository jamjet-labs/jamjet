//! Startup checks for `jamjet-server`: boolean environment flags, the dev-mode
//! bind check, and where the database is.
//!
//! Kept out of `main.rs` so each decision is a plain function a test can call.

use crate::config::ApiConfig;
use std::net::SocketAddr;

/// Lets dev mode (no authentication) listen on an address other than
/// loopback. Read only when dev mode is on.
pub const DEV_MODE_ALLOW_REMOTE_ENV: &str = "JAMJET_DEV_MODE_ALLOW_REMOTE";

/// Parse a boolean environment value. `None` (unset) is `false`.
///
/// `1`, `true`, `yes`, `on` are true; `0`, `false`, `no`, `off` and the empty
/// string are false, ignoring case and surrounding space. Anything else is an
/// error naming the variable, so a typo fails startup instead of being read
/// as one or the other. `JAMJET_EMBED_WORKERS` used to be checked for
/// presence only, so `JAMJET_EMBED_WORKERS=0` turned the pool on.
pub fn parse_flag(name: &str, raw: Option<&str>) -> Result<bool, String> {
    let Some(raw) = raw else { return Ok(false) };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(format!(
            "{name}={other:?} is not a boolean; use 1/true/yes/on or 0/false/no/off"
        )),
    }
}

/// [`parse_flag`] on the process environment.
pub fn env_flag(name: &str) -> anyhow::Result<bool> {
    let raw = match std::env::var(name) {
        Ok(v) => Some(v),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => anyhow::bail!("failed to read {name}: {e}"),
    };
    parse_flag(name, raw.as_deref()).map_err(|e| anyhow::anyhow!(e))
}

/// What dev mode may do on the addresses the server is about to bind.
#[derive(Debug, PartialEq, Eq)]
pub enum DevBind {
    /// Not dev mode, or dev mode on loopback only.
    Ok,
    /// Dev mode reachable from other machines, allowed by
    /// [`DEV_MODE_ALLOW_REMOTE_ENV`]. The caller must warn.
    RemoteAllowed,
}

/// Refuse dev mode on any address but loopback.
///
/// Dev mode turns authentication off, so on `0.0.0.0` (or a LAN address)
/// anyone who can reach the port can register workflows, start runs and
/// approve them. `addrs` are every address the bind resolves to; all of them
/// must be loopback. An empty list is refused too: nothing proves it is local.
pub fn check_dev_mode_bind(
    dev_mode: bool,
    addrs: &[SocketAddr],
    allow_remote: bool,
) -> Result<DevBind, String> {
    if !dev_mode {
        return Ok(DevBind::Ok);
    }
    let local = !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback());
    if local {
        return Ok(DevBind::Ok);
    }
    if allow_remote {
        return Ok(DevBind::RemoteAllowed);
    }
    let shown: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
    Err(format!(
        "refusing to start: JAMJET_DEV_MODE turns authentication off, and the bind \
         address ({}) is not loopback, so anyone who can reach it gets full access. \
         Bind 127.0.0.1 (JAMJET_BIND), turn dev mode off and use a token \
         (`jamjet-server token create`), or set {DEV_MODE_ALLOW_REMOTE_ENV}=1 if \
         that exposure is really what you want",
        if shown.is_empty() {
            "unresolved".to_string()
        } else {
            shown.join(", ")
        }
    ))
}

/// Whether an HTTP `Host` (or authority) names the local machine:
/// `localhost` or a loopback address, with or without a port.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:7700`
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => inner,
            _ => return false,
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            Some(_) => return false,
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// In dev mode on loopback, answer only requests addressed to the local
/// machine. Binding to 127.0.0.1 keeps other machines out, but not a web
/// page: a page whose host name re-resolves to 127.0.0.1 (DNS rebinding) is
/// then same-origin with the runtime, and dev mode has no token to stop its
/// scripts registering workflows or approving runs. Such a request still
/// carries the page's host name, so it is refused here.
pub async fn require_loopback_host(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()));
    if host.as_deref().is_some_and(is_loopback_host) {
        return next.run(req).await;
    }
    (
        axum::http::StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": format!(
                "dev mode answers only requests addressed to localhost or a loopback \
                 address; set {DEV_MODE_ALLOW_REMOTE_ENV}=1 to accept other host names"
            )
        })),
    )
        .into_response()
}

/// The SQLite database URL: `DATABASE_URL`, then the config, then
/// `.jamjet/runtime.db` in the working directory (created if missing).
pub fn database_url(config: &ApiConfig) -> String {
    std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| config.database_url.clone())
        .unwrap_or_else(|| {
            let dir = std::path::PathBuf::from(".jamjet");
            std::fs::create_dir_all(&dir).ok();
            format!("sqlite://{}", dir.join("runtime.db").display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_false_turn_a_flag_off() {
        for raw in ["0", "false", "FALSE", "no", "off", "", "  0 "] {
            assert_eq!(parse_flag("X", Some(raw)), Ok(false), "{raw:?}");
        }
        assert_eq!(parse_flag("X", None), Ok(false));
    }

    #[test]
    fn one_and_true_turn_a_flag_on() {
        for raw in ["1", "true", "True", "yes", "on"] {
            assert_eq!(parse_flag("X", Some(raw)), Ok(true), "{raw:?}");
        }
    }

    #[test]
    fn anything_else_is_an_error_naming_the_variable() {
        let err = parse_flag("JAMJET_EMBED_WORKERS", Some("enabled")).unwrap_err();
        assert!(err.contains("JAMJET_EMBED_WORKERS"), "{err}");
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn dev_mode_on_loopback_starts() {
        for a in ["127.0.0.1:7700", "[::1]:7700", "127.0.0.2:7700"] {
            assert_eq!(
                check_dev_mode_bind(true, &[addr(a)], false),
                Ok(DevBind::Ok),
                "{a}"
            );
        }
    }

    #[test]
    fn dev_mode_on_a_remote_bind_is_refused() {
        for a in ["0.0.0.0:7700", "[::]:7700", "192.168.1.10:7700"] {
            let err = check_dev_mode_bind(true, &[addr(a)], false).unwrap_err();
            assert!(err.contains(DEV_MODE_ALLOW_REMOTE_ENV), "{err}");
        }
    }

    #[test]
    fn one_remote_address_among_loopback_ones_is_refused() {
        let addrs = [addr("127.0.0.1:7700"), addr("10.0.0.5:7700")];
        assert!(check_dev_mode_bind(true, &addrs, false).is_err());
    }

    #[test]
    fn an_unresolved_bind_is_refused_in_dev_mode() {
        assert!(check_dev_mode_bind(true, &[], false).is_err());
    }

    #[test]
    fn the_opt_in_allows_a_remote_bind() {
        assert_eq!(
            check_dev_mode_bind(true, &[addr("0.0.0.0:7700")], true),
            Ok(DevBind::RemoteAllowed)
        );
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for h in [
            "localhost",
            "LOCALHOST:7700",
            "127.0.0.1",
            "127.0.0.1:7700",
            "[::1]",
            "[::1]:7700",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "evil.example",
            "evil.example:7700",
            "localhost.evil.example",
            "192.168.1.2:7700",
            "0.0.0.0:7700",
            "[::]:7700",
            "127.0.0.1:x",
            "[::1]x",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[tokio::test]
    async fn dev_mode_refuses_a_request_for_another_host_name() {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route("/health", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(require_loopback_host));
        let status = |host: &'static str| {
            let app = app.clone();
            async move {
                let mut req = axum::http::Request::get("/health");
                if !host.is_empty() {
                    req = req.header("host", host);
                }
                app.oneshot(req.body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(status("localhost:7700").await, 200);
        assert_eq!(status("127.0.0.1:7700").await, 200);
        assert_eq!(status("rebound.evil.example:7700").await, 403);
        assert_eq!(status("").await, 403, "no host at all is refused");
    }

    #[test]
    fn outside_dev_mode_any_bind_starts() {
        assert_eq!(
            check_dev_mode_bind(false, &[addr("0.0.0.0:7700")], false),
            Ok(DevBind::Ok)
        );
    }
}

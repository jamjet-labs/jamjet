//! Startup checks for `jamjet-server`: boolean environment flags, the dev-mode
//! bind check, the dev-mode request guard, and where the database is.
//!
//! Kept out of `main.rs` so each decision is a plain function a test can call.

use crate::config::ApiConfig;
use std::net::SocketAddr;

/// Lets dev mode (no authentication) listen on an address other than
/// loopback. Read only when dev mode is on.
pub const DEV_MODE_ALLOW_REMOTE_ENV: &str = "JAMJET_DEV_MODE_ALLOW_REMOTE";

/// Host names dev mode answers besides `localhost` and loopback addresses,
/// comma-separated: `host.docker.internal`, a WSL or LAN name. Read only when
/// dev mode is on.
pub const DEV_ALLOWED_HOSTS_ENV: &str = "JAMJET_DEV_ALLOWED_HOSTS";

/// Starts the in-process worker pool outside dev mode.
pub const EMBED_WORKERS_ENV: &str = "JAMJET_EMBED_WORKERS";

/// Starts the in-process cron scheduler outside dev mode.
pub const EMBED_CRON_ENV: &str = "JAMJET_EMBED_CRON";

/// Every boolean switch `jamjet-server` reads. [`check_flags`] refuses to
/// start on a value of any of them that is not a boolean.
pub const BOOL_FLAGS: &[&str] = &[
    crate::config::DEV_MODE_ENV,
    DEV_MODE_ALLOW_REMOTE_ENV,
    EMBED_WORKERS_ENV,
    EMBED_CRON_ENV,
    crate::approvals::ALLOW_SELF_APPROVAL_ENV,
    crate::auth::TENANT_TOKENS_AS_DEFAULT_ENV,
    jamjet_audit::signer::ALLOW_INSECURE_KEY_ENV,
];

/// Parse a boolean environment value ([`jamjet_core::flag::parse_flag`], the
/// one rule every switch follows). `JAMJET_EMBED_WORKERS` used to be checked
/// for presence only, so `JAMJET_EMBED_WORKERS=0` turned the pool on.
pub fn parse_flag(name: &str, raw: Option<&str>) -> Result<bool, String> {
    jamjet_core::flag::parse_flag(name, raw)
}

/// [`parse_flag`] on the process environment.
pub fn env_flag(name: &str) -> anyhow::Result<bool> {
    jamjet_core::flag::env_flag(name).map_err(|e| anyhow::anyhow!(e))
}

/// Refuse to start if any of [`BOOL_FLAGS`] holds a value that is not a
/// boolean, naming it. Code deep in the engine reads the switches again and
/// takes an unreadable one as off; this makes sure it never sees one.
/// `JAMJET_DEV_MODE`, `JAMJET_ALLOW_SELF_APPROVAL` and
/// `JAMJET_TENANT_TOKENS_AS_DEFAULT` used to accept only `1` or `true` and
/// read `yes` as off.
pub fn check_flags(get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    for name in BOOL_FLAGS {
        parse_flag(name, get(name).as_deref())?;
    }
    Ok(())
}

/// [`check_flags`] on the process environment.
pub fn check_env_flags() -> anyhow::Result<()> {
    for name in BOOL_FLAGS {
        env_flag(name)?;
    }
    Ok(())
}

/// Parse [`DEV_ALLOWED_HOSTS_ENV`]: comma-separated host names or addresses,
/// without a scheme, path or port. Empty entries are skipped; anything that
/// is not a plain name is an error, so a URL pasted in does not silently
/// allow nothing.
pub fn parse_allowed_hosts(raw: Option<&str>) -> Result<Vec<String>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut hosts = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim_matches(|c: char| c.is_ascii_whitespace());
        if entry.is_empty() {
            continue;
        }
        let plain = entry
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
        // `[` and `]` only around a whole IPv6 address: `[fd00::1]:7700` is an
        // address and a port, and used to pass as one name.
        let bracketed = entry.starts_with('[') || entry.ends_with(']');
        let well_bracketed = entry.starts_with('[')
            && entry.ends_with(']')
            && entry.matches('[').count() == 1
            && entry.matches(']').count() == 1;
        let brackets_ok = if bracketed {
            well_bracketed
        } else {
            !entry.contains(['[', ']'])
        };
        let has_port = !bracketed && entry.matches(':').count() == 1;
        if !plain || !brackets_ok || has_port || entry.contains("..") {
            return Err(format!(
                "{DEV_ALLOWED_HOSTS_ENV}: {entry:?} is not a host name; give names or \
                 addresses separated by commas, without a scheme, path or port"
            ));
        }
        hosts.push(
            entry
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase(),
        );
    }
    Ok(hosts)
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

/// The name in an HTTP `Host` (or authority), without its port: `[::1]:7700`
/// gives `::1`. `None` if it is malformed.
fn host_name(host: &str) -> Option<&str> {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:7700`
        return match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => Some(inner),
            _ => None,
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => Some(name),
        Some(_) => None,
        None => Some(host),
    }
}

/// Whether an HTTP `Host` (or authority) names the local machine:
/// `localhost` or a loopback address, with or without a port.
pub fn is_loopback_host(host: &str) -> bool {
    host_name(host).is_some_and(|name| {
        name.eq_ignore_ascii_case("localhost")
            || name
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false)
    })
}

/// Whether dev mode answers a request for `host`: the local machine, or a
/// name in `allowed` ([`DEV_ALLOWED_HOSTS_ENV`]).
pub fn is_allowed_host(host: &str, allowed: &[String]) -> bool {
    is_loopback_host(host)
        || host_name(host).is_some_and(|name| {
            !name.is_empty() && allowed.iter().any(|a| a.eq_ignore_ascii_case(name))
        })
}

/// Headers a proxy or tunnel adds. Dev mode refuses a request carrying any
/// of them, or any other `X-Forwarded-*`: ngrok (`--host-header=rewrite`)
/// and cloudflared rewrite `Host` to `localhost`, so the Host check passes
/// for a request from anywhere.
pub const FORWARDING_HEADERS: &[&str] = &["forwarded", "x-forwarded-host", "x-forwarded-for"];

/// Whether a request carries a header a proxy adds ([`FORWARDING_HEADERS`],
/// or any `X-Forwarded-*`).
pub fn is_forwarded(headers: &axum::http::HeaderMap) -> bool {
    headers.keys().any(|k| {
        let k = k.as_str();
        FORWARDING_HEADERS.contains(&k) || k.starts_with("x-forwarded-")
    })
}

/// Why dev mode refuses a request, or `None` to answer it.
///
/// Dev mode has no token to stop a caller, so it answers only a request
/// addressed to this machine (or a name in `allowed`), not relayed by a
/// proxy or tunnel, and not sent by a page of another site:
///
/// * `Host` must be allowed. A page whose host name re-resolves to 127.0.0.1
///   (DNS rebinding) is same-origin with the runtime, but its requests carry
///   its own host name.
/// * No [`FORWARDING_HEADERS`], nor any other `X-Forwarded-*`.
/// * Not from a page of another site. With an `Origin`, the origin decides:
///   it must be an allowed host (`null` is not). Without one,
///   `Sec-Fetch-Site: cross-site` is refused. A form or `fetch` from any page
///   can POST to 127.0.0.1, and a POST with no body (cancel a run,
///   deactivate an agent) needs nothing the browser would stop. A browser
///   sends `Sec-Fetch-Site: cross-site` beside a loopback `Origin` too
///   (`localhost:3000` calling `127.0.0.1:7700` is another site), so the
///   header alone used to refuse a local dev UI.
///
/// What this trusts: any page served from this machine, on any port, and
/// any request that reaches the port without a proxy header. A plain TCP
/// tunnel (`ssh -L`, `socat`) adds no header and keeps `Host` as the client
/// sent it, so a request through one passes. Don't expose the port that
/// way; use a token instead.
pub fn dev_refusal(
    headers: &axum::http::HeaderMap,
    authority: Option<&str>,
    allowed: &[String],
) -> Option<&'static str> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let host = header("host").or(authority);
    if !host.is_some_and(|h| is_allowed_host(h, allowed)) {
        return Some("dev mode answers only requests addressed to localhost, a loopback address or a name in JAMJET_DEV_ALLOWED_HOSTS");
    }
    if is_forwarded(headers) {
        return Some("dev mode refuses requests relayed by a proxy or tunnel (Forwarded, X-Forwarded-Host, X-Forwarded-For)");
    }
    let from_another_site = match headers.get("origin") {
        Some(o) => {
            let origin = o.to_str().unwrap_or("");
            let authority = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
            !is_allowed_host(authority, allowed)
        }
        None => header("sec-fetch-site").is_some_and(|v| v.eq_ignore_ascii_case("cross-site")),
    };
    if from_another_site {
        return Some("dev mode refuses requests from a page of another site");
    }
    None
}

/// The dev-mode request guard ([`dev_refusal`]), on every dev-mode router,
/// whatever the bind. [`DEV_MODE_ALLOW_REMOTE_ENV`] allows a remote bind; it
/// does not turn this off. Inside Docker the bind is `0.0.0.0` and dev mode
/// has no token, so this is all that stops a rebound page.
pub async fn require_local_request(
    axum::extract::State(allowed): axum::extract::State<std::sync::Arc<Vec<String>>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let authority = req.uri().authority().map(|a| a.to_string());
    match dev_refusal(req.headers(), authority.as_deref(), &allowed) {
        None => next.run(req).await,
        Some(reason) => (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
    }
}

/// Put [`require_local_request`] on a dev-mode router.
pub fn guard_dev_router(router: axum::Router, allowed: Vec<String>) -> axum::Router {
    router.layer(axum::middleware::from_fn_with_state(
        std::sync::Arc::new(allowed),
        require_local_request,
    ))
}

/// The SQLite database URL: `DATABASE_URL`, then the config, then
/// `.jamjet/runtime.db` in the working directory (created if missing).
pub fn database_url(config: &ApiConfig) -> String {
    let url = existing_database_url(config);
    if std::env::var("DATABASE_URL").is_err() && config.database_url.is_none() {
        std::fs::create_dir_all(".jamjet").ok();
    }
    url
}

/// [`database_url`] without creating `.jamjet`: for the token commands,
/// which open a database the server already made and must not leave a
/// directory behind when there is none.
pub fn existing_database_url(config: &ApiConfig) -> String {
    std::env::var("DATABASE_URL")
        .ok()
        .or_else(|| config.database_url.clone())
        .unwrap_or_else(|| {
            format!(
                "sqlite://{}",
                std::path::Path::new(".jamjet").join("runtime.db").display()
            )
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

    /// A dev-mode router guarded with `allowed`, answering `method path`
    /// with `headers`.
    async fn guarded(
        allowed: &[&str],
        method: &str,
        headers: &[(&str, &str)],
    ) -> axum::http::StatusCode {
        use tower::ServiceExt;
        let app = guard_dev_router(
            axum::Router::new()
                .route("/health", axum::routing::get(|| async { "ok" }))
                .route(
                    "/executions/x/cancel",
                    axum::routing::post(|| async { "ok" }),
                ),
            allowed.iter().map(|h| h.to_string()).collect(),
        );
        let path = if method == "POST" {
            "/executions/x/cancel"
        } else {
            "/health"
        };
        let mut req = axum::http::Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app.oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn dev_mode_refuses_a_request_for_another_host_name() {
        assert_eq!(
            guarded(&[], "GET", &[("host", "localhost:7700")]).await,
            200
        );
        assert_eq!(
            guarded(&[], "GET", &[("host", "127.0.0.1:7700")]).await,
            200
        );
        assert_eq!(guarded(&[], "GET", &[("host", "[::1]:7700")]).await, 200);
        assert_eq!(
            guarded(&[], "GET", &[("host", "rebound.evil.example:7700")]).await,
            403
        );
        assert_eq!(guarded(&[], "GET", &[]).await, 403, "no host at all");
    }

    #[tokio::test]
    async fn a_listed_host_is_answered_and_no_other() {
        let allowed = ["host.docker.internal"];
        assert_eq!(
            guarded(&allowed, "GET", &[("host", "host.docker.internal:7700")]).await,
            200
        );
        assert_eq!(
            guarded(&allowed, "GET", &[("host", "HOST.DOCKER.INTERNAL")]).await,
            200
        );
        for host in [
            "rebound.evil.example",
            "host.docker.internal.evil.example",
            "evil-host.docker.internal",
        ] {
            assert_eq!(
                guarded(&allowed, "GET", &[("host", host)]).await,
                403,
                "{host}"
            );
        }
    }

    /// ngrok `--host-header=rewrite` and cloudflared send `Host: localhost`
    /// for a request from anywhere; the forwarding headers give them away.
    #[tokio::test]
    async fn a_request_through_a_tunnel_is_refused() {
        for (k, v) in [
            ("x-forwarded-host", "abc.ngrok.app"),
            ("x-forwarded-for", "203.0.113.9"),
            ("forwarded", "for=203.0.113.9;host=abc.ngrok.app"),
            ("x-forwarded-proto", "https"),
            ("X-Forwarded-Port", "443"),
        ] {
            assert_eq!(
                guarded(&[], "GET", &[("host", "localhost:7700"), (k, v)]).await,
                403,
                "{k}"
            );
        }
    }

    /// A page of any site can POST to 127.0.0.1, and a POST with no body
    /// needs nothing the browser would stop.
    #[tokio::test]
    async fn a_cross_site_request_is_refused() {
        let local = ("host", "127.0.0.1:7700");
        for extra in [
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://evil.example"),
            ("origin", "null"),
            ("origin", "http://localhost.evil.example"),
        ] {
            assert_eq!(
                guarded(&[], "POST", &[local, extra]).await,
                403,
                "{extra:?}"
            );
        }
        // As a browser sends them: the Origin and Sec-Fetch-Site together.
        let cross = ("sec-fetch-site", "cross-site");
        for origin in [
            "https://evil.example",
            "null",
            "http://localhost.evil.example",
        ] {
            assert_eq!(
                guarded(&[], "POST", &[local, ("origin", origin), cross]).await,
                403,
                "{origin}"
            );
        }
        // A local dev UI on another port is another site to the browser.
        for origin in [
            "http://localhost:3000",
            "http://127.0.0.1:5173",
            "http://[::1]:8080",
        ] {
            assert_eq!(
                guarded(&[], "POST", &[local, ("origin", origin), cross]).await,
                200,
                "{origin}"
            );
        }
        assert_eq!(
            guarded(
                &["my-pc.local"],
                "POST",
                &[local, ("origin", "http://my-pc.local:3000"), cross]
            )
            .await,
            200
        );
        // A local page (a dev UI on another port) and a client that sends
        // neither header are answered.
        for extra in [
            ("origin", "http://localhost:3000"),
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-site", "same-origin"),
            ("sec-fetch-site", "none"),
        ] {
            assert_eq!(
                guarded(&[], "POST", &[local, extra]).await,
                200,
                "{extra:?}"
            );
        }
        assert_eq!(guarded(&[], "POST", &[local]).await, 200);
    }

    #[test]
    fn allowed_hosts_are_plain_names() {
        assert_eq!(
            parse_allowed_hosts(Some(" host.docker.internal, ,My-PC.local,[fd00::1]")),
            Ok(vec![
                "host.docker.internal".to_string(),
                "my-pc.local".to_string(),
                "fd00::1".to_string()
            ])
        );
        assert_eq!(parse_allowed_hosts(None), Ok(vec![]));
        for bad in [
            "http://host.docker.internal",
            "host.docker.internal:7700",
            "host/path",
            "*.example",
            "a b",
            "a..b",
            "[fd00::1]:7700",
            "[fd00::1",
            "fd00::1]",
            "[[fd00::1]]",
            "a[b]",
        ] {
            let err = parse_allowed_hosts(Some(bad)).unwrap_err();
            assert!(err.contains(DEV_ALLOWED_HOSTS_ENV), "{bad}: {err}");
        }
    }

    /// Every switch: a value that is not a boolean stops startup, naming it.
    /// `JAMJET_DEV_MODE`, `JAMJET_ALLOW_SELF_APPROVAL` and
    /// `JAMJET_TENANT_TOKENS_AS_DEFAULT` read `yes` as off before.
    #[test]
    fn every_switch_refuses_a_value_that_is_not_a_boolean() {
        assert!(check_flags(|_| None).is_ok());
        assert!(check_flags(|_| Some("yes".into())).is_ok());
        for name in BOOL_FLAGS {
            let err = check_flags(|n| (n == *name).then(|| "enabled".to_string())).unwrap_err();
            assert!(err.contains(name), "{name}: {err}");
        }
        for name in [
            "JAMJET_DEV_MODE",
            "JAMJET_DEV_MODE_ALLOW_REMOTE",
            "JAMJET_EMBED_WORKERS",
            "JAMJET_EMBED_CRON",
            "JAMJET_ALLOW_SELF_APPROVAL",
            "JAMJET_TENANT_TOKENS_AS_DEFAULT",
            "JAMJET_AUDIT_ALLOW_INSECURE_KEY",
        ] {
            assert!(
                BOOL_FLAGS.contains(&name),
                "{name} is not checked at startup"
            );
        }
    }

    #[test]
    fn outside_dev_mode_any_bind_starts() {
        assert_eq!(
            check_dev_mode_bind(false, &[addr("0.0.0.0:7700")], false),
            Ok(DevBind::Ok)
        );
    }
}

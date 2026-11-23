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

/// Set by Knative-style serverless container platforms to the service name.
pub const PLATFORM_SERVICE_ENV: &str = "K_SERVICE";

/// Lets the server keep a SQLite file on a serverless platform, where the disk
/// is lost when an instance stops. Read only when such a platform is detected.
pub const ALLOW_EPHEMERAL_STORE_ENV: &str = "JAMJET_ALLOW_EPHEMERAL_STORE";

/// Every boolean switch `jamjet-server` reads. [`check_flags`] refuses to
/// start on a value of any of them that is not a boolean.
pub const BOOL_FLAGS: &[&str] = &[
    crate::config::DEV_MODE_ENV,
    DEV_MODE_ALLOW_REMOTE_ENV,
    EMBED_WORKERS_ENV,
    EMBED_CRON_ENV,
    ALLOW_EPHEMERAL_STORE_ENV,
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

/// The store a server or a command runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Memory,
    Sqlite,
    Postgres,
}

impl StoreKind {
    /// The name the startup log gives it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

/// Whether `url` names a Postgres database: it starts `postgres://` or
/// `postgresql://`, in any case.
pub fn is_postgres_url(url: &str) -> bool {
    let url = url.trim_start().as_bytes();
    [b"postgres://".as_slice(), b"postgresql://".as_slice()]
        .iter()
        .any(|scheme| url.len() >= scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme))
}

/// The store `STORAGE_BACKEND` and the database URL select:
/// `STORAGE_BACKEND=memory` wins, then a Postgres URL, then SQLite.
pub fn store_kind(storage_backend: Option<&str>, database_url: &str) -> StoreKind {
    if storage_backend == Some("memory") {
        StoreKind::Memory
    } else if is_postgres_url(database_url) {
        StoreKind::Postgres
    } else {
        StoreKind::Sqlite
    }
}

/// `K_SERVICE` present and not blank.
pub fn on_serverless_platform(get: impl Fn(&str) -> Option<String>) -> bool {
    get(PLATFORM_SERVICE_ENV).is_some_and(|v| !v.trim().is_empty())
}

/// What to do about the store the server is about to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreCheck {
    Ok,
    Warn(String),
    Refuse(String),
}

/// On a serverless platform a SQLite file is refused unless
/// [`ALLOW_EPHEMERAL_STORE_ENV`] is on (then a warning), and the memory store
/// warns. Postgres is fine. Off a platform nothing changes.
pub fn check_store_for_platform(
    platform: bool,
    kind: StoreKind,
    allow_ephemeral: bool,
) -> StoreCheck {
    if !platform {
        return StoreCheck::Ok;
    }
    match kind {
        StoreKind::Postgres => StoreCheck::Ok,
        StoreKind::Memory => StoreCheck::Warn(
            "STORAGE_BACKEND=memory on a serverless platform: runs and approvals are lost \
             when an instance stops. Point DATABASE_URL at a managed Postgres."
                .to_string(),
        ),
        StoreKind::Sqlite if allow_ephemeral => StoreCheck::Warn(format!(
            "{ALLOW_EPHEMERAL_STORE_ENV} is set: the SQLite file on this serverless platform \
             is lost when an instance stops."
        )),
        StoreKind::Sqlite => StoreCheck::Refuse(format!(
            "refusing to keep a SQLite file on a serverless platform. Such platforms keep \
             files in memory and lose them when an instance stops, and a network volume is \
             no safe home for SQLite either. Set DATABASE_URL to a managed Postgres. To run \
             with a throwaway store anyway, set {ALLOW_EPHEMERAL_STORE_ENV}=1."
        )),
    }
}

/// [`check_store_for_platform`] on the process environment. Call it before
/// any store is opened and before [`database_url`] creates `.jamjet`.
pub fn check_store_env(config: &ApiConfig) -> anyhow::Result<()> {
    let platform = on_serverless_platform(|k| std::env::var(k).ok());
    let storage_backend = std::env::var("STORAGE_BACKEND").ok();
    let kind = store_kind(storage_backend.as_deref(), &existing_database_url(config));
    let allow = env_flag(ALLOW_EPHEMERAL_STORE_ENV)?;
    match check_store_for_platform(platform, kind, allow) {
        StoreCheck::Ok => Ok(()),
        StoreCheck::Warn(msg) => {
            tracing::warn!("{msg}");
            Ok(())
        }
        StoreCheck::Refuse(msg) => anyhow::bail!("{msg}"),
    }
}

/// The seconds the platform gives between SIGTERM and SIGKILL, when it is not
/// the default. Read at startup. An integer of at least 1.
pub const STOP_TIMEOUT_ENV: &str = "JAMJET_STOP_TIMEOUT_SECS";

/// How long a serverless platform waits after SIGTERM before it kills the
/// process, when [`STOP_TIMEOUT_ENV`] does not say.
pub const PLATFORM_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Room left in the stop timeout for the process to exit after the drains.
pub const STOP_MARGIN: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a shutdown may take, as startup works it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownPlan {
    /// How long in-flight work gets (`JAMJET_SHUTDOWN_GRACE_SECS`).
    pub grace: std::time::Duration,
    /// The time the platform gives after SIGTERM, when there is one.
    pub stop_timeout: Option<std::time::Duration>,
}

fn secs_var(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    least: u64,
) -> Result<Option<std::time::Duration>, String> {
    let Some(raw) = get(name) else {
        return Ok(None);
    };
    match raw.trim().parse::<u64>() {
        Ok(n) if n >= least => Ok(Some(std::time::Duration::from_secs(n))),
        _ => Err(format!(
            "invalid {name} `{raw}`: expected a whole number of seconds, at least {least}"
        )),
    }
}

/// The grace and the stop timeout. With a stop timeout (set, or implied by a
/// platform) the grace defaults to what is left of it after the scheduler
/// and HTTP drains and a margin, and a grace that does not fit stops startup:
/// the platform would kill the process mid-drain. Without one the grace
/// defaults to [`jamjet_worker::shutdown::DEFAULT_SHUTDOWN_GRACE`].
pub fn shutdown_plan(
    get: impl Fn(&str) -> Option<String>,
    platform: bool,
) -> Result<ShutdownPlan, String> {
    use jamjet_worker::shutdown::{DEFAULT_SHUTDOWN_GRACE, HTTP_DRAIN, SCHEDULER_STOP};
    const GRACE_ENV: &str = "JAMJET_SHUTDOWN_GRACE_SECS";
    let stop_timeout = match secs_var(&get, STOP_TIMEOUT_ENV, 1)? {
        Some(t) => Some(t),
        None => platform.then_some(PLATFORM_STOP_TIMEOUT),
    };
    let set_grace = secs_var(&get, GRACE_ENV, 0)?;
    let Some(stop) = stop_timeout else {
        return Ok(ShutdownPlan {
            grace: set_grace.unwrap_or(DEFAULT_SHUTDOWN_GRACE),
            stop_timeout,
        });
    };
    let fixed = SCHEDULER_STOP + HTTP_DRAIN + STOP_MARGIN;
    if stop < fixed {
        return Err(format!(
            "{STOP_TIMEOUT_ENV}={} is too short: the scheduler and HTTP drains need {} s \
             plus {} s margin",
            stop.as_secs(),
            (SCHEDULER_STOP + HTTP_DRAIN).as_secs(),
            STOP_MARGIN.as_secs()
        ));
    }
    let grace = set_grace.unwrap_or(stop - fixed);
    if grace + fixed > stop {
        return Err(format!(
            "{GRACE_ENV}={} does not fit {STOP_TIMEOUT_ENV}={}: grace {} + scheduler {} + \
             HTTP drain {} + margin {} = {} s, more than the stop timeout. Lower \
             {GRACE_ENV} or raise {STOP_TIMEOUT_ENV}.",
            grace.as_secs(),
            stop.as_secs(),
            grace.as_secs(),
            SCHEDULER_STOP.as_secs(),
            HTTP_DRAIN.as_secs(),
            STOP_MARGIN.as_secs(),
            (grace + fixed).as_secs()
        ));
    }
    Ok(ShutdownPlan {
        grace,
        stop_timeout,
    })
}

/// [`shutdown_plan`] on the process environment. A value that is not UTF-8
/// is read with replacement characters, so it fails the parse and stops
/// startup rather than counting as unset.
pub fn shutdown_plan_from_env() -> anyhow::Result<ShutdownPlan> {
    let get = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
    let platform = on_serverless_platform(get);
    shutdown_plan(get, platform).map_err(|e| anyhow::anyhow!("{e}"))
}

/// `url` as a message may show it. A Postgres URL loses its user, password
/// and query (which can hold a password too), keeping the host, port and
/// database; any other URL is shown as it is.
pub fn shown_url(url: &str) -> String {
    if !is_postgres_url(url) {
        return url.to_string();
    }
    let Some((scheme, rest)) = url.trim_start().split_once("://") else {
        return "a Postgres database".to_string();
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}{path}")
}

// ── Request-driven mode ──

/// Turns the engine's own loops (scheduler, projector, embedded workers,
/// outbox drain, cron) on or off: `on`, the default, or `off`. With `off` a
/// caller drives the engine through `POST /ops/tick`.
pub const BACKGROUND_ENV: &str = "JAMJET_BACKGROUND";

/// Whether the engine runs its own loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    On,
    Off,
}

impl Background {
    /// The value as `JAMJET_BACKGROUND` spells it.
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// [`BACKGROUND_ENV`]: unset is on; `on` or `off`, in any case; anything
/// else is an error naming the variable. It is not a boolean switch, so
/// `0` or `false` is refused rather than read one way or the other.
pub fn background_from(get: impl Fn(&str) -> Option<String>) -> Result<Background, String> {
    let Some(raw) = get(BACKGROUND_ENV) else {
        return Ok(Background::On);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(Background::On),
        "off" => Ok(Background::Off),
        _ => Err(format!(
            "invalid {BACKGROUND_ENV} `{raw}`: expected `on` or `off`"
        )),
    }
}

/// Logged once at startup when the loops are off.
pub const LOOPS_OFF_NOTICE: &str = "background loops are off; call POST /ops/tick every minute";

/// Milliseconds an advance after a write may spend running engine steps.
pub const INLINE_ADVANCE_ENV: &str = "JAMJET_INLINE_ADVANCE_MS";
/// Milliseconds one tick may spend.
pub const TICK_BUDGET_ENV: &str = "JAMJET_TICK_BUDGET_MS";
/// Engine steps one tick runs at once.
pub const TICK_ENGINE_CONCURRENCY_ENV: &str = "JAMJET_TICK_ENGINE_CONCURRENCY";
/// Seconds between ticks the caller is expected to keep.
pub const TICK_INTERVAL_ENV: &str = "JAMJET_TICK_INTERVAL_SECS";

/// How the engine works with the loops off: inside a tick and after a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverConfig {
    /// [`INLINE_ADVANCE_ENV`], 2000 ms by default.
    pub inline_budget: std::time::Duration,
    /// [`TICK_BUDGET_ENV`], 45000 ms by default, at least 100 ms.
    pub tick_budget: std::time::Duration,
    /// [`TICK_ENGINE_CONCURRENCY_ENV`], 4 by default, at least 1.
    pub engine_concurrency: usize,
    /// [`TICK_INTERVAL_ENV`], 60 s by default.
    pub tick_interval: std::time::Duration,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            inline_budget: std::time::Duration::from_millis(2_000),
            tick_budget: std::time::Duration::from_millis(45_000),
            engine_concurrency: 4,
            tick_interval: std::time::Duration::from_secs(60),
        }
    }
}

/// `name` as a whole number of at least `least`, or its default when unset.
fn whole_at_least(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    least: u64,
    default: u64,
) -> Result<u64, String> {
    let Some(raw) = get(name) else {
        return Ok(default);
    };
    match raw.trim().parse::<u64>() {
        Ok(n) if n >= least => Ok(n),
        _ => Err(format!(
            "invalid {name} `{raw}`: expected a whole number, at least {least}"
        )),
    }
}

/// The driver settings, each defaulted when unset. A value that is set but
/// not valid is an error naming the variable.
pub fn driver_config_from(get: impl Fn(&str) -> Option<String>) -> Result<DriverConfig, String> {
    use std::time::Duration;
    let defaults = DriverConfig::default();
    let ms = |d: Duration| d.as_millis() as u64;
    let engine_concurrency = whole_at_least(
        &get,
        TICK_ENGINE_CONCURRENCY_ENV,
        1,
        defaults.engine_concurrency as u64,
    )?;
    Ok(DriverConfig {
        inline_budget: Duration::from_millis(whole_at_least(
            &get,
            INLINE_ADVANCE_ENV,
            1,
            ms(defaults.inline_budget),
        )?),
        tick_budget: Duration::from_millis(whole_at_least(
            &get,
            TICK_BUDGET_ENV,
            100,
            ms(defaults.tick_budget),
        )?),
        engine_concurrency: usize::try_from(engine_concurrency).map_err(|_| {
            format!("{TICK_ENGINE_CONCURRENCY_ENV} is too large: {engine_concurrency}")
        })?,
        tick_interval: Duration::from_secs(whole_at_least(
            &get,
            TICK_INTERVAL_ENV,
            1,
            defaults.tick_interval.as_secs(),
        )?),
    })
}

/// A tick may run [`DriverConfig::engine_concurrency`] engine steps at once,
/// each holding a connection, plus one for the tick itself. On Postgres the
/// steps must stay below `pool_size - 1`, so a tick never takes every
/// connection from the requests.
pub fn check_tick_pool(engine_concurrency: usize, pool_size: u32) -> Result<(), String> {
    let pool = u64::from(pool_size);
    if (engine_concurrency as u64).saturating_add(1) < pool {
        return Ok(());
    }
    let pool_env = jamjet_state::postgres::POOL_SIZE_ENV;
    Err(format!(
        "{TICK_ENGINE_CONCURRENCY_ENV}={engine_concurrency} must be below {pool_env} - 1 \
         (here {pool_size} - 1 = {}), or a tick could take every connection from the \
         requests. Lower {TICK_ENGINE_CONCURRENCY_ENV} or raise {pool_env}.",
        pool.saturating_sub(1)
    ))
}

/// A line startup logs once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Info(String),
    Warn(String),
}

/// What [`BACKGROUND_ENV`] means for this start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundPlan {
    pub background: Background,
    /// Parsed whatever the switch, so a typo never waits for the day the
    /// loops are turned off.
    pub driver: DriverConfig,
    /// What to log, in order.
    pub notices: Vec<Notice>,
}

/// The loops switch and its rules, before any store is opened. With the
/// loops off, `JAMJET_EMBED_CRON` on stops startup (cron is a loop), the
/// embedded workers are not started, and on Postgres a tick must leave pool
/// connections for the requests ([`check_tick_pool`]). With the loops on, a
/// serverless platform gets a warning: the loops need CPU between requests.
pub fn background_plan(
    get: impl Fn(&str) -> Option<String>,
    store: StoreKind,
) -> Result<BackgroundPlan, String> {
    let background = background_from(&get)?;
    let driver = driver_config_from(&get)?;
    let mut notices = Vec::new();
    match background {
        Background::On => {
            if on_serverless_platform(&get) {
                notices.push(Notice::Warn(format!(
                    "the background loops are on, and they need CPU between requests. On a \
                     serverless platform that throttles CPU outside requests they stall: set \
                     {BACKGROUND_ENV}=off there and call POST /ops/tick every minute"
                )));
            }
        }
        Background::Off => {
            if parse_flag(EMBED_CRON_ENV, get(EMBED_CRON_ENV).as_deref())? {
                return Err(format!(
                    "{EMBED_CRON_ENV} runs the cron scheduler as a background loop, and \
                     {BACKGROUND_ENV}=off turns the loops off: set {EMBED_CRON_ENV}=0 or unset it"
                ));
            }
            if store == StoreKind::Postgres {
                let pool = jamjet_state::PgSettings::from_lookup(|name| Ok(get(name)))?.pool_size;
                check_tick_pool(driver.engine_concurrency, pool)?;
            }
            notices.push(Notice::Info(LOOPS_OFF_NOTICE.to_string()));
            if parse_flag(EMBED_WORKERS_ENV, get(EMBED_WORKERS_ENV).as_deref())? {
                notices.push(Notice::Info(format!(
                    "{EMBED_WORKERS_ENV} is ignored with {BACKGROUND_ENV}=off: engine steps \
                     run inside requests instead"
                )));
            }
        }
    }
    Ok(BackgroundPlan {
        background,
        driver,
        notices,
    })
}

/// [`background_plan`] on the process environment. A value that is not
/// UTF-8 is read with replacement characters, so it fails the parse.
pub fn background_plan_from_env(store: StoreKind) -> anyhow::Result<BackgroundPlan> {
    let get = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
    background_plan(get, store).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_postgres_url_selects_postgres_and_memory_still_wins() {
        for url in [
            "postgres://u:p@db:5432/jamjet",
            "postgresql://db/jamjet",
            "POSTGRES://db/jamjet",
        ] {
            assert!(is_postgres_url(url), "{url}");
            assert_eq!(store_kind(None, url), StoreKind::Postgres, "{url}");
            assert_eq!(store_kind(Some("memory"), url), StoreKind::Memory);
        }
        for url in [
            "sqlite://.jamjet/runtime.db",
            "sqlite:///var/lib/postgres://x.db",
            "postgres:/db",
            "",
        ] {
            assert!(!is_postgres_url(url), "{url}");
            assert_eq!(store_kind(None, url), StoreKind::Sqlite, "{url}");
        }
    }

    #[test]
    fn a_shown_postgres_url_has_no_credentials() {
        assert_eq!(
            shown_url(
                "postgres://jj:s3cret@db.internal:5432/jamjet?password=other&sslmode=require"
            ),
            "postgres://db.internal:5432/jamjet"
        );
        assert_eq!(
            shown_url("postgresql://jj:p@ss@word@db/jamjet"),
            "postgresql://db/jamjet"
        );
        assert_eq!(shown_url("postgres://db"), "postgres://db");
        assert_eq!(
            shown_url("sqlite://.jamjet/runtime.db"),
            "sqlite://.jamjet/runtime.db"
        );
    }

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
            "JAMJET_ALLOW_EPHEMERAL_STORE",
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

    #[test]
    fn sqlite_on_a_platform_is_refused() {
        for url in ["sqlite://data/runtime.db", "", "sqlite::memory:"] {
            let kind = store_kind(None, url);
            assert_eq!(kind, StoreKind::Sqlite, "{url:?}");
            match check_store_for_platform(true, kind, false) {
                StoreCheck::Refuse(msg) => {
                    assert!(msg.contains("DATABASE_URL"), "{msg}");
                    assert!(msg.contains("JAMJET_ALLOW_EPHEMERAL_STORE"), "{msg}");
                }
                other => panic!("{url:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_allow_flag_and_the_memory_store_warn() {
        assert!(matches!(
            check_store_for_platform(true, StoreKind::Sqlite, true),
            StoreCheck::Warn(_)
        ));
        let kind = store_kind(Some("memory"), "postgres://h/db");
        assert_eq!(kind, StoreKind::Memory);
        assert!(matches!(
            check_store_for_platform(true, kind, false),
            StoreCheck::Warn(_)
        ));
    }

    #[test]
    fn postgres_on_a_platform_and_anything_off_it_is_ok() {
        for url in ["postgres://u@h/db", "postgresql://u@h/db"] {
            let kind = store_kind(None, url);
            assert_eq!(kind, StoreKind::Postgres, "{url}");
            assert_eq!(check_store_for_platform(true, kind, false), StoreCheck::Ok);
        }
        for kind in [StoreKind::Memory, StoreKind::Sqlite, StoreKind::Postgres] {
            for allow in [false, true] {
                assert_eq!(check_store_for_platform(false, kind, allow), StoreCheck::Ok);
            }
        }
    }

    #[test]
    fn a_blank_k_service_is_no_platform() {
        for v in ["", "  "] {
            assert!(!on_serverless_platform(|_| Some(v.to_string())), "{v:?}");
        }
        assert!(!on_serverless_platform(|_| None));
        assert!(on_serverless_platform(
            |k| (k == "K_SERVICE").then(|| "svc".to_string())
        ));
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_platform_default_grace_is_four_seconds() {
        use std::time::Duration;
        let plan = shutdown_plan(env(&[("K_SERVICE", "svc")]), true).unwrap();
        assert_eq!(plan.grace, Duration::from_secs(4));
        assert_eq!(plan.stop_timeout, Some(Duration::from_secs(10)));
        let plan = shutdown_plan(env(&[]), false).unwrap();
        assert_eq!(plan.grace, Duration::from_secs(20));
        assert_eq!(plan.stop_timeout, None);
        // An explicit stop timeout off a platform sets the grace too.
        let plan = shutdown_plan(env(&[("JAMJET_STOP_TIMEOUT_SECS", "30")]), false).unwrap();
        assert_eq!(plan.grace, Duration::from_secs(24));
        assert_eq!(plan.stop_timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_grace_that_does_not_fit_is_refused() {
        use std::time::Duration;
        let err = shutdown_plan(env(&[("JAMJET_SHUTDOWN_GRACE_SECS", "20")]), true).unwrap_err();
        assert!(err.contains("26"), "{err}");
        assert!(err.contains("10"), "{err}");
        assert!(err.contains("JAMJET_SHUTDOWN_GRACE_SECS"), "{err}");
        assert!(err.contains("JAMJET_STOP_TIMEOUT_SECS"), "{err}");
        // The largest grace that fits 10 s is 4 s.
        assert!(shutdown_plan(env(&[("JAMJET_SHUTDOWN_GRACE_SECS", "5")]), true).is_err());
        let plan = shutdown_plan(env(&[("JAMJET_SHUTDOWN_GRACE_SECS", "4")]), true).unwrap();
        assert_eq!(plan.grace, Duration::from_secs(4));
        let err = shutdown_plan(env(&[("JAMJET_STOP_TIMEOUT_SECS", "5")]), true).unwrap_err();
        assert!(err.contains("5 s plus 1 s margin"), "{err}");
        let plan = shutdown_plan(env(&[("JAMJET_STOP_TIMEOUT_SECS", "6")]), true).unwrap();
        assert_eq!(plan.grace, Duration::ZERO);
        assert_eq!(plan.stop_timeout, Some(Duration::from_secs(6)));
    }

    #[test]
    fn bad_values_are_refused() {
        for name in ["JAMJET_STOP_TIMEOUT_SECS", "JAMJET_SHUTDOWN_GRACE_SECS"] {
            for bad in ["abc", "-1", ""] {
                let pairs = [(name, bad)];
                assert!(shutdown_plan(env(&pairs), true).is_err(), "{name}={bad:?}");
                assert!(shutdown_plan(env(&pairs), false).is_err(), "{name}={bad:?}");
            }
        }
        // A stop timeout of 0 is not a timeout.
        assert!(shutdown_plan(env(&[("JAMJET_STOP_TIMEOUT_SECS", "0")]), false).is_err());
    }

    // ── Request-driven mode ──

    const OFF: (&str, &str) = ("JAMJET_BACKGROUND", "off");

    #[test]
    fn background_defaults_to_on() {
        assert_eq!(background_from(env(&[])), Ok(Background::On));
        for (raw, want) in [
            ("on", Background::On),
            ("On", Background::On),
            ("off", Background::Off),
            ("OFF", Background::Off),
            (" off ", Background::Off),
        ] {
            let pairs = [("JAMJET_BACKGROUND", raw)];
            assert_eq!(background_from(env(&pairs)), Ok(want), "{raw:?}");
        }
    }

    /// Not a boolean switch: `0` and `false` are refused, not read as off.
    #[test]
    fn background_refuses_a_typo() {
        for raw in ["of", "0", "1", "false", "true", "no", "", "offf"] {
            let pairs = [("JAMJET_BACKGROUND", raw)];
            let err = background_from(env(&pairs)).unwrap_err();
            assert!(err.contains("JAMJET_BACKGROUND"), "{raw:?}: {err}");
            assert!(
                background_plan(env(&pairs), StoreKind::Sqlite).is_err(),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn loops_off_with_embedded_cron_is_refused_naming_both() {
        for kind in [StoreKind::Memory, StoreKind::Sqlite, StoreKind::Postgres] {
            for on in ["1", "true"] {
                let err =
                    background_plan(env(&[OFF, ("JAMJET_EMBED_CRON", on)]), kind).unwrap_err();
                assert!(err.contains("JAMJET_BACKGROUND"), "{kind:?}: {err}");
                assert!(err.contains("JAMJET_EMBED_CRON"), "{kind:?}: {err}");
            }
        }
        assert!(
            background_plan(env(&[OFF, ("JAMJET_EMBED_CRON", "0")]), StoreKind::Sqlite).is_ok()
        );
        // With the loops on nothing changes.
        assert!(background_plan(env(&[("JAMJET_EMBED_CRON", "1")]), StoreKind::Sqlite).is_ok());
    }

    #[test]
    fn loops_off_says_so_once_and_ignores_embedded_workers() {
        let plan = background_plan(env(&[OFF]), StoreKind::Sqlite).unwrap();
        assert_eq!(plan.background, Background::Off);
        assert_eq!(
            plan.notices,
            vec![Notice::Info(
                "background loops are off; call POST /ops/tick every minute".to_string()
            )]
        );
        let plan = background_plan(
            env(&[OFF, ("JAMJET_EMBED_WORKERS", "1")]),
            StoreKind::Sqlite,
        )
        .unwrap();
        assert_eq!(plan.notices.len(), 2, "{:?}", plan.notices);
        match &plan.notices[1] {
            Notice::Info(line) => {
                assert!(line.contains("JAMJET_EMBED_WORKERS is ignored"), "{line}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn loops_on_warn_on_a_platform_only() {
        let platform = ("K_SERVICE", "svc");
        let plan = background_plan(env(&[platform]), StoreKind::Postgres).unwrap();
        assert_eq!(plan.background, Background::On);
        match plan.notices.as_slice() {
            [Notice::Warn(line)] => {
                assert!(line.contains("need CPU between requests"), "{line}");
                assert!(line.contains("JAMJET_BACKGROUND=off"), "{line}");
            }
            other => panic!("{other:?}"),
        }
        let plan = background_plan(env(&[]), StoreKind::Postgres).unwrap();
        assert_eq!(plan.notices, Vec::<Notice>::new());
        let plan = background_plan(env(&[platform, OFF]), StoreKind::Postgres).unwrap();
        assert!(
            !plan.notices.iter().any(|n| matches!(n, Notice::Warn(_))),
            "{:?}",
            plan.notices
        );
    }

    #[test]
    fn driver_settings_default_and_parse_strictly() {
        use std::time::Duration;
        let d = driver_config_from(env(&[])).unwrap();
        assert_eq!(d.inline_budget, Duration::from_millis(2000));
        assert_eq!(d.tick_budget, Duration::from_millis(45000));
        assert_eq!(d.engine_concurrency, 4);
        assert_eq!(d.tick_interval, Duration::from_secs(60));
        let d = driver_config_from(env(&[
            ("JAMJET_INLINE_ADVANCE_MS", "1"),
            ("JAMJET_TICK_BUDGET_MS", " 100 "),
            ("JAMJET_TICK_ENGINE_CONCURRENCY", "1"),
            ("JAMJET_TICK_INTERVAL_SECS", "1"),
        ]))
        .unwrap();
        assert_eq!(d.inline_budget, Duration::from_millis(1));
        assert_eq!(d.tick_budget, Duration::from_millis(100));
        assert_eq!(d.engine_concurrency, 1);
        assert_eq!(d.tick_interval, Duration::from_secs(1));
        for (name, bad) in [
            ("JAMJET_INLINE_ADVANCE_MS", "0"),
            ("JAMJET_INLINE_ADVANCE_MS", "2s"),
            ("JAMJET_TICK_BUDGET_MS", "99"),
            ("JAMJET_TICK_BUDGET_MS", "-1"),
            ("JAMJET_TICK_ENGINE_CONCURRENCY", "0"),
            ("JAMJET_TICK_ENGINE_CONCURRENCY", "four"),
            ("JAMJET_TICK_INTERVAL_SECS", "0"),
            ("JAMJET_TICK_INTERVAL_SECS", ""),
        ] {
            let pairs = [(name, bad)];
            let err = driver_config_from(env(&pairs)).unwrap_err();
            assert!(err.contains(name), "{name}={bad:?}: {err}");
            // Refused with the loops on too: a typo does not wait for the
            // day they are turned off.
            assert!(
                background_plan(env(&pairs), StoreKind::Sqlite).is_err(),
                "{name}={bad:?}"
            );
            let pairs = [(name, bad), OFF];
            assert!(
                background_plan(env(&pairs), StoreKind::Sqlite).is_err(),
                "{name}={bad:?}"
            );
        }
    }

    #[test]
    fn a_tick_must_leave_pool_connections_for_the_requests() {
        for (concurrency, pool) in [(4, 10), (3, 5), (1, 3)] {
            assert_eq!(
                check_tick_pool(concurrency, pool),
                Ok(()),
                "{concurrency}/{pool}"
            );
        }
        for (concurrency, pool) in [(4, 5), (9, 10), (1, 2), (20, 10), (usize::MAX, u32::MAX)] {
            let err = check_tick_pool(concurrency, pool).unwrap_err();
            assert!(err.contains("JAMJET_TICK_ENGINE_CONCURRENCY"), "{err}");
            assert!(err.contains("JAMJET_PG_POOL_SIZE"), "{err}");
        }
        // The defaults (4 steps, a pool of 10) fit.
        assert!(background_plan(env(&[OFF]), StoreKind::Postgres).is_ok());
        let small = ("JAMJET_PG_POOL_SIZE", "5");
        let err = background_plan(env(&[OFF, small]), StoreKind::Postgres).unwrap_err();
        assert!(err.contains("JAMJET_PG_POOL_SIZE"), "{err}");
        let three = ("JAMJET_TICK_ENGINE_CONCURRENCY", "3");
        assert!(background_plan(env(&[OFF, small, three]), StoreKind::Postgres).is_ok());
        // Only Postgres has the pool, and only a tick needs the room.
        assert!(background_plan(env(&[OFF, small]), StoreKind::Sqlite).is_ok());
        assert!(background_plan(env(&[small]), StoreKind::Postgres).is_ok());
        let err = background_plan(
            env(&[OFF, ("JAMJET_PG_POOL_SIZE", "lots")]),
            StoreKind::Postgres,
        )
        .unwrap_err();
        assert!(err.contains("JAMJET_PG_POOL_SIZE"), "{err}");
    }
}

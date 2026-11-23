//! Noticing that nobody ticks: `background` and `tick_overdue` in `/health`,
//! the error line while a tick is overdue, and `GET /ops/status`.
//!
//! With the background loops off, a tick is the only thing that expires
//! approvals, reclaims lapsed leases and runs the engine's own steps. When
//! the scheduler that calls `POST /ops/tick` stops, nothing else fails, so
//! the engine has to say so itself. These cases drive the router as a
//! platform's probe, a scheduler and a person reading the status do, over the
//! memory store, a SQLite file and, when `JAMJET_TEST_POSTGRES_URL` is set,
//! Postgres.

mod support;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::driver::{watch_ticks, Driver, TickWatch, WatchTiming};
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::startup::{Background, DriverConfig};
use jamjet_api::state::AppState;
use jamjet_state::backend::{StateBackend, StateBackendError};
use jamjet_state::{
    InMemoryBackend, PgBackend, PgSettings, SqliteBackend, Tenant, TenantId, TenantStatus,
};
use jamjet_testkit::pg::TestDatabase;
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// The line a request writes while a tick is overdue, after `no tick for N`.
const OVERDUE_TAIL: &str = " s while the background loops are off: approvals do not expire, \
     lapsed leases are not reclaimed, engine steps do not run";

// ── What a test does around the store's calls ────────────────────────────────

/// Counts the reads a watch makes, and fails, slows or skews them for a case.
struct Probe {
    /// Calls to `tick_status` that reached the hooks.
    reads: AtomicUsize,
    /// Calls to `store_now`.
    clock_reads: AtomicUsize,
    /// What `tick_status` fails with: 0 not at all, 1 as a database that
    /// cannot be reached does, 2 as a store with no tick state does.
    read_fault: AtomicUsize,
    /// `store_now` fails.
    clock_fault: AtomicBool,
    /// `finish_tick` fails: the tick ran and its state was not saved.
    finish_fault: AtomicBool,
    /// Sleep this long in every `tick_status`.
    read_delay_ms: AtomicU64,
    /// A `tick_status` is sleeping in `read_delay_ms` now.
    read_in_flight: AtomicBool,
    /// Seconds the store's clock is set apart from the process's; 0 asks the
    /// store.
    clock_skew_secs: AtomicI64,
}

impl Probe {
    fn new() -> Self {
        Self {
            reads: AtomicUsize::new(0),
            clock_reads: AtomicUsize::new(0),
            read_fault: AtomicUsize::new(0),
            clock_fault: AtomicBool::new(false),
            finish_fault: AtomicBool::new(false),
            read_delay_ms: AtomicU64::new(0),
            read_in_flight: AtomicBool::new(false),
            clock_skew_secs: AtomicI64::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn clock_reads(&self) -> usize {
        self.clock_reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Hooks for Probe {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::TickStatus => {
                self.reads.fetch_add(1, Ordering::SeqCst);
                let ms = self.read_delay_ms.load(Ordering::SeqCst);
                if ms > 0 {
                    self.read_in_flight.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    self.read_in_flight.store(false, Ordering::SeqCst);
                }
                match self.read_fault.load(Ordering::SeqCst) {
                    1 => return Some(StateBackendError::Unavailable("injected".into())),
                    2 => {
                        return Some(StateBackendError::Database(
                            "this backend cannot coordinate ticks".into(),
                        ))
                    }
                    _ => {}
                }
            }
            Op::StoreNow => {
                self.clock_reads.fetch_add(1, Ordering::SeqCst);
                if self.clock_fault.load(Ordering::SeqCst) {
                    return Some(StateBackendError::Unavailable("injected".into()));
                }
            }
            Op::FinishTick { .. } if self.finish_fault.load(Ordering::SeqCst) => {
                return Some(StateBackendError::Unavailable("injected".into()));
            }
            _ => {}
        }
        None
    }

    fn store_now(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let skew = self.clock_skew_secs.load(Ordering::SeqCst);
        (skew != 0).then(|| chrono::Utc::now() + chrono::Duration::seconds(skew))
    }
}

// ── The error lines a case writes ────────────────────────────────────────────

/// Every line written at level error on this thread while the guard lives.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

impl Lines {
    /// The `N` of each overdue line so far, after checking the line says
    /// the rest word for word, at level error.
    fn overdue(&self) -> Vec<u64> {
        let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
        text.lines()
            .filter_map(|line| {
                let (head, rest) = line.split_once("no tick for ")?;
                assert!(head.contains("ERROR"), "not an error line: {line}");
                let (n, tail) = rest.split_at(rest.find(' ').expect("a number of seconds"));
                assert_eq!(tail, OVERDUE_TAIL, "{line}");
                Some(n.parse().unwrap_or_else(|_| panic!("not a number: {line}")))
            })
            .collect()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Capture the error lines this thread writes. The cases run on a
/// current-thread runtime, so every request, and every task a tick spawns,
/// runs on the test's own thread.
fn capture() -> (Lines, tracing::subscriber::DefaultGuard) {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::ERROR)
        .finish();
    (lines, tracing::subscriber::set_default(subscriber))
}

// ── A store, and engines on it ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Memory,
    Sqlite,
    Postgres,
}

const KINDS: [Kind; 3] = [Kind::Memory, Kind::Sqlite, Kind::Postgres];

/// The stores a case runs on: all, or the ones `OPS_STATUS_KINDS` names
/// (`memory,sqlite,postgres`).
fn kinds() -> Vec<Kind> {
    let wanted = std::env::var("OPS_STATUS_KINDS").unwrap_or_default();
    KINDS
        .into_iter()
        .filter(|k| wanted.is_empty() || wanted.contains(&format!("{k:?}").to_lowercase()))
        .collect()
}

struct Env {
    kind: Kind,
    /// The store itself, read by the case without passing the probe.
    inner: Arc<dyn StateBackend>,
    /// The store behind the probe, as the engine sees it.
    backend: Arc<dyn StateBackend>,
    probe: Arc<Probe>,
    state: AppState,
    shutdown: jamjet_worker::Shutdown,
    _trigger: jamjet_worker::ShutdownTrigger,
    dir: Option<PathBuf>,
    _db: Option<TestDatabase>,
    /// Tokens, by the role they hold: operator, developer, reviewer, viewer.
    ops: String,
    alice: String,
    bob: String,
    viewer: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// One engine on the case's store: its router as `main` builds it, with a
/// driver and a watch, outside dev mode.
struct Engine {
    app: axum::Router,
}

impl Env {
    /// `None` for Postgres when it is not configured.
    async fn new(kind: Kind, case: &str) -> Option<Env> {
        let mut dir = None;
        let mut db = None;
        let inner: Arc<dyn StateBackend> = match kind {
            Kind::Memory => Arc::new(InMemoryBackend::new()),
            Kind::Sqlite => {
                let path = std::env::temp_dir().join(format!("jjstatus-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                let file = path.join("runtime.db");
                dir = Some(path);
                Arc::new(
                    SqliteBackend::open(&format!("sqlite://{}", file.display()))
                        .await
                        .expect("open the test database"),
                )
            }
            Kind::Postgres => {
                let Some(fresh) = TestDatabase::fresh().await else {
                    jamjet_testkit::skip(case);
                    return None;
                };
                let backend = PgBackend::connect(fresh.url(), PgSettings::default())
                    .await
                    .unwrap_or_else(|e| panic!("connect to the test database: {e}"));
                db = Some(fresh);
                Arc::new(backend)
            }
        };
        let probe = Arc::new(Probe::new());
        let backend: Arc<dyn StateBackend> = Arc::new(Intercept::new(inner.clone(), probe.clone()));
        let state = support::make_state(backend.clone());
        let now = chrono::Utc::now();
        let _ = inner
            .create_tenant(Tenant {
                id: TenantId::default(),
                name: "default".into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await;
        let mint = |name: &'static str, role: &'static str| {
            let inner = inner.clone();
            async move { inner.create_token(name, role).await.unwrap().0 }
        };
        let ops = mint("ops", "operator").await;
        let alice = mint("alice", "developer").await;
        let bob = mint("bob", "reviewer").await;
        let viewer = mint("viewer", "viewer").await;
        let (trigger, shutdown) = jamjet_worker::shutdown();
        Some(Env {
            kind,
            inner,
            backend,
            probe,
            state,
            shutdown,
            _trigger: trigger,
            dir,
            _db: db,
            ops,
            alice,
            bob,
            viewer,
        })
    }

    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "memory",
            Kind::Sqlite => "sqlite",
            Kind::Postgres => "postgres",
        }
    }

    /// An engine on this store whose loops are as `background` says, with
    /// a watch that expects a tick every `interval`. Its process is "up"
    /// from here.
    fn engine(&self, background: Background, interval: Duration) -> Engine {
        self.engine_with(background, interval, WatchTiming::default())
    }

    fn engine_with(
        &self,
        background: Background,
        interval: Duration,
        timing: WatchTiming,
    ) -> Engine {
        let watch = Arc::new(
            TickWatch::new(self.backend.clone(), background, interval).with_timing(timing),
        );
        let config = DriverConfig {
            inline_budget: Duration::from_secs(30),
            tick_budget: Duration::from_secs(60),
            engine_concurrency: 2,
            tick_interval: interval,
        };
        let driver = Driver::new(
            &self.state,
            Arc::new(jamjet_models::ModelRegistry::new()),
            self.shutdown.clone(),
            config,
        )
        .with_background(background)
        .with_watch(watch.clone());
        let app = build_router_with_opts(self.state.clone(), false)
            .layer(axum::Extension(Arc::new(driver)));
        Engine {
            app: watch_ticks(app, watch),
        }
    }

    /// `GET /health`, as a platform's probe sends it: no token. It always
    /// answers 200 with `status: ok`.
    async fn health(&self, engine: &Engine) -> Value {
        let (st, body) = send(&engine.app, "GET", "/health", None).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", self.name());
        assert_eq!(body["status"], "ok", "{}: {body}", self.name());
        body
    }

    /// `tick_overdue` from `/health`.
    async fn overdue(&self, engine: &Engine) -> Value {
        self.health(engine).await["tick_overdue"].clone()
    }

    /// `POST /ops/tick` with the operator's token; it must have run.
    async fn tick(&self, engine: &Engine) -> Value {
        let (st, report) = send(&engine.app, "POST", "/ops/tick", Some(&self.ops)).await;
        assert_eq!(st, StatusCode::OK, "{}: {report}", self.name());
        assert_eq!(report["ran"], true, "{}: {report}", self.name());
        report
    }

    /// `GET /ops/status` with `token`.
    async fn status(&self, engine: &Engine, token: &str) -> (StatusCode, Value) {
        send(&engine.app, "GET", "/ops/status", Some(token)).await
    }

    /// One request of each sort an engine serves: the probe, a read with a
    /// token, a refused one, a write, the app's page and a path that is
    /// nothing. None of them reads or changes the tick row.
    async fn assorted(&self, engine: &Engine) {
        let _ = send(&engine.app, "GET", "/health", None).await;
        let _ = send(&engine.app, "GET", "/executions", Some(&self.alice)).await;
        let _ = send(&engine.app, "GET", "/executions", None).await;
        let _ = send(&engine.app, "POST", "/workflows", Some(&self.alice)).await;
        let _ = send(&engine.app, "GET", "/", None).await;
        let _ = send(&engine.app, "GET", "/no/such/path", Some(&self.bob)).await;
    }
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn sleep_until(at: Instant) {
    tokio::time::sleep(at.saturating_duration_since(Instant::now())).await;
}

// ── /health ──────────────────────────────────────────────────────────────────

/// With a tick expected every second: not overdue at first, overdue once
/// the process has been up for three intervals with no tick, not overdue
/// right after one, and overdue again three intervals after it. The probe's
/// `status` stays `ok` throughout.
#[tokio::test]
async fn health_reports_overdue_after_three_intervals() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "health_reports_overdue_after_three_intervals").await else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_secs(1));
        let up = Instant::now();
        let body = env.health(&engine).await;
        assert_eq!(body["background"], "off", "{name}: {body}");
        assert_eq!(body["tick_overdue"], false, "{name}: {body}");

        sleep_until(up + Duration::from_millis(3_500)).await;
        let body = env.health(&engine).await;
        assert_eq!(body["tick_overdue"], true, "{name}: {body}");
        assert_eq!(body["background"], "off", "{name}: {body}");

        env.tick(&engine).await;
        let ticked = Instant::now();
        assert_eq!(
            env.overdue(&engine).await,
            false,
            "{name}: right after a tick"
        );

        sleep_until(ticked + Duration::from_millis(3_500)).await;
        assert_eq!(
            env.overdue(&engine).await,
            true,
            "{name}: three intervals after the last tick"
        );
        // Every answer came from the one read the first request made.
        assert_eq!(env.probe.reads(), 1, "{name}");
    }
}

/// With the loops on there is no tick to wait for: `background: on`, and
/// `tick_overdue` null however long the process has been up, with nothing
/// read. A router built without a watch says the same.
#[tokio::test]
async fn with_the_loops_on_there_is_nothing_to_be_overdue() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "with_the_loops_on_there_is_nothing_to_be_overdue").await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::On, Duration::from_millis(10));
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..10 {
            let body = env.health(&engine).await;
            assert_eq!(body["background"], "on", "{name}: {body}");
            assert_eq!(body["tick_overdue"], Value::Null, "{name}: {body}");
        }
        assert_eq!(env.probe.reads(), 0, "{name}");
        assert_eq!(env.probe.clock_reads(), 0, "{name}");

        let (st, body) = send(
            &build_router_with_opts(env.state.clone(), false),
            "GET",
            "/health",
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");
        assert_eq!(body["status"], "ok", "{name}: {body}");
        assert_eq!(body["background"], "on", "{name}: {body}");
        assert_eq!(body["tick_overdue"], Value::Null, "{name}: {body}");
    }
}

/// A hundred probes in a row cost one read of the tick row and one of the
/// store's clock. Every other request checks the watch too, and reads
/// nothing more.
#[tokio::test]
async fn health_reads_the_store_at_most_every_thirty_seconds() {
    assert_eq!(WatchTiming::default().read_every, Duration::from_secs(30));
    for kind in kinds() {
        let Some(env) = Env::new(kind, "health_reads_the_store_at_most_every_thirty_seconds").await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_secs(60));
        for _ in 0..100 {
            assert_eq!(env.overdue(&engine).await, false, "{name}");
        }
        assert_eq!(env.probe.reads(), 1, "{name}: reads of the tick row");
        assert_eq!(
            env.probe.clock_reads(),
            1,
            "{name}: reads of the store's clock"
        );
        for _ in 0..10 {
            env.assorted(&engine).await;
        }
        assert_eq!(env.probe.reads(), 1, "{name}: after other requests");
        assert_eq!(env.probe.clock_reads(), 1, "{name}");
    }
}

/// Once the read window has passed, the next request reads again, once.
/// While a read is out, the requests that come meanwhile answer from what
/// the last read saw and read nothing themselves.
#[tokio::test]
async fn a_read_window_that_has_passed_reads_once_more() {
    let timing = WatchTiming {
        read_every: Duration::from_millis(300),
        ..WatchTiming::default()
    };
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_read_window_that_has_passed_reads_once_more").await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine_with(Background::Off, Duration::from_secs(60), timing);
        for _ in 0..10 {
            assert_eq!(env.overdue(&engine).await, false, "{name}");
        }
        assert_eq!(env.probe.reads(), 1, "{name}");
        tokio::time::sleep(Duration::from_millis(350)).await;
        for _ in 0..10 {
            assert_eq!(env.overdue(&engine).await, false, "{name}");
        }
        assert_eq!(
            env.probe.reads(),
            2,
            "{name}: one more read after the window"
        );

        // Twenty probes at once, the first of them held in its read.
        tokio::time::sleep(Duration::from_millis(350)).await;
        env.probe.read_delay_ms.store(200, Ordering::SeqCst);
        let mut probes = Vec::new();
        for _ in 0..20 {
            let app = engine.app.clone();
            let probe = env.probe.clone();
            probes.push(tokio::spawn(async move {
                let (st, body) = send(&app, "GET", "/health", None).await;
                (st, body, probe.read_in_flight.load(Ordering::SeqCst))
            }));
        }
        let mut during = 0;
        for p in probes {
            let (st, body, in_flight) = p.await.unwrap();
            assert_eq!(st, StatusCode::OK, "{name}: {body}");
            assert_eq!(body["tick_overdue"], false, "{name}: {body}");
            during += usize::from(in_flight);
        }
        env.probe.read_delay_ms.store(0, Ordering::SeqCst);
        assert!(
            during > 0,
            "{name}: no probe answered while the read was out"
        );
        assert_eq!(env.probe.reads(), 3, "{name}: one read for the twenty");
    }
}

/// A store whose tick row cannot be read, that has no tick state, whose
/// clock cannot be read, or that takes longer than the read timeout, gives
/// `tick_overdue: null`, and `status` stays `ok`. Nothing is logged as
/// overdue. A failed read counts as the window's read, and the read after
/// the window gives an answer again.
#[tokio::test]
async fn an_unreadable_store_gives_null() {
    let (lines, _guard) = capture();
    let timing = WatchTiming {
        read_every: Duration::from_millis(300),
        read_timeout: Duration::from_millis(200),
        ..WatchTiming::default()
    };
    // Every process here has been up for longer than three intervals, so a
    // store that could be read would say overdue.
    let interval = Duration::from_millis(10);
    for kind in kinds() {
        let Some(env) = Env::new(kind, "an_unreadable_store_gives_null").await else {
            continue;
        };
        let name = env.name();
        let p = &env.probe;
        for (what, read_fault, clock_fault) in [
            ("unreachable", 1, false),
            ("no tick state", 2, false),
            ("no clock", 0, true),
        ] {
            p.read_fault.store(read_fault, Ordering::SeqCst);
            p.clock_fault.store(clock_fault, Ordering::SeqCst);
            let engine = env.engine_with(Background::Off, interval, timing);
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = env.health(&engine).await;
            assert_eq!(body["tick_overdue"], Value::Null, "{name}, {what}: {body}");
            assert_eq!(body["background"], "off", "{name}, {what}: {body}");
            env.assorted(&engine).await;
        }
        p.read_fault.store(0, Ordering::SeqCst);
        p.clock_fault.store(false, Ordering::SeqCst);

        // A read that hangs: the probe is answered at the timeout.
        p.read_delay_ms.store(5_000, Ordering::SeqCst);
        let engine = env.engine_with(Background::Off, interval, timing);
        let asked = Instant::now();
        assert_eq!(
            env.overdue(&engine).await,
            Value::Null,
            "{name}: a hung read"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{name}: the probe waited {:?}",
            asked.elapsed()
        );
        p.read_delay_ms.store(0, Ordering::SeqCst);
        assert!(lines.overdue().is_empty(), "{name}: {:?}", lines.overdue());

        // The store comes back: the failed read stands for its window, then
        // the next read answers.
        p.read_fault.store(1, Ordering::SeqCst);
        let engine = env.engine_with(Background::Off, interval, timing);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(env.overdue(&engine).await, Value::Null, "{name}");
        p.read_fault.store(0, Ordering::SeqCst);
        let before = p.reads();
        assert_eq!(
            env.overdue(&engine).await,
            Value::Null,
            "{name}: within the window"
        );
        assert_eq!(
            p.reads(),
            before,
            "{name}: a failed read is the window's read"
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(env.overdue(&engine).await, true, "{name}: after the window");
        assert_eq!(p.reads(), before + 1, "{name}");
        lines.clear();
    }
}

/// A tick this engine ran whose state the store did not save does not make
/// `/health` say a tick finished: other engines on the store would not see
/// one either.
#[tokio::test]
async fn a_tick_whose_state_was_not_saved_does_not_count() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_tick_whose_state_was_not_saved_does_not_count").await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_millis(200));
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(env.overdue(&engine).await, true, "{name}");
        env.probe.finish_fault.store(true, Ordering::SeqCst);
        env.tick(&engine).await;
        assert_eq!(
            env.overdue(&engine).await,
            true,
            "{name}: the tick was not saved"
        );
        let row = env
            .inner
            .tick_status()
            .await
            .unwrap()
            .expect("the tick began");
        assert_eq!(row.last_finished_at, None, "{name}: {row:?}");
    }
}

/// A tick another engine finishes is seen by this one at its next read of
/// the row: until then this one answers from what it last read. A read of
/// `GET /ops/status` is such a read.
#[tokio::test]
async fn a_tick_another_engine_finishes_is_seen_at_the_next_read() {
    for kind in kinds() {
        let Some(env) = Env::new(
            kind,
            "a_tick_another_engine_finishes_is_seen_at_the_next_read",
        )
        .await
        else {
            continue;
        };
        let name = env.name();
        let here = env.engine(Background::Off, Duration::from_secs(1));
        let up = Instant::now();
        assert_eq!(env.overdue(&here).await, false, "{name}");
        sleep_until(up + Duration::from_millis(3_500)).await;
        assert_eq!(env.overdue(&here).await, true, "{name}");

        let there = env.engine(Background::Off, Duration::from_secs(1));
        env.tick(&there).await;
        assert_eq!(
            env.overdue(&here).await,
            true,
            "{name}: this engine has not read the row since"
        );
        let (st, status) = env.status(&here, &env.viewer).await;
        assert_eq!(st, StatusCode::OK, "{name}: {status}");
        assert_eq!(status["tick_overdue"], false, "{name}: {status}");
        assert!(
            status["tick"]["last_finished_at"].is_string(),
            "{name}: {status}"
        );
        assert_eq!(env.overdue(&here).await, false, "{name}: after the read");
    }
}

/// The age of the last tick is taken on the store's clock, the clock the
/// store stamped it with, not this process's: a store whose clock is an
/// hour ahead sees a tick that just finished as an hour old, and one an
/// hour behind as not old at all. The line says how long, in seconds.
#[tokio::test]
async fn the_age_of_the_last_tick_is_read_on_the_store_clock() {
    let (lines, _guard) = capture();
    for kind in kinds() {
        let Some(env) = Env::new(kind, "the_age_of_the_last_tick_is_read_on_the_store_clock").await
        else {
            continue;
        };
        let name = env.name();
        let first = env.engine(Background::Off, Duration::from_secs(60));
        env.tick(&first).await;
        lines.clear();

        env.probe.clock_skew_secs.store(3_600, Ordering::SeqCst);
        let ahead = env.engine(Background::Off, Duration::from_secs(60));
        assert_eq!(env.overdue(&ahead).await, true, "{name}: an hour ahead");
        let written = lines.overdue();
        assert_eq!(written.len(), 1, "{name}: {written:?}");
        // The test's Postgres runs in a container whose clock may be a few
        // seconds from this machine's.
        assert!((3_595..=3_605).contains(&written[0]), "{name}: {written:?}");

        env.probe.clock_skew_secs.store(-3_600, Ordering::SeqCst);
        let behind = env.engine(Background::Off, Duration::from_secs(60));
        assert_eq!(env.overdue(&behind).await, false, "{name}: an hour behind");
        env.probe.clock_skew_secs.store(0, Ordering::SeqCst);
        lines.clear();
    }
}

// ── The error line ───────────────────────────────────────────────────────────

/// While a tick is overdue, any request writes the line, and one engine
/// writes it at most once per window (here 2 s, 5 minutes by default). A
/// tick stops it. With the loops on it is never written.
#[tokio::test]
async fn overdue_is_logged_once_per_window() {
    assert_eq!(WatchTiming::default().log_every, Duration::from_secs(300));
    let (lines, _guard) = capture();
    let timing = WatchTiming {
        log_every: Duration::from_secs(2),
        ..WatchTiming::default()
    };
    for kind in kinds() {
        let Some(env) = Env::new(kind, "overdue_is_logged_once_per_window").await else {
            continue;
        };
        let name = env.name();
        lines.clear();
        let engine = env.engine_with(Background::Off, Duration::from_secs(1), timing);
        let up = Instant::now();
        for _ in 0..2 {
            env.assorted(&engine).await;
        }
        assert_eq!(lines.overdue(), Vec::<u64>::new(), "{name}: on time");

        // No tick has finished, so `N` is how long the process has been up
        // when the line was written: between these two readings of it.
        sleep_until(up + Duration::from_millis(3_200)).await;
        let from = up.elapsed().as_secs();
        for _ in 0..2 {
            env.assorted(&engine).await;
        }
        let to = up.elapsed().as_secs();
        let written = lines.overdue();
        assert_eq!(written.len(), 1, "{name}: {written:?}");
        assert!(
            (from..=to).contains(&written[0]),
            "{name}: {written:?}, {from}..={to}"
        );

        tokio::time::sleep(Duration::from_millis(2_100)).await;
        let from = up.elapsed().as_secs();
        for _ in 0..2 {
            env.assorted(&engine).await;
        }
        let to = up.elapsed().as_secs();
        let written = lines.overdue();
        assert_eq!(written.len(), 2, "{name}: the next window: {written:?}");
        assert!(
            (from..=to).contains(&written[1]),
            "{name}: {written:?}, {from}..={to}"
        );

        // A tick: the window passes again, the tick is still recent, and
        // nothing is written.
        env.tick(&engine).await;
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        env.assorted(&engine).await;
        assert_eq!(lines.overdue().len(), 2, "{name}: after a tick");

        // The loops on, a process up for longer than three intervals.
        let on = env.engine_with(Background::On, Duration::from_millis(10), timing);
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..3 {
            env.assorted(&on).await;
        }
        assert_eq!(lines.overdue().len(), 2, "{name}: loops on");
    }
}

// ── GET /ops/status ──────────────────────────────────────────────────────────

/// The tick row as the store keeps it, with the last tick's report as that
/// tick answered it, and what `/health` says about it.
#[tokio::test]
async fn ops_status_returns_the_stored_status_and_the_last_report() {
    for kind in kinds() {
        let Some(env) = Env::new(
            kind,
            "ops_status_returns_the_stored_status_and_the_last_report",
        )
        .await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_secs(5));
        let (st, body) = env.status(&engine, &env.viewer).await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");
        assert_eq!(
            body,
            json!({
                "background": "off",
                "tick_interval_secs": 5,
                "tick_overdue": false,
                "tick": null,
            }),
            "{name}: before any tick"
        );

        let before = chrono::Utc::now() - chrono::Duration::seconds(5);
        let report = env.tick(&engine).await;
        let (st, body) = env.status(&engine, &env.viewer).await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");
        assert_eq!(body["background"], "off", "{name}: {body}");
        assert_eq!(body["tick_overdue"], false, "{name}: {body}");
        let tick = &body["tick"];
        let mut keys: Vec<&str> = tick
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "holder",
                "last_complete",
                "last_finished_at",
                "last_report",
                "last_started_at",
                "lease_until",
                "next_after"
            ],
            "{name}: {body}"
        );
        assert_eq!(tick["holder"], Value::Null, "{name}: {body}");
        assert_eq!(tick["lease_until"], Value::Null, "{name}: {body}");
        assert_eq!(tick["last_complete"], true, "{name}: {body}");
        assert_eq!(tick["next_after"], Value::Null, "{name}: {body}");
        assert_eq!(tick["last_report"], report, "{name}: the tick's own answer");
        let at = |field: &str| -> chrono::DateTime<chrono::Utc> {
            tick[field]
                .as_str()
                .and_then(|t| t.parse().ok())
                .unwrap_or_else(|| panic!("{name}: {field} is not a time: {body}"))
        };
        let (started, finished) = (at("last_started_at"), at("last_finished_at"));
        assert!(started <= finished, "{name}: {body}");
        assert!(started > before, "{name}: {body}");

        // A tick that holds the lease now: who, and until when.
        let held = env
            .inner
            .begin_tick("another-engine:1", Duration::from_secs(600))
            .await
            .unwrap();
        assert!(
            matches!(held, jamjet_state::TickBegin::Began { .. }),
            "{name}: {held:?}"
        );
        let (_, body) = env.status(&engine, &env.viewer).await;
        assert_eq!(body["tick"]["holder"], "another-engine:1", "{name}: {body}");
        let until: chrono::DateTime<chrono::Utc> = body["tick"]["lease_until"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let left = until - chrono::Utc::now();
        assert!(
            left > chrono::Duration::seconds(590) && left <= chrono::Duration::seconds(605),
            "{name}: {body}"
        );
        // The last finished tick is still the one before.
        assert_eq!(body["tick"]["last_report"], report, "{name}: {body}");
    }
}

/// Any role may read it, viewers too, as it changes nothing. A request
/// without a valid token is refused, and it takes GET only.
#[tokio::test]
async fn ops_status_is_for_viewers_and_above() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "ops_status_is_for_viewers_and_above").await else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_secs(60));
        for (who, token) in [
            ("viewer", &env.viewer),
            ("reviewer", &env.bob),
            ("developer", &env.alice),
            ("operator", &env.ops),
        ] {
            let (st, body) = env.status(&engine, token).await;
            assert_eq!(st, StatusCode::OK, "{name}, {who}: {body}");
            assert_eq!(body["background"], "off", "{name}, {who}: {body}");
        }
        let (st, body) = send(&engine.app, "GET", "/ops/status", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{name}: {body}");
        let (st, body) = send(&engine.app, "GET", "/ops/status", Some("made-up")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{name}: {body}");
        for method in ["POST", "PUT", "DELETE"] {
            let (st, body) = send(&engine.app, method, "/ops/status", Some(&env.ops)).await;
            assert_eq!(
                st,
                StatusCode::METHOD_NOT_ALLOWED,
                "{name}, {method}: {body}"
            );
        }
    }
}

/// A store that cannot be read answers the status as the store's error
/// does: 503 with `Retry-After` when it cannot be reached, 500 when it has
/// no tick state.
#[tokio::test]
async fn ops_status_says_when_the_store_cannot_be_read() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "ops_status_says_when_the_store_cannot_be_read").await
        else {
            continue;
        };
        let name = env.name();
        let engine = env.engine(Background::Off, Duration::from_secs(60));
        env.probe.read_fault.store(1, Ordering::SeqCst);
        let resp = engine
            .app
            .clone()
            .oneshot(
                Request::get("/ops/status")
                    .header("authorization", format!("Bearer {}", env.viewer))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{name}");
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap()),
            Some("1"),
            "{name}"
        );
        env.probe.read_fault.store(2, Ordering::SeqCst);
        let (st, body) = env.status(&engine, &env.viewer).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{name}: {body}");
        assert!(body["error"].is_string(), "{name}: {body}");
        env.probe.read_fault.store(0, Ordering::SeqCst);
        // `/health` says null meanwhile, and `status` is still `ok`.
        assert_eq!(env.overdue(&engine).await, Value::Null, "{name}");
    }
}

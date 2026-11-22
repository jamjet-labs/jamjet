//! What several case modules share: tenant views, skipping a case on a
//! store it does not apply to, racing two calls, and stored times compared
//! at the precision a store keeps.

use crate::conformance::{Fixture, StoreKind};
use chrono::{DateTime, SubsecRound, Timelike, Utc};
use jamjet_state::{StateBackend, Tenant, TenantId, TenantStatus};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Whether the case does not apply to this store. `kinds` are the stores it
/// skips and `why` says why; the case returns when this answers true.
pub fn not_on(fx: &Fixture, kinds: &[StoreKind], why: &str) -> bool {
    let skip = kinds.contains(&fx.kind);
    if skip {
        skipped(fx, why);
    }
    skip
}

/// Say the case stops here on this store, and why. One line on stderr, so
/// a run with `--nocapture` shows what each store left out.
pub fn skipped(fx: &Fixture, why: &str) {
    eprintln!("not on {:?}: {why}", fx.kind);
}

/// For a case that needs tenant views, on a store that has none.
pub fn without_tenant_views(fx: &Fixture) {
    skipped(fx, "the store has no tenant views");
}

/// Whether a check is left out on this store because it fails there and
/// never ran there before the suite: a finding for the owner to decide, not
/// a porting error. `kinds` are the stores it is left out on.
pub fn finding(fx: &Fixture, kinds: &[StoreKind], what: &str) -> bool {
    let left_out = kinds.contains(&fx.kind);
    if left_out {
        eprintln!("finding on {:?}: {what}", fx.kind);
    }
    left_out
}

/// A tenant row for `id`, active.
pub fn tenant_row(id: &str) -> Tenant {
    let now = chrono::Utc::now();
    Tenant {
        id: TenantId::from(id),
        name: id.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    }
}

/// `tenant`'s view of the store, with its tenant row made first (the SQL
/// stores refuse an unknown tenant). `None` where the fixture has no
/// scoping.
pub async fn tenant_view(fx: &Fixture, tenant: &str) -> Option<Arc<dyn StateBackend>> {
    let scoper = fx.scoped.as_ref()?;
    let id = TenantId::from(tenant);
    let view = scoper(&id);
    if view.get_tenant(&id).await.unwrap().is_none() {
        view.create_tenant(tenant_row(tenant)).await.unwrap();
    }
    Some(view)
}

/// The backend a case runs on, seen through `tenant` where the fixture is
/// scoped, so the scoped code runs with a tenant other than the default.
/// Elsewhere, the fixture's backend.
pub async fn in_tenant(fx: &Fixture, tenant: &str) -> Arc<dyn StateBackend> {
    match tenant_view(fx, tenant).await {
        Some(view) => view,
        None => fx.backend.clone(),
    }
}

/// `t` with digits below the microsecond, as Linux's clock gives them.
/// macOS's clock steps in whole microseconds, so a case that stores a time
/// it built from `Utc::now()` meets those digits on every platform this way.
pub fn with_nanos(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_nanosecond(t.nanosecond() / 1_000 * 1_000 + 789)
        .expect("a time from the clock is not in a leap second")
}

/// `t` cut to whole microseconds, the precision a Postgres `timestamptz`
/// keeps. A case that reads back a time it stored compares at this
/// precision.
pub fn to_micros(t: DateTime<Utc>) -> DateTime<Utc> {
    t.trunc_subsecs(6)
}

/// When a call started and when it returned.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    start: Instant,
    end: Instant,
}

impl Span {
    /// Both calls had started before either returned.
    pub fn overlaps(self, other: Span) -> bool {
        self.start.max(other.start) < self.end.min(other.end)
    }
}

/// Run `work` and say when it started and returned.
pub async fn timed<F: std::future::Future>(work: F) -> (F::Output, Span) {
    let start = Instant::now();
    let out = work.await;
    let end = Instant::now();
    (out, Span { start, end })
}

/// Run `a` and `b` at once, and say whether they overlapped: both started
/// before either returned. Each runs on a thread of its own that spins until
/// both are ready, so the two start within nanoseconds. Woken by a barrier
/// instead, the second starts a wake-up later, by which time an in-memory
/// write (no await between its checks and its writes) has finished, and the
/// race never opens. Two calls run one after the other give the answers the
/// race tests accept, so a round that did not overlap shows nothing; each
/// race test asserts that some rounds did.
pub async fn race<A, B>(a: A, b: B) -> (A::Output, B::Output, bool)
where
    A: std::future::Future + Send + 'static,
    A::Output: Send + 'static,
    B: std::future::Future + Send + 'static,
    B::Output: Send + 'static,
{
    let start = Start::new();
    let a = start.spin_then_run(a);
    let b = start.spin_then_run(b);
    start.go(2).await;
    let (x, a_span) = a.await.unwrap();
    let (y, b_span) = b.await.unwrap();
    (x, y, a_span.overlaps(b_span))
}

/// Run every future in `work` at once, each on a thread of its own, started
/// as [`race`] starts its two, and return their outputs in order.
pub async fn all_at_once<F>(work: Vec<F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let start = Start::new();
    let n = work.len();
    let running: Vec<_> = work.into_iter().map(|w| start.spin_then_run(w)).collect();
    start.go(n).await;
    let mut out = Vec::with_capacity(n);
    for r in running {
        out.push(r.await.unwrap().0);
    }
    out
}

/// A start line: threads spin on it until every one is ready, then all go.
struct Start {
    ready: Arc<AtomicUsize>,
    go: Arc<AtomicBool>,
}

impl Start {
    fn new() -> Self {
        Self {
            ready: Arc::new(AtomicUsize::new(0)),
            go: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run `work` on a thread of its own once the start line opens.
    fn spin_then_run<F>(&self, work: F) -> tokio::task::JoinHandle<(F::Output, Span)>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (ready, go) = (self.ready.clone(), self.go.clone());
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            ready.fetch_add(1, Ordering::AcqRel);
            while !go.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            runtime.block_on(timed(work))
        })
    }

    /// Open the line once `n` threads are spinning on it.
    async fn go(&self, n: usize) {
        while self.ready.load(Ordering::Acquire) < n {
            tokio::task::yield_now().await;
        }
        self.go.store(true, Ordering::Release);
    }
}

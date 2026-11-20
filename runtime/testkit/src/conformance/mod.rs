//! One behaviour suite for every [`StateBackend`].
//!
//! A case is an async function in [`cases`] that takes a [`Fixture`]. A
//! [`StoreFactory`] makes a fresh fixture for one backend, and
//! [`conformance_suite!`](crate::conformance_suite) turns every case into a
//! `#[tokio::test]` on that factory, so a backend's suite is one line in
//! `testkit/tests/`:
//!
//! ```text
//! jamjet_testkit::conformance_suite!(jamjet_testkit::conformance::SqliteFactory);
//! ```
//!
//! A case reaches the store only through the fixture: the trait on
//! [`Fixture::backend`], [`Fixture::second`] and [`Fixture::scoped`], and
//! [`StoreHooks`] for what the trait cannot do (backdate a lease, read a raw
//! row, reopen the store). So the same case runs unchanged on every backend.

pub mod cases;
mod factories;
mod hooks;

pub use factories::{MemoryFactory, SqliteFactory, SqliteScopedFactory};
pub use hooks::{ItemRow, StoreHooks};

use async_trait::async_trait;
use jamjet_state::{StateBackend, TenantId};
use std::sync::Arc;

/// The store seen through one tenant, as the server's per-request backend
/// sees it.
pub type Scoper = Arc<dyn Fn(&TenantId) -> Arc<dyn StateBackend> + Send + Sync>;

/// Which backend a fixture holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Memory,
    Sqlite,
    /// SQLite through `TenantScopedSqliteBackend`.
    SqliteScoped,
    Postgres,
    /// Postgres scoped to a tenant.
    PostgresScoped,
}

impl StoreKind {
    /// Whether [`Fixture::backend`] is a tenant-scoped view.
    pub fn is_scoped(self) -> bool {
        matches!(self, StoreKind::SqliteScoped | StoreKind::PostgresScoped)
    }
}

/// A fresh store for one case. Fields drop in order, so `keep` (the temp
/// directory or the test database) goes last, after every pool on it.
pub struct Fixture {
    /// The backend the case runs against. Unscoped for `Memory`, `Sqlite`
    /// and `Postgres`, as the engine holds it. For the scoped kinds, the
    /// default tenant's scoped view, so every case runs through the scoped
    /// code.
    pub backend: Arc<dyn StateBackend>,
    /// The same store through a pool of its own, as a second engine sees
    /// it, and scoped like `backend`. On memory, the same `Arc`: one
    /// process, so a race between the two handles races the backend's own
    /// locking.
    pub second: Option<Arc<dyn StateBackend>>,
    /// The same store, and `backend`'s pool, seen through any tenant. Set
    /// for the scoped kinds only.
    pub scoped: Option<Scoper>,
    /// What a case needs and the trait does not offer.
    pub hooks: Arc<dyn StoreHooks>,
    pub kind: StoreKind,
    /// What keeps the store alive: a temp directory, a test database, or
    /// nothing.
    pub keep: Box<dyn std::any::Any + Send + Sync>,
}

/// Makes a fresh [`Fixture`] per case.
#[async_trait]
pub trait StoreFactory: Send + Sync {
    /// `None` means the store is not configured here and the case skips:
    /// Postgres with no `JAMJET_TEST_POSTGRES_URL` and no
    /// `JAMJET_REQUIRE_POSTGRES_TESTS`.
    async fn fresh(&self) -> Option<Fixture>;
}

/// Expands every case in [`cases`] into a test on `$factory`, one module
/// per case file.
#[macro_export]
macro_rules! conformance_suite {
    ($factory:expr) => {
        $crate::for_each_case!($crate::__conformance_module, $factory);
    };
}

/// Calls `$m!($factory, <module>, [<case>, ...]);` once per case module.
/// A case must be listed here to run.
#[macro_export]
macro_rules! for_each_case {
    ($m:path, $factory:expr) => {
        $m!(
            $factory,
            fixture,
            [
                the_second_handle_sees_what_the_first_wrote,
                item_row_follows_a_claim_and_a_park,
                a_forced_expiry_is_reclaimed_and_a_future_one_is_not,
                the_store_term_only_rises_and_reaches_the_fence,
                a_dead_lettered_item_names_its_run,
                artifact_rows_count_one_copy_per_tenant,
                a_token_moved_to_another_tenant_validates_there,
                a_reopened_store_keeps_what_was_written,
                a_scoped_handle_sees_only_its_tenant,
            ]
        );
    };
}

/// One module of tests, one per case, each on a fresh fixture.
#[doc(hidden)]
#[macro_export]
macro_rules! __conformance_module {
    ($factory:expr, $module:ident, [$($case:ident),* $(,)?]) => {
        mod $module {
            #[allow(unused_imports)]
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let Some(fx) = $crate::conformance::StoreFactory::fresh(&$factory).await else {
                        return $crate::skip(stringify!($case));
                    };
                    $crate::conformance::cases::$module::$case(&fx).await;
                }
            )*
        }
    };
}

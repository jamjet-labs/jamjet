//! The suite fails a store that would pass parts of it without running
//! them. The store here is memory labelled as Postgres: no fault point
//! belongs to it, and its hooks answer as memory's do, with no rows. That is
//! what a Postgres backend whose points and row hooks were left out would
//! look like to the cases.

use jamjet_testkit::conformance::{cases, Fixture, MemoryFactory, StoreFactory, StoreKind};
use std::future::Future;

const POSTGRES: [StoreKind; 2] = [StoreKind::Postgres, StoreKind::PostgresScoped];

/// Run `case` on a fresh memory fixture labelled `kind`, on a runtime of
/// its own, and return its panic message, or `None` if it passed.
fn panic_of<F, Fut>(kind: StoreKind, case: F) -> Option<String>
where
    F: FnOnce(Fixture) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let outcome = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async move {
            let mut fx = MemoryFactory.fresh().await.expect("a memory fixture");
            fx.kind = kind;
            case(fx).await
        })
    })
    .join();
    outcome.err().map(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

#[test]
fn the_faults_case_fails_a_store_that_owns_no_fault_point() {
    for kind in POSTGRES {
        let message = panic_of(kind, |fx| async move {
            cases::faults::an_error_at_any_step_leaves_no_partial_write(&fx).await
        })
        .unwrap_or_else(|| panic!("{kind:?}: the error case passed with no point to fail"));
        assert!(
            message.contains("no point in faults::POINTS belongs to this store"),
            "{kind:?}: {message}"
        );
    }
}

#[test]
fn the_hang_case_fails_a_postgres_store() {
    for kind in POSTGRES {
        let message = panic_of(kind, |fx| async move {
            cases::faults::a_hang_inside_a_transaction_blocks_only_its_run(&fx).await
        })
        .unwrap_or_else(|| panic!("{kind:?}: the hang case passed without a hang"));
        assert!(
            message.contains("the Postgres half of this case is not written"),
            "{kind:?}: {message}"
        );
    }
}

#[test]
fn the_fixture_fails_a_store_whose_row_hooks_do_not_answer() {
    for kind in POSTGRES {
        let message = panic_of(kind, |fx| async move {
            cases::fixture::the_row_hooks_answer_wherever_the_store_keeps_rows(&fx).await
        })
        .unwrap_or_else(|| panic!("{kind:?}: the row hooks case passed with no rows read"));
        assert!(message.contains("did not answer"), "{kind:?}: {message}");
    }
}

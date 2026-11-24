//! Tick state: the lease that lets one tick run at a time across every
//! engine on a store, and what the last tick left behind (where its walk
//! stopped, whether it got through, its report).
//!
//! The lease is taken, renewed and given back with conditional writes on the
//! store's clock. A second holder is told who holds it, a lapsed lease is
//! taken over, a renewed lease is not, and a holder that lost the lease can
//! neither renew it nor write its outcome. A store
//! has one tick row: a tenant's view takes and reads the same lease as the
//! unscoped store. `pg_ticks` checks that the lease follows the database's
//! clock on Postgres.

use crate::conformance::cases::support::race;
use crate::conformance::Fixture;
use jamjet_state::{StateBackend, StateBackendError, TickBegin, TickOutcome, TickStatus};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// A lease no case outlives.
const LIVE: Duration = Duration::from_secs(60);
/// A lease a case waits out.
const SHORT: Duration = Duration::from_millis(100);

fn outcome(cursor: Option<&str>, complete: bool, report: serde_json::Value) -> TickOutcome {
    TickOutcome {
        cursor: cursor.map(str::to_string),
        complete,
        report,
    }
}

async fn status(b: &dyn StateBackend) -> TickStatus {
    b.tick_status()
        .await
        .unwrap()
        .expect("the store has a tick row")
}

fn second(fx: &Fixture) -> Arc<dyn StateBackend> {
    fx.second.clone().unwrap_or_else(|| fx.backend.clone())
}

/// A fresh store has no tick row, and its first tick begins with nothing to
/// resume from. While that lease is live, another holder is told who holds
/// it and until when, through the other handle and through the unscoped
/// store alike, and nothing is written. Once a lease lapses unfinished, the
/// next holder takes it over and resumes where the last finished tick
/// stopped. A lease too long to add to the clock is refused.
pub async fn a_fresh_store_begins_and_a_held_lease_is_busy(fx: &Fixture) {
    let b = &*fx.backend;
    assert_eq!(b.tick_status().await.unwrap(), None, "a fresh store");

    let first = b.begin_tick("engine-a:1", LIVE).await.unwrap();
    assert_eq!(
        first,
        TickBegin::Began {
            cursor: None,
            last_finished_at: None
        }
    );
    let held = status(b).await;
    assert_eq!(held.holder.as_deref(), Some("engine-a:1"));
    let lease_until = held.lease_until.expect("a live lease has an end");
    let left = lease_until - b.store_now().await.unwrap();
    assert!(
        (55..=60).contains(&left.num_seconds()),
        "a 60 s lease has {left} left by the store's clock"
    );
    assert!(held.last_started_at.is_some(), "{held:?}");
    assert_eq!(
        (held.last_finished_at, held.last_complete, &held.cursor),
        (None, None, &None)
    );

    for (whose, other) in [
        ("second handle", second(fx)),
        ("unscoped", fx.hooks.unscoped()),
    ] {
        let busy = other.begin_tick("engine-b:1", LIVE).await.unwrap();
        assert_eq!(
            busy,
            TickBegin::Busy {
                holder: "engine-a:1".into(),
                lease_until
            },
            "{whose}"
        );
    }
    assert_eq!(status(b).await, held, "a busy begin wrote nothing");

    // The first tick stops part way. The next one's lease lapses before it
    // finishes, and a third takes over from where the first stopped.
    let stopped = outcome(Some("run-7"), false, json!({ "visited": 7 }));
    assert!(b.finish_tick("engine-a:1", &stopped).await.unwrap());
    let finished_at = status(b).await.last_finished_at.expect("finished");
    let resume = TickBegin::Began {
        cursor: Some("run-7".into()),
        last_finished_at: Some(finished_at),
    };
    assert_eq!(b.begin_tick("engine-b:1", SHORT).await.unwrap(), resume);
    tokio::time::sleep(SHORT * 3).await;
    assert_eq!(
        second(fx).begin_tick("engine-c:1", LIVE).await.unwrap(),
        resume,
        "the lapsed lease was not taken over"
    );
    let taken = status(b).await;
    assert_eq!(taken.holder.as_deref(), Some("engine-c:1"));

    let refused = b.begin_tick("engine-d:1", Duration::MAX).await;
    assert!(
        matches!(refused, Err(StateBackendError::Database(_))),
        "{refused:?}"
    );
    assert_eq!(status(b).await, taken, "a refused begin wrote nothing");
}

/// A finish lands only for the holder of the lease. One whose lease lapsed
/// and was taken over, one that never held it, and one that already gave it
/// back are told `false` and change nothing. The holder's finish frees the
/// lease and stores where the tick stopped, whether it got through and its
/// report, which `tick_status` reads back as written and the next tick
/// resumes from.
pub async fn finish_needs_the_lease_and_stores_the_outcome(fx: &Fixture) {
    let b = &*fx.backend;
    let other = second(fx);
    let nothing = outcome(Some("none"), true, json!({}));
    assert!(!b.finish_tick("engine-a:1", &nothing).await.unwrap());
    assert_eq!(
        b.tick_status().await.unwrap(),
        None,
        "a finish with no lease made a row"
    );

    assert!(matches!(
        b.begin_tick("engine-a:1", SHORT).await.unwrap(),
        TickBegin::Began { .. }
    ));
    tokio::time::sleep(SHORT * 3).await;
    assert!(matches!(
        other.begin_tick("engine-b:1", LIVE).await.unwrap(),
        TickBegin::Began { .. }
    ));
    let before = status(b).await;
    let lost = outcome(Some("lost"), true, json!({ "from": "a" }));
    for holder in ["engine-a:1", "engine-z:1"] {
        assert!(
            !b.finish_tick(holder, &lost).await.unwrap(),
            "{holder} finished a tick it does not hold"
        );
    }
    assert_eq!(status(b).await, before, "a refused finish changed the row");

    let report = json!({
        "runs": { "visited": 3, "advanced": 2 },
        "note": "a NUL \u{0} and non-ASCII text: ünïcødé"
    });
    let cursor = "0b7e3f0e-2f7a-4c55-9d43-5b1f5a9c2e11";
    assert!(other
        .finish_tick("engine-b:1", &outcome(Some(cursor), true, report.clone()))
        .await
        .unwrap());
    let after = status(b).await;
    assert_eq!(
        (&after.holder, after.lease_until),
        (&None, None),
        "the lease is free"
    );
    assert_eq!(after.cursor.as_deref(), Some(cursor));
    assert_eq!(after.last_complete, Some(true));
    assert_eq!(after.last_report, Some(report));
    assert_eq!(after.last_started_at, before.last_started_at);
    let finished_at = after.last_finished_at.expect("finished");
    assert!(
        Some(finished_at) >= before.last_started_at,
        "finished {finished_at} before it started {:?}",
        before.last_started_at
    );

    assert!(
        !other
            .finish_tick("engine-b:1", &outcome(None, false, json!({})))
            .await
            .unwrap(),
        "a tick finished twice"
    );
    assert_eq!(status(b).await, after, "the second finish changed the row");

    assert_eq!(
        b.begin_tick("engine-c:1", LIVE).await.unwrap(),
        TickBegin::Began {
            cursor: Some(cursor.into()),
            last_finished_at: Some(finished_at)
        }
    );
    assert!(b
        .finish_tick("engine-c:1", &outcome(None, false, json!({ "runs": 0 })))
        .await
        .unwrap());
    let cleared = status(b).await;
    assert_eq!(
        (&cleared.cursor, cleared.last_complete),
        (&None, Some(false)),
        "a finish with no cursor clears it"
    );
}

/// Two handles begin at once, as two engines' ticks would: one begins, the
/// other is told the winner holds the lease. Each round's winner finishes,
/// and the next round's winner resumes from that finish. Some rounds must
/// overlap, or one begin after the other would give the same answers.
pub async fn two_handles_begin_at_once_one_wins(fx: &Fixture) {
    const ROUNDS: usize = 200;
    let first = fx.backend.clone();
    let other = second(fx);
    let mut overlapped = 0;
    let mut last_cursor: Option<String> = None;
    for round in 0..ROUNDS {
        let (ha, hb) = (format!("engine-a:{round}"), format!("engine-b:{round}"));
        let (a, b) = (first.clone(), other.clone());
        let (x, y, at_once) = race(
            {
                let ha = ha.clone();
                async move { a.begin_tick(&ha, LIVE).await }
            },
            {
                let hb = hb.clone();
                async move { b.begin_tick(&hb, LIVE).await }
            },
        )
        .await;
        overlapped += usize::from(at_once);
        let (winner, cursor) = match (x.unwrap(), y.unwrap()) {
            (TickBegin::Began { cursor, .. }, TickBegin::Busy { holder, .. }) => {
                assert_eq!(holder, ha, "round {round}");
                (ha, cursor)
            }
            (TickBegin::Busy { holder, .. }, TickBegin::Began { cursor, .. }) => {
                assert_eq!(holder, hb, "round {round}");
                (hb, cursor)
            }
            other => panic!("round {round}: not one begin and one busy: {other:?}"),
        };
        assert_eq!(cursor, last_cursor, "round {round}: resumed from");
        assert_eq!(
            status(&*first).await.holder.as_deref(),
            Some(winner.as_str()),
            "round {round}"
        );
        let stopped = format!("run-{round}");
        assert!(
            first
                .finish_tick(
                    &winner,
                    &outcome(Some(&stopped), true, json!({ "round": round }))
                )
                .await
                .unwrap(),
            "round {round}: the winner could not finish"
        );
        last_cursor = Some(stopped);
    }
    assert!(
        overlapped > 0,
        "the two begins never overlapped in {ROUNDS} rounds, so no round raced"
    );
}

/// A renewal moves the end of the holder's lease, and only the holder's. A
/// short lease renewed before it lapses is still held after its first end:
/// another holder is told it is busy, through either handle. A renewal by a
/// holder the row does not name, with no row, after the lease was given
/// back, or after it lapsed and was taken over, is `false` and changes
/// nothing. A lease too long to add to the clock is refused. The row is the
/// store's, not a tenant's: the other handle renews the same lease.
pub async fn a_renewal_keeps_the_lease_for_its_holder_only(fx: &Fixture) {
    let b = &*fx.backend;
    let other = second(fx);
    assert!(
        !b.renew_tick("engine-a:1", LIVE).await.unwrap(),
        "a renewal with no row"
    );
    assert_eq!(
        b.tick_status().await.unwrap(),
        None,
        "a renewal with no lease made a row"
    );

    assert!(matches!(
        b.begin_tick("engine-a:1", SHORT).await.unwrap(),
        TickBegin::Began { .. }
    ));
    let begun = status(b).await;
    assert!(other.renew_tick("engine-a:1", LIVE).await.unwrap());
    let renewed = status(b).await;
    let left =
        renewed.lease_until.expect("a renewed lease has an end") - b.store_now().await.unwrap();
    assert!(
        (55..=60).contains(&left.num_seconds()),
        "a lease renewed for 60 s has {left} left by the store's clock"
    );
    assert_eq!(
        TickStatus {
            lease_until: begun.lease_until,
            ..renewed.clone()
        },
        begun,
        "a renewal moves the end of the lease and nothing else"
    );
    tokio::time::sleep(SHORT * 3).await;
    for (whose, handle) in [
        ("this handle", fx.backend.clone()),
        ("second handle", other.clone()),
    ] {
        assert_eq!(
            handle.begin_tick("engine-b:1", LIVE).await.unwrap(),
            TickBegin::Busy {
                holder: "engine-a:1".into(),
                lease_until: renewed.lease_until.unwrap()
            },
            "{whose}: the renewed lease was taken over at its first end"
        );
    }

    for holder in ["engine-b:1", "engine-a:2", ""] {
        assert!(
            !b.renew_tick(holder, LIVE).await.unwrap(),
            "{holder:?} renewed a lease it does not hold"
        );
    }
    let refused = b.renew_tick("engine-a:1", Duration::MAX).await;
    assert!(
        matches!(refused, Err(StateBackendError::Database(_))),
        "{refused:?}"
    );
    assert_eq!(
        status(b).await,
        renewed,
        "a refused renewal changed the row"
    );

    // Given back: nothing to renew.
    assert!(b
        .finish_tick("engine-a:1", &outcome(Some("run-3"), true, json!({})))
        .await
        .unwrap());
    let finished = status(b).await;
    assert!(!b.renew_tick("engine-a:1", LIVE).await.unwrap());
    assert_eq!(status(b).await, finished, "a renewal after the finish");

    // Lapsed and taken over: the old holder cannot take it back.
    assert!(matches!(
        b.begin_tick("engine-c:1", SHORT).await.unwrap(),
        TickBegin::Began { .. }
    ));
    tokio::time::sleep(SHORT * 3).await;
    assert!(matches!(
        other.begin_tick("engine-d:1", LIVE).await.unwrap(),
        TickBegin::Began { .. }
    ));
    let taken = status(b).await;
    assert!(
        !b.renew_tick("engine-c:1", LIVE).await.unwrap(),
        "a holder whose lease was taken over renewed it"
    );
    assert_eq!(
        status(b).await,
        taken,
        "the lost holder's renewal changed the row"
    );
    assert_eq!(taken.holder.as_deref(), Some("engine-d:1"));
}

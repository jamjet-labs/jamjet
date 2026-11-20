//! A run key names one run: a second start with the key writes nothing and
//! is told which run holds it, two starts at once make one run, and keys are
//! per tenant. A scoped lookup of another tenant's key finds nothing, as a
//! lookup of a key no one holds does, and a scoped keyed start whose item
//! names another tenant's run is `NotFound` and writes nothing.
//!
//! `jamjet-state`'s own `run_keys` tests cover the rest of the claim (a
//! failed start, `replaces`, the rows themselves) on memory and both SQLite
//! backends.

use crate::conformance::cases::support::{race, tenant_view, without_tenant_views};
use crate::conformance::Fixture;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    BackendResult, EventKind, KeyedStart, RunKeyClaim, StateBackend, StateBackendError, WorkItem,
    DEFAULT_TENANT,
};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-keys".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "order": 1 }),
        current_state: json!({ "order": 1 }),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

/// The start item of `item_run`, on a queue of its own.
fn item(item_run: &ExecutionId) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: item_run.clone(),
        node_id: "start".into(),
        queue_type: format!("keys-{}", item_run.0.simple()),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

fn claim(key: &str) -> RunKeyClaim {
    RunKeyClaim {
        run_key: key.into(),
        workflow_id: "wf-keys".into(),
        workflow_version: "1.0.0".into(),
        input_hash: "h1".into(),
        replaces: None,
    }
}

/// A keyed start of the run `id`, whose start item belongs to `item_run`.
async fn start_keyed(
    b: &dyn StateBackend,
    id: &ExecutionId,
    item_run: &ExecutionId,
    key: &str,
) -> BackendResult<KeyedStart> {
    let item = item(item_run);
    let scheduled = EventKind::NodeScheduled {
        node_id: item.node_id.clone(),
        queue_type: item.queue_type.clone(),
    };
    let started = EventKind::WorkflowStarted {
        workflow_id: "wf-keys".into(),
        workflow_version: "1.0.0".into(),
        initial_input: json!({ "order": 1 }),
        initiated_by: None,
        run_key: None,
    };
    b.start_execution_keyed(run(id), started, scheduled, item, claim(key))
        .await
}

/// A fresh run started under `key`.
async fn started(b: &dyn StateBackend, key: &str) -> ExecutionId {
    let id = ExecutionId::new();
    let out = start_keyed(b, &id, &id, key).await.unwrap();
    assert_eq!(out, KeyedStart::Started, "{key}");
    id
}

fn new_key() -> String {
    format!("order:{}", Uuid::new_v4().simple())
}

/// The second start with a key is told the first run holds it, and leaves
/// no run of its own. The key names the first run, with what it was claimed
/// with, and the log of that run carries the key.
pub async fn a_key_started_twice_makes_one_run(fx: &Fixture) {
    let b = &*fx.backend;
    let key = new_key();
    let first = started(b, &key).await;

    let second = ExecutionId::new();
    let out = start_keyed(b, &second, &second, &key).await.unwrap();
    let KeyedStart::Taken(held) = out else {
        panic!("the second start with {key} was not refused: {out:?}");
    };
    assert_eq!(held.execution_id, first);
    assert_eq!(
        (held.workflow_id.as_str(), held.workflow_version.as_str()),
        ("wf-keys", "1.0.0")
    );
    assert_eq!(held.input_hash, "h1");
    assert!(b.get_execution(&second).await.unwrap().is_none());

    let found = b.get_run_key(&key).await.unwrap().expect("the key");
    assert_eq!(found, held);
    let log = b.get_events(&first).await.unwrap();
    assert!(
        matches!(
            &log[0].kind,
            EventKind::WorkflowStarted { run_key: Some(k), .. } if *k == key
        ),
        "{:?}",
        log[0].kind
    );
    assert!(b.get_run_key(&new_key()).await.unwrap().is_none());
}

/// Two starts with one key, from the store's two handles at once: one is
/// stored, the other is told it lost to that run. Some rounds must overlap,
/// or one start after the other would give the same answers.
pub async fn two_starts_with_one_key_at_once_make_one_run(fx: &Fixture) {
    const ROUNDS: usize = 50;
    let first: Arc<dyn StateBackend> = fx.backend.clone();
    let second = fx.second.clone().unwrap_or_else(|| first.clone());
    let mut overlapped = 0;
    for round in 0..ROUNDS {
        let key = new_key();
        let (one, two) = (ExecutionId::new(), ExecutionId::new());
        let (a, b) = (first.clone(), second.clone());
        let (ka, kb) = (key.clone(), key.clone());
        let (ia, ib) = (one.clone(), two.clone());
        let (x, y, at_once) = race(
            async move { start_keyed(&*a, &ia, &ia, &ka).await },
            async move { start_keyed(&*b, &ib, &ib, &kb).await },
        )
        .await;
        overlapped += usize::from(at_once);
        let (x, y) = (x.unwrap(), y.unwrap());
        let winner = match (&x, &y) {
            (KeyedStart::Started, KeyedStart::Taken(held)) => {
                assert_eq!(held.execution_id, one, "round {round}");
                one.clone()
            }
            (KeyedStart::Taken(held), KeyedStart::Started) => {
                assert_eq!(held.execution_id, two, "round {round}");
                two.clone()
            }
            other => panic!("round {round}: not one start and one refusal: {other:?}"),
        };
        let loser = if winner == one { &two } else { &one };
        assert!(
            first.get_execution(loser).await.unwrap().is_none(),
            "round {round}: the refused start left a run"
        );
        let held = first.get_run_key(&key).await.unwrap().expect("the key");
        assert_eq!(held.execution_id, winner, "round {round}");
    }
    assert!(
        overlapped > 0,
        "the two starts never overlapped in {ROUNDS} rounds, so no round raced"
    );
}

/// `globex`'s key is not `acme`'s: `acme` finds nothing under it, as under
/// a key no one holds, and a start of its own with the key is its own run.
/// `globex`'s key still names `globex`'s run.
pub async fn scoped_lookup_of_another_tenants_key_finds_nothing(fx: &Fixture) {
    let (Some(acme), Some(globex)) = (
        tenant_view(fx, "acme").await,
        tenant_view(fx, "globex").await,
    ) else {
        return without_tenant_views(fx);
    };
    let key = new_key();
    let theirs = started(&*globex, &key).await;

    assert_eq!(acme.get_run_key(&key).await.unwrap(), None);
    assert_eq!(acme.get_run_key(&new_key()).await.unwrap(), None);

    let ours = started(&*acme, &key).await;
    assert_ne!(ours, theirs);
    assert_eq!(
        acme.get_run_key(&key)
            .await
            .unwrap()
            .map(|r| r.execution_id),
        Some(ours)
    );
    assert_eq!(
        globex
            .get_run_key(&key)
            .await
            .unwrap()
            .map(|r| r.execution_id),
        Some(theirs)
    );
}

/// `acme`'s keyed start whose item names `globex`'s run, or a run no one
/// holds, is `NotFound`, as its unkeyed start is: no run, no key, no row.
pub async fn scoped_keyed_start_naming_another_tenants_run_is_not_found(fx: &Fixture) {
    let (Some(acme), Some(globex)) = (
        tenant_view(fx, "acme").await,
        tenant_view(fx, "globex").await,
    ) else {
        return without_tenant_views(fx);
    };
    let globex_run = started(&*globex, &new_key()).await;
    let before = fx.hooks.every_row().await;
    for (whose, other) in [
        ("globex's run", globex_run),
        ("an unknown run", ExecutionId::new()),
    ] {
        let key = new_key();
        let id = ExecutionId::new();
        let out = start_keyed(&*acme, &id, &other, &key).await;
        assert!(
            matches!(out, Err(StateBackendError::NotFound(_))),
            "a keyed start with an item of {whose}: {out:?}"
        );
        assert_eq!(acme.get_run_key(&key).await.unwrap(), None, "{whose}");
        assert!(acme.get_execution(&id).await.unwrap().is_none(), "{whose}");
        assert!(
            fx.hooks.every_row().await == before,
            "{whose}: the refused start changed the store"
        );
    }
}

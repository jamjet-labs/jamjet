//! The fixture's own contract: every handle and hook reaches the same
//! store, and the hooks read and write what the backend does.

use crate::conformance::cases::support::{to_micros, with_nanos};
use crate::conformance::{Fixture, StoreKind, StoredJson};
use chrono::{Duration, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    Event, EventKind, Tenant, TenantId, TenantStatus, WorkItem, WorkItemId, DEFAULT_TENANT,
};
use serde_json::json;

const QUEUE: &str = "general";

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-fixture".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn tenant(id: &str) -> Tenant {
    let now = Utc::now();
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

/// A running run with one pending item on [`QUEUE`].
async fn run_with_an_item(fx: &Fixture) -> (ExecutionId, WorkItemId) {
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "step".into(),
        queue_type: QUEUE.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    let item_id = fx.backend.enqueue_work_item(item).await.unwrap();
    (id, item_id)
}

async fn claim(fx: &Fixture, item: WorkItemId) -> i64 {
    let claimed = fx
        .backend
        .claim_work_item("worker-a", &[QUEUE])
        .await
        .unwrap()
        .expect("the item is claimable");
    assert_eq!(claimed.id, item);
    claimed.lease_fence
}

pub async fn the_second_handle_sees_what_the_first_wrote(fx: &Fixture) {
    let second = fx
        .second
        .as_ref()
        .expect("every factory here has a second handle");
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    assert!(
        second.get_execution(&id).await.unwrap().is_some(),
        "{:?}: the second handle does not see the first one's run",
        fx.kind
    );
}

pub async fn item_row_follows_a_claim_and_a_park(fx: &Fixture) {
    let (_, item) = run_with_an_item(fx).await;
    let row = fx.hooks.item_row(item).await.expect("a row for the item");
    assert_eq!(row.status, "pending");
    assert_eq!((row.attempt, row.lease_epoch, row.lease_fence), (0, 0, 0));
    assert_eq!(row.worker_id, None);

    let fence = claim(fx, item).await;
    let row = fx.hooks.item_row(item).await.unwrap();
    assert_eq!(row.status, "claimed");
    assert_eq!(row.worker_id.as_deref(), Some("worker-a"));
    assert_eq!((row.lease_epoch, row.lease_fence), (1, fence));

    let retry_after = with_nanos(Utc::now() + Duration::hours(1));
    let parked = fx
        .backend
        .park_work_item(item, fence, &retry_after.to_rfc3339(), 2)
        .await
        .unwrap();
    assert!(parked);
    let row = fx.hooks.item_row(item).await.unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!((row.attempt, row.lease_epoch), (2, 2));
    assert_eq!(row.worker_id, None);
    let kept = match fx.kind {
        StoreKind::Memory => None,
        _ => Some(to_micros(retry_after)),
    };
    assert_eq!(row.retry_after.map(to_micros), kept);
}

pub async fn a_forced_expiry_is_reclaimed_and_a_future_one_is_not(fx: &Fixture) {
    let (_, item) = run_with_an_item(fx).await;
    claim(fx, item).await;

    fx.hooks
        .set_lease_expiry(item, Utc::now() + Duration::hours(1))
        .await;
    let swept = fx.backend.reclaim_expired_leases().await.unwrap();
    assert!(swept.retryable.is_empty() && swept.exhausted.is_empty());
    assert_eq!(fx.hooks.item_row(item).await.unwrap().status, "claimed");

    fx.hooks.force_lease_expired(item).await;
    let swept = fx.backend.reclaim_expired_leases().await.unwrap();
    let requeued: Vec<WorkItemId> = swept.retryable.iter().map(|i| i.id).collect();
    assert_eq!(requeued, vec![item]);
    assert_eq!(fx.hooks.item_row(item).await.unwrap().status, "pending");
}

pub async fn the_store_term_only_rises_and_reaches_the_fence(fx: &Fixture) {
    assert_eq!(fx.hooks.set_store_term_at_least(5).await, 5);
    assert_eq!(fx.hooks.set_store_term_at_least(3).await, 5);
    let (_, item) = run_with_an_item(fx).await;
    let fence = claim(fx, item).await;
    assert_eq!(fence >> 32, 5, "the fence carries the term above the epoch");
    assert_eq!(fence & 0xffff_ffff, 1);
}

pub async fn a_dead_lettered_item_names_its_run(fx: &Fixture) {
    let (run_id, item) = run_with_an_item(fx).await;
    assert_eq!(fx.hooks.dead_letter_execution(item).await, None);
    fx.backend
        .move_to_dead_letter(item, "gave up")
        .await
        .unwrap();
    assert_eq!(fx.hooks.dead_letter_execution(item).await, Some(run_id));
    assert_eq!(
        fx.hooks.item_row(item).await.map(|r| r.status),
        Some("dead_lettered".to_string())
    );
}

pub async fn artifact_rows_count_one_copy_per_tenant(fx: &Fixture) {
    let bytes = b"a fixture artifact";
    let stored = fx
        .backend
        .put_artifact(bytes, Some("text/plain"))
        .await
        .unwrap();
    fx.backend.put_artifact(bytes, None).await.unwrap();
    assert_eq!(fx.hooks.artifact_rows(&stored.hash, None).await, 1);
    assert_eq!(
        fx.hooks
            .artifact_rows(&stored.hash, Some(DEFAULT_TENANT))
            .await,
        1
    );
    assert_eq!(
        fx.hooks.artifact_rows(&stored.hash, Some("nobody")).await,
        0
    );
    let never_stored = jamjet_state::sha256_hex(b"never stored");
    assert_eq!(fx.hooks.artifact_rows(&never_stored, None).await, 0);
}

pub async fn a_token_moved_to_another_tenant_validates_there(fx: &Fixture) {
    let other = "fixture-other";
    match (&fx.scoped, fx.kind) {
        (Some(scoper), _) => scoper(&TenantId::from(other))
            .create_tenant(tenant(other))
            .await
            .unwrap(),
        // Memory has no tenant rows to make.
        (None, StoreKind::Memory) => {}
        // An unscoped SQL store makes tenants only through a scoped handle.
        (None, _) => return,
    }
    let (plaintext, token) = fx
        .backend
        .create_token("fixture-token", "operator")
        .await
        .unwrap();
    assert_eq!(token.tenant_id, DEFAULT_TENANT);
    fx.hooks.set_token_tenant("fixture-token", other).await;
    let seen = fx
        .backend
        .validate_token(&plaintext)
        .await
        .unwrap()
        .expect("the token is still valid");
    assert_eq!(seen.tenant_id, other);
}

pub async fn a_reopened_store_keeps_what_was_written(fx: &Fixture) {
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    match fx.hooks.reopen().await {
        None => assert_eq!(fx.kind, StoreKind::Memory, "only memory cannot reopen"),
        Some(again) => assert!(
            again.get_execution(&id).await.unwrap().is_some(),
            "{:?}: the reopened store lost the run",
            fx.kind
        ),
    }
}

/// The hooks that read a store's rows or write one answer on every store
/// that keeps rows, and answer with the store's own text. The payload,
/// event-compat, running-page and faults cases make their check only where
/// the hook answers, so a store whose hooks answered as memory's do would
/// pass those checks without running them. Memory keeps parsed values and
/// no rows; it is the one store let off, and there the hooks must say so.
pub async fn the_row_hooks_answer_wherever_the_store_keeps_rows(fx: &Fixture) {
    // The stores with no rows for these hooks to read or write.
    const NO_ROWS: &[StoreKind] = &[StoreKind::Memory];
    let keeps_rows = !NO_ROWS.contains(&fx.kind);

    let id = ExecutionId::new();
    let mut started = run(&id);
    let input = json!({ "input": "kept as written" });
    started.initial_input = input.clone();
    fx.backend.create_execution(started).await.unwrap();
    let payload = json!({ "payload": "kept as written" });
    let item = fx
        .backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: id.clone(),
            node_id: "step".into(),
            queue_type: QUEUE.into(),
            payload: payload.clone(),
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    let kind = EventKind::NodeSkipped {
        node_id: "step".into(),
        reason: "kept as written".into(),
    };
    let sequence = fx
        .backend
        .append_event(Event::new(id.clone(), 0, kind.clone()))
        .await
        .unwrap();
    let raw = EventKind::NodeSkipped {
        node_id: "raw".into(),
        reason: "written as text".into(),
    };
    let raw_text = serde_json::to_string(&raw).unwrap();
    let damaged = ExecutionId::new();
    fx.backend.create_execution(run(&damaged)).await.unwrap();

    let stored_input = fx.hooks.stored_json(StoredJson::RunInput(id.clone())).await;
    let stored_payload = fx.hooks.stored_json(StoredJson::ItemPayload(item)).await;
    let stored_event = fx
        .hooks
        .stored_json(StoredJson::Event {
            execution: id.clone(),
            sequence,
        })
        .await;
    let rows = fx.hooks.every_row().await;
    let inserted = fx
        .hooks
        .insert_event_json(&id, sequence + 1, &raw_text)
        .await;
    let made_unreadable = fx.hooks.make_execution_unreadable(&damaged).await;
    for (hook, answered) in [
        ("stored_json of a run's input", stored_input.is_some()),
        ("stored_json of an item's payload", stored_payload.is_some()),
        ("stored_json of an event", stored_event.is_some()),
        ("every_row", rows.is_some()),
        ("insert_event_json", inserted),
        ("make_execution_unreadable", made_unreadable),
    ] {
        if keeps_rows {
            assert!(
                answered,
                "{:?}: {hook} did not answer, so the cases that check what it reads \
                 or writes would pass here without checking",
                fx.kind
            );
        } else {
            assert!(
                !answered,
                "{:?}: {hook} answered on a store with no rows",
                fx.kind
            );
        }
    }
    if !keeps_rows {
        return;
    }

    let text = |value: &serde_json::Value| serde_json::to_string(value).unwrap();
    assert_eq!(
        stored_input,
        Some(text(&input)),
        "{:?}: the run's input",
        fx.kind
    );
    assert_eq!(
        stored_payload,
        Some(text(&payload)),
        "{:?}: the item's payload",
        fx.kind
    );
    assert_eq!(
        stored_event,
        Some(serde_json::to_string(&kind).unwrap()),
        "{:?}: the event",
        fx.kind
    );
    let rows = rows.unwrap();
    let holds = |table: &str, needle: &str| {
        rows.get(table)
            .is_some_and(|found| found.iter().any(|row| row.contains(needle)))
    };
    assert!(
        holds("workflow_executions", &id.0.to_string()),
        "{:?}: every_row lacks the run",
        fx.kind
    );
    assert!(
        holds("work_items", &item.to_string()),
        "{:?}: every_row lacks the item",
        fx.kind
    );
    assert!(
        holds("events", "kept as written"),
        "{:?}: every_row lacks the event",
        fx.kind
    );
    let events = fx.backend.get_events(&id).await.unwrap();
    let read: Vec<(i64, String)> = events
        .iter()
        .map(|e| (e.sequence, serde_json::to_string(&e.kind).unwrap()))
        .collect();
    assert_eq!(
        read,
        vec![
            (sequence, serde_json::to_string(&kind).unwrap()),
            (sequence + 1, raw_text)
        ],
        "{:?}: the event insert_event_json wrote",
        fx.kind
    );
    assert!(
        fx.backend.get_execution(&damaged).await.is_err(),
        "{:?}: the run make_execution_unreadable damaged still reads back",
        fx.kind
    );
}

pub async fn a_scoped_handle_sees_only_its_tenant(fx: &Fixture) {
    let Some(scoper) = &fx.scoped else {
        assert!(!fx.kind.is_scoped(), "{:?} has no scoper", fx.kind);
        return;
    };
    for t in ["fixture-a", "fixture-b"] {
        scoper(&TenantId::from(t))
            .create_tenant(tenant(t))
            .await
            .unwrap();
    }
    let a = scoper(&TenantId::from("fixture-a"));
    let b = scoper(&TenantId::from("fixture-b"));
    let id = ExecutionId::new();
    a.create_execution(run(&id)).await.unwrap();
    assert!(a.get_execution(&id).await.unwrap().is_some());
    assert!(b.get_execution(&id).await.unwrap().is_none());
    // `backend`, `second` and a reopened handle are the default tenant's views.
    assert!(fx.backend.get_execution(&id).await.unwrap().is_none());
    let second = fx.second.as_ref().unwrap();
    assert!(second.get_execution(&id).await.unwrap().is_none());
    if let Some(again) = fx.hooks.reopen().await {
        assert!(again.get_execution(&id).await.unwrap().is_none());
    }
}

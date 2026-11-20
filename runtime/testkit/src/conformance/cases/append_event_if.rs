//! `append_event_if`: the compare-and-set the approval loop resolves a request
//! under. The condition is read in the same step as the append, a terminal or
//! unknown execution is refused, and a refused append writes nothing, on
//! every backend.

use crate::conformance::cases::support::{finding, in_tenant, tenant_view, without_tenant_views};
use crate::conformance::{Fixture, StoreKind};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::approvals::still_pending;
use jamjet_state::{ConditionalAppend, Event, EventKind, StateBackendError, TenantId};
use serde_json::json;

fn exec(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
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

fn request() -> EventKind {
    EventKind::ToolApprovalRequired {
        node_id: "a".into(),
        tool_name: "t".into(),
        approver: "human".into(),
        context: json!({}),
    }
}

fn expiry(request_sequence: i64) -> EventKind {
    EventKind::ApprovalExpired {
        node_id: "a".into(),
        request_sequence,
        expires_at: chrono::Utc::now(),
        principal: "system:deadline".into(),
    }
}

pub async fn an_append_lands_only_while_its_condition_holds(fx: &Fixture) {
    // The scoped stores run in a tenant other than the default.
    let b = in_tenant(fx, "acme").await;
    let name = format!("{:?}", fx.kind);
    let id = ExecutionId::new();
    b.create_execution(exec(&id)).await.unwrap();
    let seq = b
        .append_event(Event::new(id.clone(), 0, request()))
        .await
        .unwrap();
    let cond = move |log: &[Event]| still_pending(log, "a", seq);

    let first = b
        .append_event_if(Event::new(id.clone(), 0, expiry(seq)), &cond)
        .await
        .unwrap();
    assert!(
        matches!(first, ConditionalAppend::Appended(s) if s == seq + 1),
        "{name}: {first:?}"
    );
    // The same append again: the request is no longer pending.
    let second = b
        .append_event_if(Event::new(id.clone(), 0, expiry(seq)), &cond)
        .await
        .unwrap();
    assert_eq!(second, ConditionalAppend::Refused, "{name}");
    let events = b.get_events(&id).await.unwrap();
    assert_eq!(events.len(), 2, "{name}: a refused append writes nothing");
}

pub async fn a_terminal_or_unknown_execution_is_refused(fx: &Fixture) {
    // The scoped stores run in a tenant other than the default.
    let b = in_tenant(fx, "acme").await;
    let name = format!("{:?}", fx.kind);
    let id = ExecutionId::new();
    b.create_execution(exec(&id)).await.unwrap();
    b.append_event(Event::new(id.clone(), 0, request()))
        .await
        .unwrap();
    b.finish_execution(
        Event::new(id.clone(), 0, EventKind::WorkflowCancelled { reason: None }),
        WorkflowStatus::Cancelled,
    )
    .await
    .unwrap();
    let always = |_: &[Event]| true;
    let out = b
        .append_event_if(Event::new(id.clone(), 0, expiry(1)), &always)
        .await
        .unwrap();
    assert_eq!(
        out,
        ConditionalAppend::ExecutionTerminal(WorkflowStatus::Cancelled),
        "{name}"
    );
    assert_eq!(b.get_events(&id).await.unwrap().len(), 2, "{name}");

    let unknown = b
        .append_event_if(Event::new(ExecutionId::new(), 0, expiry(1)), &always)
        .await;
    assert!(
        matches!(unknown, Err(StateBackendError::NotFound(_))),
        "{name}: {unknown:?}"
    );
}

/// The condition sees the log as it is at the append, not as the caller last
/// read it: a decision recorded after the caller's read refuses the append.
pub async fn the_condition_sees_writes_made_after_the_callers_read(fx: &Fixture) {
    // The scoped stores run in a tenant other than the default.
    let b = in_tenant(fx, "acme").await;
    let name = format!("{:?}", fx.kind);
    let id = ExecutionId::new();
    b.create_execution(exec(&id)).await.unwrap();
    let seq = b
        .append_event(Event::new(id.clone(), 0, request()))
        .await
        .unwrap();
    let read = b.get_events(&id).await.unwrap();
    assert!(still_pending(&read, "a", seq));
    // Something resolves the request after that read.
    b.append_event(Event::new(id.clone(), 0, expiry(seq)))
        .await
        .unwrap();
    let cond = move |log: &[Event]| still_pending(log, "a", seq);
    let out = b
        .append_event_if(Event::new(id.clone(), 0, expiry(seq)), &cond)
        .await
        .unwrap();
    assert_eq!(out, ConditionalAppend::Refused, "{name}");
}

/// `execution_tenant` reads the run's tenant from its execution row, and a
/// tenant-scoped view answers only for its own runs.
pub async fn execution_tenant_is_the_rows_tenant_and_is_scoped(fx: &Fixture) {
    let (Some(acme), Some(other)) = (
        tenant_view(fx, "acme").await,
        tenant_view(fx, "other").await,
    ) else {
        return without_tenant_views(fx);
    };
    let base = fx.hooks.unscoped();
    let id = ExecutionId::new();
    acme.create_execution(exec(&id)).await.unwrap();
    assert_eq!(
        base.execution_tenant(&id).await.unwrap(),
        Some(TenantId::from("acme".to_string()))
    );
    assert_eq!(
        acme.execution_tenant(&id).await.unwrap(),
        Some(TenantId::from("acme".to_string()))
    );
    assert_eq!(other.execution_tenant(&id).await.unwrap(), None);
    assert_eq!(
        base.execution_tenant(&ExecutionId::new()).await.unwrap(),
        None
    );
}

/// Review M-5: an event's position in its log and
/// its sequence agree under concurrent appends. The sequence used to be taken
/// before the log's lock, so two appends could push out of order.
pub async fn log_order_matches_sequence_order_under_concurrency(fx: &Fixture) {
    // It ran on memory only before the suite. On SQLite, eight writers on
    // one log answer `database is locked` once a wait passes the 5 s busy
    // timeout: `append_event_if` reads the whole log under the write lock,
    // so each append costs more as the log grows.
    if finding(
        fx,
        &[StoreKind::Sqlite, StoreKind::SqliteScoped],
        "16,000 appends by eight writers on one log pass the SQLite busy timeout",
    ) {
        return;
    }
    let b = fx.backend.clone();
    let id = ExecutionId::new();
    b.create_execution(exec(&id)).await.unwrap();
    let mut tasks = Vec::new();
    for t in 0..8 {
        let b = b.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..2000 {
                if t % 2 == 0 {
                    b.append_event(Event::new(id.clone(), 0, request()))
                        .await
                        .unwrap();
                } else {
                    let always = |_: &[Event]| true;
                    b.append_event_if(Event::new(id.clone(), 0, request()), &always)
                        .await
                        .unwrap();
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let log = b.get_events(&id).await.unwrap();
    assert_eq!(log.len(), 16_000);
    let out_of_order = log
        .windows(2)
        .filter(|w| w[0].sequence >= w[1].sequence)
        .count();
    assert_eq!(
        out_of_order, 0,
        "{out_of_order} adjacent pairs out of sequence order"
    );
}

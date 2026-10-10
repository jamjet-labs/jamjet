//! `append_event_if`: the compare-and-set the approval loop resolves a request
//! under. The condition is read in the same step as the append, a terminal or
//! unknown execution is refused, and a refused append writes nothing, on
//! every backend.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::approvals::still_pending;
use jamjet_state::{
    ConditionalAppend, Event, EventKind, InMemoryBackend, SqliteBackend, StateBackend,
    StateBackendError, Tenant, TenantId, TenantStatus,
};
use serde_json::json;
use std::sync::Arc;

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

async fn backends() -> Vec<(&'static str, Arc<dyn StateBackend>)> {
    let db = |n: &str| {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "jamjet_cas_{n}_{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        format!("sqlite://{}", p.display())
    };
    let sqlite = SqliteBackend::open(&db("a")).await.unwrap();
    let base = SqliteBackend::open(&db("b")).await.unwrap();
    let scoped = base.for_tenant(TenantId::from("acme".to_string()));
    let now = chrono::Utc::now();
    scoped
        .create_tenant(Tenant {
            id: TenantId::from("acme".to_string()),
            name: "acme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    vec![
        ("memory", Arc::new(InMemoryBackend::new())),
        ("sqlite", Arc::new(sqlite)),
        ("tenant-scoped sqlite", Arc::new(scoped)),
    ]
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

#[tokio::test]
async fn an_append_lands_only_while_its_condition_holds() {
    for (name, b) in backends().await {
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
}

#[tokio::test]
async fn a_terminal_or_unknown_execution_is_refused() {
    for (name, b) in backends().await {
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
}

/// The condition sees the log as it is at the append, not as the caller last
/// read it: a decision recorded after the caller's read refuses the append.
#[tokio::test]
async fn the_condition_sees_writes_made_after_the_callers_read() {
    for (name, b) in backends().await {
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
}

/// `execution_tenant` reads the run's tenant from its execution row, and a
/// tenant-scoped view answers only for its own runs.
#[tokio::test]
async fn execution_tenant_is_the_rows_tenant_and_is_scoped() {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "jamjet_exec_tenant_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let base = SqliteBackend::open(&format!("sqlite://{}", p.display()))
        .await
        .unwrap();
    let now = chrono::Utc::now();
    for t in ["acme", "other"] {
        base.for_tenant(TenantId::from(t.to_string()))
            .create_tenant(Tenant {
                id: TenantId::from(t.to_string()),
                name: t.into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }
    let acme = base.for_tenant(TenantId::from("acme".to_string()));
    let other = base.for_tenant(TenantId::from("other".to_string()));
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

/// Review M-5: on the in-memory backend an event's position in its log and
/// its sequence agree under concurrent appends. The sequence used to be taken
/// before the log's lock, so two appends could push out of order.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn memory_log_order_matches_sequence_order_under_concurrency() {
    let b = Arc::new(InMemoryBackend::new());
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

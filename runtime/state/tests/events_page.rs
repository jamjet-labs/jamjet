//! `get_events_page` with a byte budget: the page stops at the event that
//! reaches it, in the query on SQLite, so a page of large events is never
//! read past its budget. Small events first and large ones after used to be
//! read far past it.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{Event, EventKind, InMemoryBackend, SqliteBackend, StateBackend, TenantId};
use serde_json::json;

fn execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1".into(),
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

fn skipped(i: usize, len: usize) -> EventKind {
    EventKind::NodeSkipped {
        node_id: format!("n{i}"),
        reason: "x".repeat(len),
    }
}

#[tokio::test]
async fn a_page_stops_at_its_budget_on_every_backend() {
    let sqlite = SqliteBackend::open("sqlite::memory:").await.unwrap();
    let scoped = SqliteBackend::open("sqlite::memory:")
        .await
        .unwrap()
        .for_tenant(TenantId::default());
    let memory = InMemoryBackend::new();
    let backends: [(&str, &dyn StateBackend); 3] = [
        ("sqlite", &sqlite),
        ("tenant_scoped", &scoped),
        ("memory", &memory),
    ];
    for (name, db) in backends {
        let eid = ExecutionId::new();
        db.create_execution(execution(&eid)).await.unwrap();
        // A hundred small events, then a thousand of 8 KiB.
        for i in 0..1100 {
            let len = if i < 100 { 10 } else { 8 * 1024 };
            db.append_event(Event::new(eid.clone(), 0, skipped(i, len)))
                .await
                .unwrap();
        }
        let budget = 256 * 1024;
        let page = db.get_events_page(&eid, 0, 10_000, budget).await.unwrap();
        let sizes: Vec<usize> = page
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .collect();
        let read: usize = sizes.iter().sum();
        let largest = sizes.iter().copied().max().unwrap();
        assert!(
            read <= budget + largest,
            "{name}: read {read} bytes for a {budget}-byte page"
        );
        // SQLite estimates each row's size a little high, so it may stop a
        // little before the budget; not a whole event before it.
        assert!(
            read + 2 * largest >= budget,
            "{name}: stopped early at {read}"
        );
        // At least one event, however large.
        let one = db.get_events_page(&eid, 0, 10_000, 1).await.unwrap();
        assert_eq!(one.len(), 1, "{name}");
        // The limit still holds.
        let few = db.get_events_page(&eid, 0, 5, usize::MAX).await.unwrap();
        assert_eq!(few.len(), 5, "{name}");
    }
}

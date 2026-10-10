//! A run's start used to be four writes: the execution row, `WorkflowStarted`,
//! the start node's `NodeScheduled`, its work item. A scheduler tick landing
//! after the row and before `NodeScheduled` saw a running execution whose
//! start node (a root) had not been scheduled, and scheduled it too: the start
//! node ran twice (seen as ["a","a"] under load). The start is now one write.
//!
//! Each test runs a real scheduler tick after EVERY write `start_run` makes,
//! which is the worst interleaving a concurrent scheduler can produce, on the
//! in-memory backend, SQLite, and the tenant-scoped SQLite view.
mod support;

use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::TenantId;
use serde_json::json;
use std::sync::Arc;
use support::hooked::{AfterWrite, HookedBackend, Write};

/// Runs one scheduler tick after each write.
struct TickAfterWrite {
    scheduler: Scheduler,
}

#[async_trait::async_trait]
impl AfterWrite for TickAfterWrite {
    async fn after(&self, _write: Write) {
        self.scheduler.tick().await.expect("scheduler tick");
    }
}

fn two_node_ir() -> serde_json::Value {
    let mut ir = support::python_fn_start_ir("wf-start-race");
    ir["nodes"]["b"] = json!({
        "id": "b",
        "kind": { "type": "python_fn", "module": "myapp.tools", "function": "after", "output_schema": "" },
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
    });
    ir["edges"] = json!([
        { "from": "py_start", "to": "b", "condition": null },
        { "from": "b", "to": "end", "condition": null }
    ]);
    ir
}

/// (backend name, the backend the scheduler reads, the backend `start_run`
/// writes through).
async fn cases() -> Vec<(&'static str, Arc<dyn StateBackend>, Arc<dyn StateBackend>)> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_start_race_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let url = format!("sqlite://{}", path.display());
    let sqlite = Arc::new(jamjet_state::SqliteBackend::open(&url).await.unwrap());
    let scoped: Arc<dyn StateBackend> =
        Arc::new(sqlite.for_tenant(TenantId::from("acme".to_string())));
    let now = chrono::Utc::now();
    scoped
        .create_tenant(jamjet_state::Tenant {
            id: TenantId::from("acme".to_string()),
            name: "acme".into(),
            status: jamjet_state::TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let mut path2 = std::env::temp_dir();
    path2.push(format!(
        "jamjet_start_race_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let url2 = format!("sqlite://{}", path2.display());
    let sqlite2: Arc<dyn StateBackend> =
        Arc::new(jamjet_state::SqliteBackend::open(&url2).await.unwrap());

    let memory: Arc<dyn StateBackend> = Arc::new(jamjet_state::InMemoryBackend::new());
    vec![
        ("memory", memory.clone(), memory),
        ("sqlite", sqlite2.clone(), sqlite2),
        ("tenant-scoped sqlite", sqlite, scoped),
    ]
}

#[tokio::test]
async fn a_scheduler_tick_between_the_start_writes_does_not_schedule_the_start_node_twice() {
    let mut failures = Vec::new();
    for (name, base, writer) in cases().await {
        let hooked: Arc<dyn StateBackend> = Arc::new(HookedBackend::new(
            writer.clone(),
            Arc::new(TickAfterWrite {
                scheduler: Scheduler::new(base.clone()),
            }),
        ));
        let state = support::make_state(hooked);
        let exec = support::register_and_start(&state, two_node_ir(), json!({})).await;
        let execution_id = jamjet_core::workflow::ExecutionId(
            uuid::Uuid::parse_str(exec.trim_start_matches("exec_")).unwrap(),
        );

        // One more tick after the start has fully landed.
        Scheduler::new(base.clone()).tick().await.unwrap();

        // Read through the unscoped backend: the scheduler writes under the
        // default tenant, which a scoped read would hide.
        let events = base.get_events(&execution_id).await.unwrap();
        let scheduled: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeScheduled { node_id, .. } => Some(node_id.clone()),
                _ => None,
            })
            .collect();
        if scheduled != ["py_start"] {
            failures.push(format!("{name}: start node scheduled as {scheduled:?}"));
        }
        let started = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::WorkflowStarted { .. }))
            .count();
        if started != 1 || !matches!(events[0].kind, EventKind::WorkflowStarted { .. }) {
            failures.push(format!(
                "{name}: WorkflowStarted must be the one first event, got {} of them, first {:?}",
                started, events[0].kind
            ));
        }

        // Exactly one claimable item for the start node.
        let mut items = 0;
        while let Some(item) = base.claim_work_item("w", &["python_tool"]).await.unwrap() {
            assert_eq!(item.execution_id, execution_id, "{name}");
            items += 1;
        }
        if items != 1 {
            failures.push(format!("{name}: {items} start items enqueued"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

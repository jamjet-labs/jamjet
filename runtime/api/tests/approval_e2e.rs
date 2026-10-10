//! End-to-end approval (HITL) regression tests.
//!
//! A node gated by `require_approval_for` must park (not run, not dead-letter),
//! resume and complete after an approval event, and fail the workflow after a
//! rejection. Guards the worker hold path + scheduler resume fold.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::{Event, SqliteBackend, DEFAULT_TENANT};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Serializes the tests in this binary. Each test spawns a scheduler (25ms
/// ticks) plus a 5-worker pool, which is a lot of concurrent machinery per
/// test; running three such stacks at once adds CPU/IO noise on top. The
/// historical flake here (worker stranded its claimed item until lease
/// reclaim) was a real bug: deferred SQLite transactions upgrading read->write
/// bypass the busy handler (fixed with BEGIN IMMEDIATE in the state backend).
/// The mutex stays as belt-and-braces so timing assertions are load-independent.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Install a test-writer tracing subscriber so worker/scheduler `warn!`s
/// (e.g. "Worker error: …") surface in failing-test output instead of being
/// silently dropped. Controlled by RUST_LOG; no-op if already installed.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init()
        .ok();
}

/// Single tool node gated by a node-level require_approval policy, edge to end.
///
/// NodeKind::Tool requires: tool_ref (String), input_mapping ({}), output_schema (String).
/// The policy engine's EvaluationContext::from_node_kind reads tool_ref as the tool_name,
/// which is then matched against require_approval_for: ["payments.*"].
fn gated_ir() -> serde_json::Value {
    serde_json::json!({
        "workflow_id": "e2e-approval",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gated",
        "nodes": {
            "gated": {
                "id": "gated",
                "kind": {
                    "type": "tool",
                    "tool_ref": "payments.transfer",
                    "input_mapping": {},
                    "output_schema": ""
                },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": {
                    "blocked_tools": [],
                    "require_approval_for": ["payments.*"],
                    "model_allowlist": []
                }
            }
        },
        "edges": [ { "from": "gated", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

/// The start item's payload exactly as `POST /executions` and the scheduler
/// build it. An approval binds the dispatched payload, so a hand-made payload
/// would not be the one the scheduler re-dispatches after the approval.
fn engine_payload() -> serde_json::Value {
    let ir: jamjet_ir::WorkflowIr = serde_json::from_value(gated_ir()).unwrap();
    jamjet_scheduler::build_work_item_payload(
        &ir,
        "gated",
        &ir.nodes["gated"],
        &serde_json::Map::new(),
        "e2e-approval",
        "0.1.0",
    )
}

struct Harness {
    backend: Arc<dyn StateBackend>,
    execution_id: ExecutionId,
    db_path: std::path::PathBuf,
    sched_handle: tokio::task::JoinHandle<()>,
    worker_handles: Vec<tokio::task::JoinHandle<()>>,
}

async fn start() -> Harness {
    let db_path = std::env::temp_dir().join(format!("jjtest-{}.db", Uuid::new_v4()));
    let url = format!("sqlite://{}", db_path.display());
    let backend: Arc<dyn StateBackend> = Arc::new(
        SqliteBackend::open(&url)
            .await
            .expect("open sqlite backend"),
    );

    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: "e2e-approval".into(),
            version: "0.1.0".into(),
            ir: gated_ir(),
            created_at: chrono::Utc::now(),
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("store_workflow");

    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "e2e-approval".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .expect("create_execution");
    backend
        .append_event(Event::new(
            execution_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "e2e-approval".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
                initiated_by: None,
            },
        ))
        .await
        .expect("append WorkflowStarted");
    backend
        .append_event(Event::new(
            execution_id.clone(),
            2,
            EventKind::NodeScheduled {
                node_id: "gated".into(),
                queue_type: "general".into(),
            },
        ))
        .await
        .expect("append NodeScheduled");
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: execution_id.clone(),
            node_id: "gated".into(),
            queue_type: "general".into(),
            payload: engine_payload(),
            attempt: 0,
            max_attempts: 3,
            created_at: now,
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("enqueue_work_item");

    let scheduler = jamjet_scheduler::Scheduler::new(backend.clone())
        .with_poll_interval(Duration::from_millis(25));
    let sched_handle = tokio::spawn(async move { scheduler.run().await });
    let worker_handles = jamjet_worker::default_pool(backend.clone()).spawn();

    Harness {
        backend,
        execution_id,
        db_path,
        sched_handle,
        worker_handles,
    }
}

impl Harness {
    async fn wait_for<F: Fn(&[Event]) -> bool>(&self, pred: F, secs: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let events = self
                .backend
                .get_events(&self.execution_id)
                .await
                .unwrap_or_default();
            if pred(&events) {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                eprintln!(
                    "WAIT TIMEOUT — events: {:#?}",
                    events
                        .iter()
                        .map(|e| format!("{:?}", e.kind))
                        .collect::<Vec<_>>()
                );
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Decide through the real submission path (`submit_approval`, what the
    /// REST route and the MCP bridge call), so the recorded approval carries
    /// the digest binding exactly as in production.
    async fn approve(&self, decision: ApprovalDecision, comment: Option<&str>) {
        jamjet_api::approvals::submit_approval(
            &self.backend,
            &self.execution_id,
            jamjet_api::approvals::ApprovalSubmission {
                node_id: Some("gated".into()),
                user_id: "e2e-tester".into(),
                principal: None,
                decision,
                comment: comment.map(String::from),
                state_patch: None,
                action_digest: None,
                // The harness drives the engine with no auth layer.
                dev_mode: true,
            },
            Default::default(),
        )
        .await
        .expect("submit_approval");
    }

    /// An approval as a pre-digest writer would have appended it: no
    /// `approved_digest`, so it does not cover any digest-bound action.
    async fn approve_legacy(&self) {
        let seq = self
            .backend
            .latest_sequence(&self.execution_id)
            .await
            .unwrap()
            + 1;
        let kind: EventKind = serde_json::from_value(serde_json::json!({
            "type": "approval_received",
            "node_id": "gated",
            "user_id": "legacy",
            "decision": "approved",
            "comment": null,
            "state_patch": null,
        }))
        .unwrap();
        self.backend
            .append_event(Event::new(self.execution_id.clone(), seq, kind))
            .await
            .expect("append legacy ApprovalReceived");
    }

    async fn shutdown(self) {
        self.sched_handle.abort();
        for h in self.worker_handles {
            h.abort();
        }
        let _ = std::fs::remove_file(&self.db_path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gated_node_parks_without_dead_lettering() {
    let _serial = SERIAL.lock().await;
    init_tracing();
    let h = start().await;

    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. })),
            30
        )
        .await,
        "ToolApprovalRequired should be emitted"
    );
    // Let scheduler + workers churn; the hold must stay stable.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Simulate a lease-expiry reclaim: a second work item for the held node
    // (what reclaim_expired_leases would re-enqueue after a worker crash).
    // The worker must settle it without duplicating the hold event.
    h.backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: h.execution_id.clone(),
            node_id: "gated".into(),
            queue_type: "general".into(),
            payload: engine_payload(),
            attempt: 1,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("enqueue duplicate work item");
    tokio::time::sleep(Duration::from_secs(2)).await;

    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    let holds = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
        .count();
    assert_eq!(
        holds, 1,
        "exactly one hold event — no retry loop, no duplicate from reclaimed items"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e.kind,
            EventKind::NodeFailed { .. } | EventKind::NodeCompleted { .. }
        )),
        "held node neither ran nor failed"
    );
    let exec = h
        .backend
        .get_execution(&h.execution_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        exec.status,
        WorkflowStatus::Running,
        "execution stays running while parked"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_node_resumes_and_completes() {
    let _serial = SERIAL.lock().await;
    init_tracing();
    let h = start().await;

    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. })),
            30
        )
        .await
    );
    h.approve(ApprovalDecision::Approved, None).await;

    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::WorkflowCompleted { .. })),
            30
        )
        .await,
        "approved workflow should run to completion"
    );
    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    assert!(
        events.iter().any(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "gated")
        ),
        "the gated node actually executed after approval"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_node_fails_workflow_with_reason() {
    let _serial = SERIAL.lock().await;
    init_tracing();
    let h = start().await;

    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. })),
            30
        )
        .await
    );
    h.approve(ApprovalDecision::Rejected, Some("not in business hours"))
        .await;

    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })),
            30
        )
        .await,
        "rejected workflow should fail"
    );
    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    let reason = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::NodeFailed { node_id, error, .. } if node_id == "gated" => {
                Some(error.clone())
            }
            _ => None,
        })
        .expect("NodeFailed for the gated node");
    assert!(
        reason.contains("e2e-tester") && reason.contains("not in business hours"),
        "NodeFailed reason should mention reviewer and comment; got: {reason}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "gated")
        ),
        "rejected node never executed"
    );

    h.shutdown().await;
}

fn holds(ev: &[Event]) -> usize {
    ev.iter()
        .filter(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
        .count()
}

fn gated_completions(ev: &[Event]) -> usize {
    ev.iter()
        .filter(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "gated"),
        )
        .count()
}

/// An approval with no digest (written before digests existed) does not
/// release a digest-bound node: it asks again, and only a bound approval of
/// the new request lets it run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_approval_re_requests_and_a_bound_one_then_runs() {
    let _serial = SERIAL.lock().await;
    init_tracing();
    let h = start().await;

    assert!(h.wait_for(|ev| holds(ev) == 1, 30).await);
    h.approve_legacy().await;

    assert!(
        h.wait_for(|ev| holds(ev) == 2, 30).await,
        "the legacy approval must trigger a fresh request"
    );
    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    assert_eq!(
        gated_completions(&events),
        0,
        "the legacy approval fired nothing"
    );

    h.approve(ApprovalDecision::Approved, None).await;
    assert!(
        h.wait_for(
            |ev| ev
                .iter()
                .any(|e| matches!(e.kind, EventKind::WorkflowCompleted { .. })),
            30
        )
        .await
    );
    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    assert_eq!(gated_completions(&events), 1);

    h.shutdown().await;
}

/// The run's state changes between the decision and the resume, but not in a
/// way that reaches this node: a tool node is dispatched without the run's
/// state, so its payload, and therefore the action, is unchanged. The approval
/// still covers it: it fires once, with no second request. (A change that does
/// reach a node's input voids the approval: `approval_binds_payload.rs`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_change_outside_the_payload_keeps_the_approval_end_to_end() {
    let _serial = SERIAL.lock().await;
    init_tracing();
    let h = start().await;

    assert!(h.wait_for(|ev| holds(ev) == 1, 30).await);
    h.approve(ApprovalDecision::Approved, None).await;
    let seq = h.backend.latest_sequence(&h.execution_id).await.unwrap() + 1;
    h.backend
        .append_event(Event::new(
            h.execution_id.clone(),
            seq,
            EventKind::NodeCompleted {
                node_id: "sibling".into(),
                output: serde_json::json!({}),
                state_patch: serde_json::json!({ "amount": 1_000_000 }),
                duration_ms: 0,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        ))
        .await
        .unwrap();

    assert!(
        h.wait_for(|ev| gated_completions(ev) == 1, 30).await,
        "the approved, unchanged action fires"
    );
    let events = h.backend.get_events(&h.execution_id).await.unwrap();
    assert_eq!(holds(&events), 1, "no second request");

    h.shutdown().await;
}

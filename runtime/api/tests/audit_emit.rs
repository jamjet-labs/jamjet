//! The live approval append path emits signed, hash-chained audit entries.
//!
//! T3-4 Part A added the `AuditEnricher` (sign + hash-chain) but left it
//! dormant — plumbed onto `AppState`, never called. This test drives a real
//! `POST /executions/:id/approve` through the HTTP router with a recording
//! audit backend behind the enricher and asserts the entries the live path
//! wrote are sealed (entry_hash + signature), linked into a chain, and verify
//! under the signer — plus the adversarial dual that tampering a live-written
//! entry breaks `verify_chain`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use async_trait::async_trait;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery,
    AuditSigner, ChainError,
};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};

// ── Recording audit backend ─────────────────────────────────────────────────

/// Keeps every appended entry (in append/chain order) so the test can verify
/// the chain the enricher wrote. `query` returns newest-first to match the
/// `SqliteAuditBackend` ordering the enricher relies on for chain seeding.
#[derive(Default)]
struct CapturingAuditBackend {
    entries: tokio::sync::Mutex<Vec<AuditLogEntry>>,
}

#[async_trait]
impl AuditBackend for CapturingAuditBackend {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.entries.lock().await.push(entry);
        Ok(())
    }

    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(self.entries.lock().await.iter().rev().cloned().collect())
    }

    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(self.entries.lock().await.len() as u64)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build an `AppState` whose enricher signs with `signer` and writes to `audit`.
fn make_state(audit: Arc<dyn AuditBackend>, signer: AuditSigner) -> AppState {
    let backend = Arc::new(InMemoryBackend::new());
    let backend_clone = backend.clone();
    let enricher = Arc::new(AuditEnricher::with_signer(audit.clone(), signer));
    AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

async fn create_execution(backend: &Arc<dyn StateBackend>) -> ExecutionId {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "test-wf".into(),
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
                workflow_id: "test-wf".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");
    execution_id
}

async fn seed_approval_required(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    node_id: &str,
) {
    let seq = backend
        .latest_sequence(execution_id)
        .await
        .expect("latest_sequence")
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            EventKind::ToolApprovalRequired {
                node_id: node_id.into(),
                tool_name: format!("tool_{node_id}"),
                approver: "human".into(),
                context: serde_json::json!({ "action": node_id }),
            },
        ))
        .await
        .expect("append ToolApprovalRequired");
}

fn approve_body(node_id: &str, user_id: &str) -> Body {
    Body::from(
        serde_json::to_vec(&serde_json::json!({
            "decision": "approved",
            "node_id": node_id,
            "user_id": user_id,
        }))
        .unwrap(),
    )
}

// ── Test ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approve_path_writes_signed_chained_audit_entries() {
    let signer = AuditSigner::new(b"audit-emit-test-key".to_vec());
    let audit = Arc::new(CapturingAuditBackend::default());
    let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
    let state = make_state(audit_dyn, signer.clone());
    let backend = state.backend.clone();
    let router = build_router_with_opts(state, true);

    // One execution with two pending approvals.
    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    seed_approval_required(&backend, &execution_id, "b").await;
    let id_str = execution_id.to_string();

    // Drive two real approvals through the HTTP write path.
    for (node, user) in [("a", "alice"), ("b", "bob")] {
        let resp = router
            .clone()
            .oneshot(
                Request::post(format!("/executions/{id_str}/approve"))
                    .header("content-type", "application/json")
                    .body(approve_body(node, user))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "approve {node} must 200");
    }

    // The enricher must have written one sealed audit entry per approval.
    let entries = audit.entries.lock().await.clone();
    assert_eq!(entries.len(), 2, "one audit entry per approval");

    for (entry, user) in entries.iter().zip(["alice", "bob"]) {
        assert_eq!(entry.event_type, "approval_received");
        assert_eq!(entry.actor_type, ActorType::Human, "approver is a human");
        assert_eq!(
            entry.actor_id, user,
            "approver id flows into the audit entry"
        );
        assert!(entry.entry_hash.is_some(), "live entry must be sealed");
        assert!(entry.signature.is_some(), "live entry must be signed");
    }

    // It is a real chain: genesis has no predecessor, the next links to it.
    assert!(entries[0].prev_hash.is_none(), "first entry is the genesis");
    assert_eq!(
        entries[1].prev_hash, entries[0].entry_hash,
        "second entry links to the first"
    );

    // The chain the live path wrote verifies under the same signer.
    verify_chain(&entries, &signer).expect("live audit entries must form a verifiable chain");

    // Adversarial dual: tampering a live-written entry breaks verification.
    let mut tampered = entries.clone();
    tampered[0].actor_id = "mallory".to_string();
    assert_eq!(
        verify_chain(&tampered, &signer).unwrap_err(),
        ChainError::HashMismatch { index: 0 },
        "mutating a sealed entry must fail verification"
    );
}

//! The audit outbox on Postgres: every sealed event ends with exactly one
//! audit entry.
//!
//! Each event of a sealed kind (`SEALED_EVENT_KINDS`) is appended with an
//! `audit_outbox` row in the same transaction, and its audit entry deletes
//! the row in the entry's. The engine seals each event right after its
//! append, as before; the outbox drain seals what that seal missed: an
//! engine that stopped between the two, or an audit write that failed.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::backend::SealOntoHead;
use jamjet_audit::outbox::{drain, DUE_AFTER};
use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery,
    AuditSigner, PgAuditBackend,
};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_scheduler::Scheduler;
use jamjet_state::approvals::SEALED_EVENT_KINDS;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{Event, EventKind};
use jamjet_state::faults::{Fault, Scope};
use jamjet_state::{PgBackend, PgSettings, Tenant, TenantId, TenantStatus};
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ── An engine on Postgres ────────────────────────────────────────────────────

struct Env {
    db: TestDatabase,
    pg: PgBackend,
    /// The audit log as stored, for reading it back.
    log: PgAuditBackend,
    enricher: Arc<AuditEnricher>,
    signer: AuditSigner,
    app: axum::Router,
    alice: String,
    bob: String,
}

fn signer() -> AuditSigner {
    AuditSigner::new(b"outbox-test-key".to_vec())
}

async fn connect(db: &TestDatabase) -> PgBackend {
    let settings = PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    };
    PgBackend::connect(db.url(), settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

/// A server on a fresh database, sealing into the database's audit log.
async fn env(case: &str) -> Option<Env> {
    let Some(db) = TestDatabase::fresh().await else {
        jamjet_testkit::skip(case);
        return None;
    };
    let pg = connect(&db).await;
    let log = PgAuditBackend::new(pg.pool());
    let audit: Arc<dyn AuditBackend> = Arc::new(PgAuditBackend::new(pg.pool()));
    let signer = signer();
    let enricher = Arc::new(AuditEnricher::with_signer(audit.clone(), signer.clone()));
    let scoped = pg.clone();
    let state = AppState {
        backend: Arc::new(pg.clone()),
        backend_for_fn: Arc::new(move |t: &jamjet_state::TenantId| {
            Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher: enricher.clone(),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    let app = build_router_with_opts(state, false);
    let (alice, _) = pg.create_token("alice", "developer").await.unwrap();
    let (bob, _) = pg.create_token("bob", "reviewer").await.unwrap();
    Some(Env {
        db,
        pg,
        log,
        enricher,
        signer,
        app,
        alice,
        bob,
    })
}

async fn call(app: &axum::Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ir() -> Value {
    json!({
        "workflow_id": "wf-outbox", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "nodes": { "pay": {
            "id": "pay",
            "kind": { "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
            "policy": { "require_approval_for": ["*"], "approval_timeout_secs": 600, "escalate_after_secs": 60 }
        } },
        "edges": [ { "from": "pay", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// Start a run as alice. The first call registers the workflow.
async fn start(env: &Env) -> ExecutionId {
    let (st, body) = call(&env.app, "/workflows", &env.alice, json!({ "ir": ir() })).await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-outbox", "workflow_version": "0.1.0", "input": { "amount": 5 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

/// Start a run and claim its item on the claim route, whose node guard
/// raises and seals the approval request. The request's action digest.
async fn start_and_request(env: &Env) -> (ExecutionId, String) {
    let eid = start(env).await;
    let (st, body) = call(
        &env.app,
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!((st, body), (StatusCode::OK, json!({ "claimed": false })));
    let digest = env
        .pg
        .get_events(&eid)
        .await
        .unwrap()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => {
                context["action_digest"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .expect("a request with a digest in the log");
    (eid, digest)
}

/// Bob approves on the REST route.
async fn approve_rest(env: &Env, eid: &ExecutionId, digest: &str) -> (StatusCode, Value) {
    call(
        &env.app,
        &format!("/executions/exec_{}/approve", eid.0.simple()),
        &env.bob,
        json!({ "decision": "approved", "node_id": "pay", "action_digest": digest }),
    )
    .await
}

/// Bob approves through the MCP bridge.
async fn approve_mcp(env: &Env, eid: &ExecutionId, digest: &str) {
    let (st, body) = call(
        &env.app,
        "/mcp",
        &env.bob,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_approve", "arguments": {
                "execution_id": eid.to_string(), "decision": "approved",
                "node_id": "pay", "action_digest": digest,
            } },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(false), "{body}");
}

/// Every audit entry, oldest first.
async fn entries(env: &Env) -> Vec<AuditLogEntry> {
    let mut all = env
        .log
        .query(&AuditQuery {
            limit: 10_000,
            ..Default::default()
        })
        .await
        .unwrap();
    all.reverse();
    all
}

async fn outbox_rows(env: &Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_outbox")
        .fetch_one(&env.pg.pool())
        .await
        .unwrap()
}

fn kind_type(kind: &EventKind) -> String {
    serde_json::to_value(kind).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The ids of the run's events of a sealed kind.
async fn sealed_events(env: &Env, eid: &ExecutionId) -> Vec<Uuid> {
    env.pg
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter(|e| SEALED_EVENT_KINDS.contains(&kind_type(&e.kind).as_str()))
        .map(|e| e.id)
        .collect()
}

async fn gaps(env: &Env, eid: &ExecutionId) -> Vec<Uuid> {
    env.pg
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AuditSealFailed { event_id, .. } => Some(*event_id),
            _ => None,
        })
        .collect()
}

/// A request appended straight to the store, with no seal after it: what
/// a crash right after the append leaves.
async fn append_unsealed_request(pg: &PgBackend, eid: &ExecutionId, node: &str) -> Uuid {
    let event = Event::new(
        eid.clone(),
        0,
        EventKind::ToolApprovalRequired {
            node_id: node.into(),
            tool_name: node.into(),
            approver: "human".into(),
            context: json!({}),
        },
    );
    let id = event.id;
    pg.append_event(event).await.unwrap();
    id
}

// ── The tests ────────────────────────────────────────────────────────────────

/// A request (the node guard on the claim route), a REST decision, an MCP
/// decision, an escalation and an expiry each end with one audit entry and
/// no outbox row. The first seal of a fresh enricher goes through
/// `append_sealed` and the rest through `append_linked`, so both delete
/// their event's row.
#[tokio::test]
async fn every_sealed_path_leaves_the_outbox_empty() {
    let Some(env) = env("every_sealed_path_leaves_the_outbox_empty").await else {
        return;
    };

    let (rest, digest) = start_and_request(&env).await;
    let (st, body) = approve_rest(&env, &rest, &digest).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (mcp, digest) = start_and_request(&env).await;
    approve_mcp(&env, &mcp, &digest).await;

    let (swept, _) = start_and_request(&env).await;
    let scheduler = Scheduler::new(Arc::new(env.pg.clone()) as Arc<dyn StateBackend>)
        .with_sealer(env.enricher.clone());
    let now = Utc::now();
    for at in [
        now + ChronoDuration::seconds(120),
        now + ChronoDuration::seconds(3600),
    ] {
        scheduler
            .sweep_approval_deadlines(&swept, at)
            .await
            .unwrap();
    }

    let entries = entries(&env).await;
    let types: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "tool_approval_required",
            "approval_received",
            "tool_approval_required",
            "approval_received",
            "tool_approval_required",
            "approval_escalated",
            "approval_expired",
        ]
    );
    let mut sealed = Vec::new();
    for eid in [&rest, &mcp, &swept] {
        sealed.extend(sealed_events(&env, eid).await);
    }
    let sealed: BTreeSet<Uuid> = sealed.into_iter().collect();
    let entered: BTreeSet<Uuid> = entries.iter().map(|e| e.event_id).collect();
    assert_eq!(entered.len(), entries.len(), "one entry per event");
    assert_eq!(entered, sealed, "each sealed event has its entry");
    verify_chain(&entries, &env.signer).expect("the chain verifies");
    assert_eq!(
        outbox_rows(&env).await,
        0,
        "every entry deleted its outbox row"
    );
}

/// An event of a kind that is not sealed gets no outbox row. Each sealed
/// kind, appended beside it, gets one, with its run's sequence and tenant.
#[tokio::test]
async fn a_kind_outside_the_list_writes_no_outbox_row() {
    let Some(env) = env("a_kind_outside_the_list_writes_no_outbox_row").await else {
        return;
    };
    let eid = start(&env).await;
    let completed: EventKind = serde_json::from_value(json!({
        "type": "node_completed", "node_id": "pay", "output": {}, "state_patch": {},
        "duration_ms": 1,
    }))
    .unwrap();
    assert!(!SEALED_EVENT_KINDS.contains(&kind_type(&completed).as_str()));
    let completed = Event::new(eid.clone(), 0, completed);
    let completed_id = completed.id;
    env.pg.append_event(completed).await.unwrap();
    let mut expected = Vec::new();
    for kind in sealed_samples() {
        let event = Event::new(eid.clone(), 0, kind);
        let (id, kind) = (event.id, kind_type(&event.kind));
        let at = env.pg.append_event(event).await.unwrap();
        expected.push((id, eid.0, at, "default".to_string(), kind));
    }

    let rows: Vec<(Uuid, Uuid, i64, String, String)> = sqlx::query_as(
        "SELECT event_id, execution_id, sequence, tenant_id, kind_type FROM audit_outbox \
         ORDER BY sequence",
    )
    .fetch_all(&env.pg.pool())
    .await
    .unwrap();
    let logged = env.pg.get_events(&eid).await.unwrap();
    assert!(logged.iter().any(|e| e.id == completed_id));
    assert_eq!(
        rows, expected,
        "a row for each sealed event: none for the run's start events or node_completed"
    );
}

/// A decision whose inline seal was skipped (an engine that stopped between
/// the append and the seal) has no audit entry until the drain seals it,
/// once its row is old enough. The drain's entry has no HTTP context; its
/// actor is the decision's own approver, as the inline seal's would be.
#[tokio::test]
async fn a_decision_whose_inline_seal_failed_is_sealed_by_the_drain() {
    let Some(env) = env("a_decision_whose_inline_seal_failed_is_sealed_by_the_drain").await else {
        return;
    };
    let (eid, digest) = start_and_request(&env).await;

    let scope = Scope::new();
    scope.arm("decision.after_append_before_seal", 1, Fault::Error);
    let (st, body) = scope.run(approve_rest(&env, &eid, &digest)).await;
    assert_eq!(st, StatusCode::OK, "the decision is recorded: {body}");
    assert_eq!(scope.fired("decision.after_append_before_seal", 1), 1);

    let decision = env
        .pg
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .find(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .expect("the decision is in the log");
    let principal = match &decision.kind {
        EventKind::ApprovalReceived { principal, .. } => principal.clone().expect("a token"),
        _ => unreachable!(),
    };
    let of_decision = |all: &[AuditLogEntry]| -> Vec<AuditLogEntry> {
        all.iter()
            .filter(|e| e.event_id == decision.id)
            .cloned()
            .collect()
    };
    assert!(
        of_decision(&entries(&env).await).is_empty(),
        "the inline seal was skipped"
    );
    assert_eq!(outbox_rows(&env).await, 1, "the decision's row is left");

    let early = drain(&env.pg, &env.enricher, DUE_AFTER, 100).await;
    assert_eq!(
        early.due, 0,
        "a row newer than {DUE_AFTER:?} waits: {early:?}"
    );

    let report = drain(&env.pg, &env.enricher, Duration::ZERO, 100).await;
    assert_eq!(
        (report.due, report.sealed, report.failed),
        (1, 1, 0),
        "{report:?}"
    );
    let all = entries(&env).await;
    let sealed = of_decision(&all);
    assert_eq!(sealed.len(), 1, "one entry for the decision: {all:#?}");
    let entry = &sealed[0];
    assert_eq!(entry.event_type, "approval_received");
    assert_eq!(entry.sequence, decision.sequence);
    assert_eq!(entry.actor_id, principal);
    assert_eq!(entry.actor_type, ActorType::Human);
    assert_eq!(entry.tenant_id, "default");
    assert_eq!(
        (entry.http_method.as_deref(), entry.http_path.as_deref()),
        (None, None),
        "sealed by the drain, which has no request"
    );
    verify_chain(&all, &env.signer).expect("the chain verifies");
    assert_eq!(outbox_rows(&env).await, 0);
    assert!(gaps(&env, &eid).await.is_empty());
}

/// The audit log as stored, refusing every write while `failing` is set.
struct Flaky {
    inner: PgAuditBackend,
    failing: AtomicBool,
}

impl Flaky {
    fn refuse(&self) -> Result<(), AuditError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(AuditError::Database("audit log unavailable".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl AuditBackend for Flaky {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.refuse()?;
        self.inner.append(entry).await
    }
    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        self.refuse()?;
        self.inner.append_linked(entry).await
    }
    async fn append_sealed(
        &self,
        seal: &SealOntoHead<'_>,
    ) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.refuse()?;
        self.inner.append_sealed(seal).await
    }
    async fn head_at(&self) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.inner.head_at().await
    }
    async fn head(&self) -> Result<Option<String>, AuditError> {
        self.inner.head().await
    }
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        self.inner.query(q).await
    }
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        self.inner.count(q).await
    }
    async fn now(&self) -> Result<DateTime<Utc>, AuditError> {
        self.inner.now().await
    }
}

/// While the audit log refuses writes, each drain counts a failed seal; the
/// third records the gap in the run's log, once, and the row stays so the
/// event is tried again. Once the log takes writes, the next drain seals
/// it, and the gap record stays where it is.
#[tokio::test]
async fn a_seal_that_keeps_failing_records_one_gap_and_stays_visible() {
    let Some(env) = env("a_seal_that_keeps_failing_records_one_gap_and_stays_visible").await else {
        return;
    };
    // The drain's enricher writes through a log that refuses for now.
    let flaky = Arc::new(Flaky {
        inner: PgAuditBackend::new(env.pg.pool()),
        failing: AtomicBool::new(true),
    });
    let enricher = AuditEnricher::with_signer(flaky.clone(), env.signer.clone());

    let eid = start(&env).await;
    let request = append_unsealed_request(&env.pg, &eid, "pay").await;
    let row = || async {
        sqlx::query_as::<_, (i32, bool, Option<String>)>(
            "SELECT attempts, gap_recorded, last_error FROM audit_outbox WHERE event_id = $1",
        )
        .bind(request)
        .fetch_optional(&env.pg.pool())
        .await
        .unwrap()
    };

    for attempt in 1..=2 {
        let report = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
        assert_eq!((report.failed, report.gaps_recorded), (1, 0), "{report:?}");
        assert!(gaps(&env, &eid).await.is_empty(), "no gap after {attempt}");
        assert_eq!(row().await.map(|r| (r.0, r.1)), Some((attempt, false)));
    }
    let report = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
    assert_eq!((report.failed, report.gaps_recorded), (1, 1), "{report:?}");
    assert_eq!(gaps(&env, &eid).await, [request], "the gap names the event");
    let (attempts, gap_recorded, last_error) = row().await.expect("the row stays");
    assert_eq!((attempts, gap_recorded), (3, true));
    assert!(
        last_error
            .as_deref()
            .is_some_and(|e| e.contains("audit log unavailable")),
        "{last_error:?}"
    );

    let report = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
    assert_eq!((report.failed, report.gaps_recorded), (1, 0), "{report:?}");
    assert_eq!(gaps(&env, &eid).await.len(), 1, "the gap is recorded once");
    assert_eq!(row().await.map(|r| r.0), Some(4), "and tried again");
    assert!(entries(&env).await.is_empty());

    flaky.failing.store(false, Ordering::SeqCst);
    let report = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
    assert_eq!((report.sealed, report.failed), (1, 0), "{report:?}");
    let all = entries(&env).await;
    assert_eq!(
        all.iter().map(|e| e.event_id).collect::<Vec<_>>(),
        [request]
    );
    assert_eq!(all[0].actor_type, ActorType::System);
    assert_eq!(all[0].actor_id, "system");
    assert_eq!(row().await, None, "the row is gone with the seal");
    assert_eq!(gaps(&env, &eid).await, [request], "the gap record stays");
}

/// Two engines drain one outbox of 100 rows at once: 100 entries, one
/// chain, no row left. Both drains read rows the other also read, so the
/// race opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_drains_seal_once() {
    let Some(env) = env("two_drains_seal_once").await else {
        return;
    };
    let mut appended = BTreeSet::new();
    for _ in 0..10 {
        let eid = start(&env).await;
        for n in 0..10 {
            appended.insert(append_unsealed_request(&env.pg, &eid, &format!("n{n}")).await);
        }
    }
    assert_eq!(outbox_rows(&env).await, 100);

    let other = connect(&env.db).await;
    let other_enricher = AuditEnricher::with_signer(
        Arc::new(PgAuditBackend::new(other.pool())),
        env.signer.clone(),
    );
    let (a, b) = tokio::join!(
        drain(&env.pg, &env.enricher, Duration::ZERO, 1_000),
        drain(&other, &other_enricher, Duration::ZERO, 1_000),
    );
    assert_eq!((a.failed, b.failed), (0, 0), "{a:?} {b:?}");
    assert!(a.due + b.due > 100, "the drains overlapped: {a:?} {b:?}");

    let all = entries(&env).await;
    assert_eq!(all.len(), 100);
    let sealed: BTreeSet<Uuid> = all.iter().map(|e| e.event_id).collect();
    assert_eq!(sealed, appended, "each event once");
    verify_chain(&all, &env.signer).expect("one chain");
    assert_eq!(all.iter().filter(|e| e.prev_hash.is_none()).count(), 1);
    assert_eq!(outbox_rows(&env).await, 0);
}

/// A request in a run of a second tenant: its outbox row and its drained
/// entry carry that tenant, though the drain runs on the unscoped store. A
/// view of another tenant neither sees the row nor counts a failure on it;
/// the run's own tenant's view sees it.
#[tokio::test]
async fn a_drained_event_keeps_its_runs_tenant() {
    let Some(env) = env("a_drained_event_keeps_its_runs_tenant").await else {
        return;
    };
    let now = Utc::now();
    let acme = TenantId::from("acme");
    // A tenant is created through its own view, as the tenant routes do.
    env.pg
        .for_tenant(acme.clone())
        .create_tenant(Tenant {
            id: acme.clone(),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let eid = ExecutionId::new();
    env.pg
        .for_tenant(acme.clone())
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf-outbox".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    let request = append_unsealed_request(&env.pg, &eid, "pay").await;
    let tenant_of_row: String =
        sqlx::query_scalar("SELECT tenant_id FROM audit_outbox WHERE event_id = $1")
            .bind(request)
            .fetch_one(&env.pg.pool())
            .await
            .unwrap();
    assert_eq!(tenant_of_row, "acme", "the row carries the run's tenant");

    let other = env.pg.for_tenant(TenantId::from("default"));
    assert!(other
        .outbox_due(Duration::ZERO, 100)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(other.outbox_note_failure(request, "no").await.unwrap(), 0);
    other.outbox_mark_gap(request).await.unwrap();
    let own = env.pg.for_tenant(acme.clone());
    let due = own.outbox_due(Duration::ZERO, 100).await.unwrap();
    assert_eq!(
        due.iter()
            .map(|r| (r.event.id, r.tenant_id.as_str(), r.attempts, r.gap_recorded))
            .collect::<Vec<_>>(),
        [(request, "acme", 0, false)],
        "the other tenant's view changed nothing"
    );

    let report = drain(&env.pg, &env.enricher, Duration::ZERO, 100).await;
    assert_eq!((report.due, report.sealed), (1, 1), "{report:?}");
    let all = entries(&env).await;
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].event_id, all[0].tenant_id.as_str()),
        (request, "acme")
    );
    assert_eq!(outbox_rows(&env).await, 0);
}

/// An outbox row whose event this build cannot read is left where it is and
/// does not hold up the rows after it.
#[tokio::test]
async fn an_unreadable_row_does_not_hold_up_the_rest() {
    let Some(env) = env("an_unreadable_row_does_not_hold_up_the_rest").await else {
        return;
    };
    let eid = start(&env).await;
    let unreadable = append_unsealed_request(&env.pg, &eid, "a").await;
    let readable = append_unsealed_request(&env.pg, &eid, "b").await;
    sqlx::query("UPDATE events SET kind_json = 'not json' WHERE id = $1")
        .bind(unreadable)
        .execute(&env.pg.pool())
        .await
        .unwrap();

    let report = drain(&env.pg, &env.enricher, Duration::ZERO, 100).await;
    assert_eq!(
        (report.due, report.sealed, report.error.as_deref()),
        (1, 1, None),
        "{report:?}"
    );
    let all = entries(&env).await;
    assert_eq!(
        all.iter().map(|e| e.event_id).collect::<Vec<_>>(),
        [readable]
    );
    let left: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM audit_outbox")
        .fetch_all(&env.pg.pool())
        .await
        .unwrap();
    assert_eq!(left, [unreadable], "the unreadable row stays");
}

/// A full page of unreadable rows at the head of the outbox: one drain at
/// that limit still reaches the readable row behind them.
#[tokio::test]
async fn a_page_of_unreadable_rows_does_not_starve_the_drain() {
    let Some(env) = env("a_page_of_unreadable_rows_does_not_starve_the_drain").await else {
        return;
    };
    let eid = start(&env).await;
    for n in 0..100 {
        let id = append_unsealed_request(&env.pg, &eid, &format!("bad{n}")).await;
        sqlx::query("UPDATE events SET kind_json = 'not json' WHERE id = $1")
            .bind(id)
            .execute(&env.pg.pool())
            .await
            .unwrap();
    }
    let readable = append_unsealed_request(&env.pg, &eid, "good").await;

    let report = drain(&env.pg, &env.enricher, Duration::ZERO, 100).await;
    assert_eq!(
        (report.due, report.sealed, report.error.as_deref()),
        (1, 1, None),
        "{report:?}"
    );
    assert_eq!(
        entries(&env)
            .await
            .iter()
            .map(|e| e.event_id)
            .collect::<Vec<_>>(),
        [readable]
    );
    assert_eq!(outbox_rows(&env).await, 100, "the unreadable rows stay");
}

/// The audit log as stored, refusing the entries of the events named.
struct Refusing {
    inner: PgAuditBackend,
    refused: BTreeSet<Uuid>,
}

impl Refusing {
    fn check(&self, event_id: Uuid) -> Result<(), AuditError> {
        if self.refused.contains(&event_id) {
            return Err(AuditError::Database("this entry is refused".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl AuditBackend for Refusing {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.check(entry.event_id)?;
        self.inner.append(entry).await
    }
    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        self.check(entry.event_id)?;
        self.inner.append_linked(entry).await
    }
    async fn append_sealed(
        &self,
        seal: &SealOntoHead<'_>,
    ) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        // Sealing has no effect but the entry it answers: peek at its event.
        self.check(seal(None, None, Utc::now()).event_id)?;
        self.inner.append_sealed(seal).await
    }
    async fn head_at(&self) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.inner.head_at().await
    }
    async fn head(&self) -> Result<Option<String>, AuditError> {
        self.inner.head().await
    }
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        self.inner.query(q).await
    }
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        self.inner.count(q).await
    }
    async fn now(&self) -> Result<DateTime<Utc>, AuditError> {
        self.inner.now().await
    }
}

/// A full page of rows whose seal keeps failing, at the head of the outbox:
/// after one drain has tried them, the next reaches the row behind them.
#[tokio::test]
async fn a_page_of_failing_rows_does_not_starve_the_drain() {
    let Some(env) = env("a_page_of_failing_rows_does_not_starve_the_drain").await else {
        return;
    };
    let eid = start(&env).await;
    let mut failing = BTreeSet::new();
    for n in 0..100 {
        failing.insert(append_unsealed_request(&env.pg, &eid, &format!("bad{n}")).await);
    }
    let readable = append_unsealed_request(&env.pg, &eid, "good").await;
    let enricher = AuditEnricher::with_signer(
        Arc::new(Refusing {
            inner: PgAuditBackend::new(env.pg.pool()),
            refused: failing,
        }),
        env.signer.clone(),
    );

    let first = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
    assert_eq!((first.due, first.failed), (100, 100), "{first:?}");
    let second = drain(&env.pg, &enricher, Duration::ZERO, 100).await;
    assert_eq!(second.sealed, 1, "{second:?}");
    assert_eq!(
        entries(&env)
            .await
            .iter()
            .map(|e| e.event_id)
            .collect::<Vec<_>>(),
        [readable]
    );
    assert_eq!(outbox_rows(&env).await, 100, "the failing rows stay");
}

/// The audit log refusing every write, after both of two drains have come
/// to their seal: each drain has read the row before either counts it.
struct Gate {
    barrier: tokio::sync::Barrier,
    met: AtomicBool,
}

impl Gate {
    async fn refuse(&self) -> Result<(), AuditError> {
        if tokio::time::timeout(Duration::from_secs(5), self.barrier.wait())
            .await
            .is_ok()
        {
            self.met.store(true, Ordering::SeqCst);
        }
        Err(AuditError::Database("audit log unavailable".into()))
    }
}

#[async_trait]
impl AuditBackend for Gate {
    async fn append(&self, _: AuditLogEntry) -> Result<(), AuditError> {
        self.refuse().await
    }
    async fn append_linked(&self, _: AuditLogEntry) -> Result<bool, AuditError> {
        self.refuse().await.map(|_| false)
    }
    async fn append_sealed(
        &self,
        _: &SealOntoHead<'_>,
    ) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.refuse().await.map(|_| None)
    }
    async fn query(&self, _: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(Vec::new())
    }
    async fn count(&self, _: &AuditQuery) -> Result<u64, AuditError> {
        Ok(0)
    }
}

/// Two engines drain a row whose seal has failed twice, at once, and both
/// fail it a third time: the run's log records its gap once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_drains_at_the_third_failure_record_one_gap() {
    let Some(env) = env("two_drains_at_the_third_failure_record_one_gap").await else {
        return;
    };
    let eid = start(&env).await;
    let request = append_unsealed_request(&env.pg, &eid, "pay").await;
    sqlx::query("UPDATE audit_outbox SET attempts = 2 WHERE event_id = $1")
        .bind(request)
        .execute(&env.pg.pool())
        .await
        .unwrap();
    let gate = Arc::new(Gate {
        barrier: tokio::sync::Barrier::new(2),
        met: AtomicBool::new(false),
    });
    let a = AuditEnricher::with_signer(gate.clone(), env.signer.clone());
    let b = AuditEnricher::with_signer(gate.clone(), env.signer.clone());
    let other = connect(&env.db).await;
    let (ra, rb) = tokio::join!(
        drain(&env.pg, &a, Duration::ZERO, 100),
        drain(&other, &b, Duration::ZERO, 100),
    );
    assert!(
        gate.met.load(Ordering::SeqCst),
        "both drains read the row before either failed it"
    );
    assert_eq!((ra.failed, rb.failed), (1, 1), "{ra:?} {rb:?}");
    assert_eq!(gaps(&env, &eid).await, [request], "one gap: {ra:?} {rb:?}");
    assert_eq!(ra.gaps_recorded + rb.gaps_recorded, 1, "{ra:?} {rb:?}");
}

/// The engine's own seal fails right after the append and records the
/// gap; the drain then fails the event three times more and records no
/// second one.
#[tokio::test]
async fn an_inline_gap_is_not_recorded_again_by_the_drain() {
    let Some(env) = env("an_inline_gap_is_not_recorded_again_by_the_drain").await else {
        return;
    };
    let down = AuditEnricher::with_signer(
        Arc::new(Flaky {
            inner: PgAuditBackend::new(env.pg.pool()),
            failing: AtomicBool::new(true),
        }),
        env.signer.clone(),
    );
    let eid = start(&env).await;
    let event = Event::new(
        eid.clone(),
        0,
        EventKind::ToolApprovalRequired {
            node_id: "pay".into(),
            tool_name: "pay".into(),
            approver: "human".into(),
            context: json!({}),
        },
    );
    let sequence = env.pg.append_event(event.clone()).await.unwrap();
    jamjet_state::approvals::seal_appended(
        &env.pg,
        Some(&down),
        event.clone(),
        sequence,
        "default",
    )
    .await;
    assert_eq!(gaps(&env, &eid).await, [event.id], "the inline seal's gap");

    for _ in 0..3 {
        let report = drain(&env.pg, &down, Duration::ZERO, 100).await;
        assert_eq!(report.failed, 1, "{report:?}");
    }
    assert_eq!(gaps(&env, &eid).await, [event.id], "still one gap");
}

// ── The list of sealed kinds ─────────────────────────────────────────────────

/// A kind a sealer seals, and the files that make its event.
type Sealed = (&'static str, &'static [&'static str]);

/// Each place the engine seals an event: the file, the function the call
/// sits in, and the kinds it seals there (`None`: the drain, which seals
/// what the outbox holds, and `seal_appended`, which seals for its
/// callers listed here). Each kind is made as `EventKind::<Variant> {` in
/// the files named with it.
const SEALERS: &[(&str, &str, Option<&[Sealed]>)] = &[
    (
        "workers/src/node_guard.rs",
        "record_request",
        Some(&[(
            "tool_approval_required",
            &["workers/src/node_guard.rs", "workers/src/dispatch_guard.rs"],
        )]),
    ),
    (
        "scheduler/src/runner.rs",
        "seal",
        Some(&[
            ("approval_expired", &["scheduler/src/runner.rs"]),
            ("approval_escalated", &["scheduler/src/runner.rs"]),
        ]),
    ),
    (
        "api/src/routes.rs",
        "approve_execution",
        Some(&[("approval_received", &["api/src/approvals.rs"])]),
    ),
    (
        "api/src/mcp_bridge.rs",
        "build_mcp_bridge",
        Some(&[("approval_received", &["api/src/approvals.rs"])]),
    ),
    ("audit/src/outbox.rs", "drain", None),
    // Hands an event from the sealers above that call it to the enricher.
    ("state/src/approvals.rs", "seal_appended", None),
];

/// What the scan looks for: each way into the enricher's seal.
/// `enrich_and_append(` also matches `try_enrich_and_append(`.
const SEAL_CALLS: &[&str] = &[
    "seal_appended(",
    "enrich_and_append(",
    "seal_approval_record(",
];

/// Calls the scan passes over, by file and function: the enricher's entry
/// points calling its own seal, which are the seal, not a sealer. Each must
/// still be in the code, so a stale entry cannot hide a new call.
const INTERNAL: &[(&str, &str)] = &[
    ("audit/src/enricher.rs", "enrich_and_append"),
    ("audit/src/enricher.rs", "seal_approval_record"),
];

/// The source scan finds every call that seals an event, outside the
/// enricher itself, and each is in [`SEALERS`] with the kinds it seals;
/// those kinds are `SEALED_EVENT_KINDS`, each one an event kind's real
/// `type` tag. A new sealer fails here until its kind is on the list, since
/// an event of a kind off the list gets no outbox row on Postgres.
#[test]
fn sealed_kinds_list_matches_the_sealers() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the api crate sits in runtime/");
    let mut found: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for file in sources(runtime) {
        let rel = file
            .strip_prefix(runtime)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || !SEAL_CALLS.iter().any(|c| code.contains(c)) {
                continue;
            }
            if fn_name(code).is_some() {
                continue; // the definition
            }
            let within = lines[..n]
                .iter()
                .rev()
                .find_map(|l| fn_name(l.trim_start()))
                .unwrap_or_else(|| panic!("{rel}:{}: a seal outside any function", n + 1));
            found.entry((rel.clone(), within)).or_default().push(n + 1);
        }
    }
    for (file, within) in INTERNAL {
        let key = (file.to_string(), within.to_string());
        assert!(
            found.remove(&key).is_some(),
            "{file} {within} no longer calls the seal: drop it from INTERNAL"
        );
    }

    let listed: BTreeSet<(String, String)> = SEALERS
        .iter()
        .map(|(file, within, _)| (file.to_string(), within.to_string()))
        .collect();
    let in_code: BTreeSet<(String, String)> = found.keys().cloned().collect();
    assert_eq!(
        in_code, listed,
        "the sealers in the code (left, with their lines: {found:?}) are not the ones listed \
         here: a new one seals a kind that must be on SEALED_EVENT_KINDS, and goes in SEALERS"
    );

    let mut kinds = BTreeSet::new();
    for (_, _, sealed) in SEALERS {
        for (kind, made_in) in sealed.iter().flat_map(|s| s.iter()) {
            kinds.insert(*kind);
            let variant = camel(kind);
            for file in *made_in {
                let text = std::fs::read_to_string(runtime.join(file)).unwrap();
                assert!(
                    text.contains(&format!("EventKind::{variant} {{")),
                    "{file} makes no EventKind::{variant}"
                );
            }
        }
    }
    let list: BTreeSet<&str> = SEALED_EVENT_KINDS.iter().copied().collect();
    assert_eq!(list.len(), SEALED_EVENT_KINDS.len(), "a kind listed twice");
    assert_eq!(kinds, list, "the kinds sealed are not SEALED_EVENT_KINDS");

    // Each listed name is the tag the event's `type` carries, as the store
    // compares it.
    let samples = sealed_samples();
    let tags: BTreeSet<String> = samples.iter().map(kind_type).collect();
    assert_eq!(tags, list.iter().map(|k| k.to_string()).collect());
}

/// One event of each sealed kind.
fn sealed_samples() -> Vec<EventKind> {
    let at = Utc::now();
    vec![
        EventKind::ToolApprovalRequired {
            node_id: "n".into(),
            tool_name: "t".into(),
            approver: "a".into(),
            context: json!({}),
        },
        serde_json::from_value(json!({
            "type": "approval_received", "node_id": "n", "user_id": "u",
            "decision": "approved", "comment": null, "state_patch": null,
        }))
        .unwrap(),
        EventKind::ApprovalExpired {
            node_id: "n".into(),
            request_sequence: 1,
            expires_at: at,
            principal: "p".into(),
        },
        EventKind::ApprovalEscalated {
            node_id: "n".into(),
            request_sequence: 1,
            escalate_at: at,
            expires_at: at,
        },
    ]
}

/// `approval_received` as `ApprovalReceived`.
fn camel(kind: &str) -> String {
    kind.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

/// The name of the function a trimmed line declares, if it declares one.
fn fn_name(code: &str) -> Option<String> {
    let at = code.find("fn ")?;
    let before = &code[..at];
    let modifiers = before
        .split_whitespace()
        .all(|w| w == "pub" || w.starts_with("pub(") || w == "async" || w == "const");
    if !modifiers {
        return None;
    }
    let name: String = code[at + 3..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Every `.rs` file under a `src` directory of a crate in `runtime`, in a
/// stable order. Build output and test directories are left out.
fn sources(runtime: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, in_src: bool, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if path.is_dir() {
                if matches!(name.as_str(), "target" | "tests" | "node_modules")
                    || name.starts_with('.')
                {
                    continue;
                }
                walk(&path, in_src || name == "src", out);
            } else if in_src && name.ends_with(".rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(runtime, false, &mut out);
    assert!(out.len() > 50, "the scan found only {} files", out.len());
    out
}

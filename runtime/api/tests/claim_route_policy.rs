//! Tool-policy enforcement on `POST /work-items/claim`.
//!
//! An ADK agent compiles a whole turn's model-chosen tool calls into ONE
//! `python_fn` node, which the scheduler routes to the `python_tool` queue. The
//! engine's own worker pool registers that queue with ZERO in-process workers
//! (`runtime/workers/src/pool.rs`) — it is claimed exclusively by the external
//! `jamjet worker` CLI over HTTP. So the in-process worker's policy check never
//! saw an ADK dispatch node in production, and `blocked_tools` /
//! `require_approval_for` were silently unenforced on exactly the nodes that
//! execute model-chosen tools (review finding C1).
//!
//! These tests drive the real route. The load-bearing assertion in every
//! enforcement case is that the response carries NO `work_item` — because the
//! payload IS the capability: once it is handed back, the external worker
//! dispatches the tool and no later check can un-send a wire transfer.
//!
//! Harness follows `work_item_genai.rs` (in-process axum router over an
//! `InMemoryBackend`, dev-mode so no auth) and `java_tool_roundtrip.rs` (a
//! SQLite backend where durable work-item status is the thing under test).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, StateBackendError, WorkItem, WorkflowDefinition};
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::{content_hash, Event, InMemoryBackend, SqliteBackend, DEFAULT_TENANT};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

// ── Harness ───────────────────────────────────────────────────────────────────

fn make_state(backend: Arc<dyn StateBackend>) -> AppState {
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

/// The raw response bytes of one claim, so a refusal can be compared to an
/// empty-queue response byte for byte rather than field by field.
async fn claim_raw(state: &AppState, queue: &str) -> Vec<u8> {
    let body = json!({ "worker_id": "external-python-worker-0", "queue_types": [queue] });
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post("/work-items/claim")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "claim must stay a 200 — a distinct status is itself a signal the \
         untrusted worker could act on"
    );
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn claim(state: &AppState, queue: &str) -> Value {
    serde_json::from_slice(&claim_raw(state, queue).await).unwrap()
}

/// The whole enforcement contract in one assertion: the caller learns nothing
/// and, above all, does NOT get the payload.
fn assert_withheld(resp: &Value) {
    // The payload assertion comes FIRST because it is the load-bearing one: the
    // payload is the capability, and a failure here must name what leaked.
    assert_eq!(
        resp.get("work_item"),
        None,
        "the payload is the capability — a refusal that still returns it has \
         enforced nothing; got {resp}"
    );
    assert_eq!(
        resp["claimed"],
        json!(false),
        "the item must not be claimed"
    );
}

fn assert_handed_out(resp: &Value, node_id: &str) {
    assert_eq!(resp["claimed"], json!(true), "the item must be claimed");
    assert_eq!(resp["work_item"]["node_id"], json!(node_id));
    assert!(
        resp["work_item"]["payload"].is_object(),
        "the payload must be intact; got {resp}"
    );
}

// ── Fixtures ──────────────────────────────────────────────────────────────────

const WF: &str = "adk-wf";
const VERSION: &str = "1.0.0";

/// A one-node workflow whose node is the ADK tool-dispatch `python_fn`.
///
/// `marked` toggles `agent_tool_dispatch`, which is the ONLY thing separating a
/// node the guard evaluates from one it must pass through untouched.
fn dispatch_ir(marked: bool, blocked: &[&str], approval: &[&str]) -> Value {
    ir_with_node(json!({
        "id": "n1",
        "kind": {
            "type": "python_fn",
            "module": "jamjet.agents.tool_runtime",
            "function": "dispatch_tool_calls",
            "output_schema": "",
            "agent_tool_dispatch": marked
        }
    }))
    .tap_policy(blocked, approval)
}

/// A node kind that can never be an agent dispatch — the "every other queue"
/// case the route must not perturb.
///
/// It carries a REAL `blocked_tools` policy on purpose. An unpoliced fixture
/// would prove nothing: `guard_dispatch` short-circuits an empty policy chain
/// and returns `Allow` before it reads anything, so such a node sails through
/// whether or not the marker gate exists. With a policy in force and a payload
/// that has no `input` key, removing the gate makes the guard read the calls,
/// get `NotAnObject`, and terminally fail the item — which is precisely the
/// "breaks all durable tool execution" regression this fixture must catch.
fn policed_condition_ir() -> Value {
    ir_with_node(json!({ "id": "n1", "kind": { "type": "condition", "branches": [] } }))
        .tap_policy(&["send_wire"], &[])
}

fn ir_with_node(node: Value) -> Value {
    json!({
        "workflow_id": WF,
        "version": VERSION,
        "state_schema": "{}",
        "start_node": "n1",
        "nodes": { "n1": node },
        "edges": [],
        "retry_policies": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {}
    })
}

/// Attach a workflow-level policy set. Absent entirely when both lists are
/// empty, so the empty-policy-chain case is genuinely policy-free.
trait TapPolicy {
    fn tap_policy(self, blocked: &[&str], approval: &[&str]) -> Value;
}

impl TapPolicy for Value {
    fn tap_policy(mut self, blocked: &[&str], approval: &[&str]) -> Value {
        if blocked.is_empty() && approval.is_empty() {
            return self;
        }
        self["policy"] = json!({
            "blocked_tools": blocked,
            "require_approval_for": approval,
            "model_allowlist": []
        });
        self
    }
}

fn call(name: &str) -> Value {
    json!({ "id": format!("call-{name}"), "name": name, "arguments": {} })
}

/// The scheduler's enriched `python_fn` payload (`runtime/scheduler/src/runner.rs`):
/// workflow coordinates, dispatch coordinates, and the frozen accumulated state
/// as `input`. The pending tool calls live at `payload["input"]["tool_calls"]`.
fn dispatch_payload(input: Value) -> Value {
    json!({
        "workflow_id": WF,
        "workflow_version": VERSION,
        "node_id": "n1",
        "module": "jamjet.agents.tool_runtime",
        "function": "dispatch_tool_calls",
        "input": input
    })
}

fn calls_input(names: &[&str]) -> Value {
    json!({ "tool_calls": names.iter().map(|n| call(n)).collect::<Vec<_>>() })
}

/// The exact shape `guard_dispatch` hashes when it binds an approval to the
/// calls that approval authorises.
fn calls_hash_for(names: &[&str]) -> String {
    content_hash(&json!(names
        .iter()
        .map(|n| json!({ "name": n, "arguments": {} }))
        .collect::<Vec<_>>()))
}

/// Store `ir`, create a Running execution, and enqueue one claimable work item.
async fn seed(
    backend: &Arc<dyn StateBackend>,
    ir: Value,
    queue_type: &str,
    payload: Value,
) -> (ExecutionId, Uuid) {
    store(backend, WF, VERSION, ir).await;
    seed_item(backend, queue_type, payload).await
}

/// Store one workflow definition under explicit coordinates.
async fn store(backend: &Arc<dyn StateBackend>, workflow_id: &str, version: &str, ir: Value) {
    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: workflow_id.into(),
            version: version.into(),
            ir,
            created_at: chrono::Utc::now(),
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("store_workflow");
}

/// Enqueue a claimable item for a fresh execution WITHOUT storing a workflow —
/// the unresolvable-coordinates case.
async fn seed_item(
    backend: &Arc<dyn StateBackend>,
    queue_type: &str,
    payload: Value,
) -> (ExecutionId, Uuid) {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: WF.into(),
            workflow_version: VERSION.into(),
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
        .expect("create_execution");
    let node_id = payload
        .get("node_id")
        .and_then(|v| v.as_str())
        .unwrap_or("n1")
        .to_string();
    let id = Uuid::new_v4();
    backend
        .enqueue_work_item(WorkItem {
            id,
            execution_id: execution_id.clone(),
            node_id,
            queue_type: queue_type.into(),
            payload,
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
    (execution_id, id)
}

async fn append(backend: &Arc<dyn StateBackend>, execution_id: &ExecutionId, kind: EventKind) {
    // Sequence 0 is a placeholder; both backends assign the real sequence.
    backend
        .append_event(Event::new(execution_id.clone(), 0, kind))
        .await
        .expect("append_event");
}

async fn violations(backend: &Arc<dyn StateBackend>, execution_id: &ExecutionId) -> Vec<String> {
    backend
        .get_events(execution_id)
        .await
        .expect("get_events")
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::PolicyViolation { rule, .. } => Some(rule.clone()),
            _ => None,
        })
        .collect()
}

async fn approval_requests(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
) -> Vec<Value> {
    backend
        .get_events(execution_id)
        .await
        .expect("get_events")
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
            _ => None,
        })
        .collect()
}

fn memory() -> Arc<dyn StateBackend> {
    Arc::new(InMemoryBackend::new())
}

/// A durable backend plus the raw pool used to read `work_items.status`.
///
/// `StateBackend` exposes no work-item read, so the SETTLE a claim performed can
/// only be observed in the table. Indirect probes (re-claim, `renew_lease`)
/// cannot separate `fail_work_item` from `complete_work_item` — both leave the
/// item unclaimable — which is exactly how an unbound settle assertion hides.
struct Durable {
    backend: Arc<dyn StateBackend>,
    pool: sqlx::SqlitePool,
    db_path: std::path::PathBuf,
}

impl Durable {
    async fn new() -> Self {
        let db_path = std::env::temp_dir().join(format!("jjtest-claim-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}", db_path.display());
        let backend: Arc<dyn StateBackend> =
            Arc::new(SqliteBackend::open(&url).await.expect("open sqlite"));
        let pool = sqlx::SqlitePool::connect(&url).await.expect("open pool");
        Self {
            backend,
            pool,
            db_path,
        }
    }

    /// The item's durable `status`: `claimed`, `completed`, `failed`, `pending`.
    async fn status(&self, item_id: Uuid) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM work_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&self.pool)
            .await
            .expect("the work item row must exist")
    }

    async fn cleanup(self) {
        self.pool.close().await;
        let _ = std::fs::remove_file(&self.db_path);
    }
}

// ══ 1. Enforcement ════════════════════════════════════════════════════════════

/// **The C1 reproduction.** Before this route ran the guard, `send_wire` came
/// straight back to the external worker with `claimed: true` and a payload
/// carrying the call — while the SDK told the operator the engine enforced
/// `blocked_tools` fail-closed.
#[tokio::test]
async fn a_blocked_tool_is_never_handed_to_the_worker() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["read_file", "send_wire"])),
    )
    .await;
    let state = make_state(backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    let recorded = violations(&backend, &execution_id).await;
    assert_eq!(
        recorded.len(),
        1,
        "a refusal must reach the Prove surface exactly once; got {recorded:?}"
    );
    assert!(
        recorded[0].contains("send_wire"),
        "the audit rule must name the rule that matched; got {recorded:?}"
    );
}

/// A refusal must be indistinguishable from an empty queue. The external worker
/// is untrusted for this decision: if it could tell "blocked" from "nothing to
/// do" it would gain a probe for which tools a tenant's policy forbids.
#[tokio::test]
async fn a_refusal_is_byte_identical_to_an_empty_queue() {
    let empty = make_state(memory());
    let empty_bytes = claim_raw(&empty, "python_tool").await;

    let backend = memory();
    seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let blocked_bytes = claim_raw(&make_state(backend), "python_tool").await;

    assert_eq!(
        blocked_bytes, empty_bytes,
        "a blocked claim must be byte-identical to an empty-queue claim"
    );
}

/// Anti-livelock. `InMemoryBackend::fail_work_item` clears the lease rather than
/// marking the item terminal, so the blocked item becomes claimable again — the
/// most hostile version of this test. However many times it comes back, the
/// payload must never escape.
#[tokio::test]
async fn a_blocked_item_never_leaks_its_payload_however_often_it_is_reclaimed() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let state = make_state(backend.clone());

    for attempt in 1..=3 {
        let resp = claim(&state, "python_tool").await;
        assert_eq!(
            resp["claimed"],
            json!(false),
            "claim {attempt} handed out a blocked item"
        );
        assert_eq!(
            resp.get("work_item"),
            None,
            "claim {attempt} leaked the payload"
        );
    }

    assert!(
        !violations(&backend, &execution_id).await.is_empty(),
        "every refusal is audited"
    );
}

/// On a durable backend a blocked item is TERMINALLY failed.
///
/// The status assertion is the load-bearing one: `failed` and `completed` are
/// both unclaimable, so any probe that only re-claims would pass for either, and
/// the settle would be untested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_dispatch_is_terminally_failed_on_a_durable_backend() {
    let d = Durable::new().await;
    let (execution_id, item_id) = seed(
        &d.backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let state = make_state(d.backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    assert_eq!(
        d.status(item_id).await,
        "failed",
        "a blocked dispatch must be failed, not completed and not left claimed"
    );

    // And terminal: never re-claimed, so never re-evaluated.
    assert_withheld(&claim(&state, "python_tool").await);
    assert_eq!(
        violations(&d.backend, &execution_id).await.len(),
        1,
        "a terminally failed item is not re-claimed, so it is not re-evaluated"
    );

    d.cleanup().await;
}

/// A held item is SETTLED, and settled as `completed` — not failed, and above
/// all not left `claimed` with a lease ticking down into the retry path.
///
/// This assertion is what binds the `Held` arm's settle. Removing the settle
/// leaves the item `claimed`, which no re-claim probe can detect: an item still
/// leased is skipped by `claim_work_item` exactly as a completed one is, so the
/// re-claim returns nothing and the request count stays at one either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_dispatch_is_settled_as_completed_on_a_durable_backend() {
    let d = Durable::new().await;
    let (execution_id, item_id) = seed(
        &d.backend,
        dispatch_ir(true, &[], &["send_wire"]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let state = make_state(d.backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    assert_eq!(
        d.status(item_id).await,
        "completed",
        "a held item must be settled so its lease never expires into the retry \
         path; still 'claimed' means the settle is missing"
    );
    assert_eq!(
        approval_requests(&d.backend, &execution_id).await.len(),
        1,
        "exactly one approval request"
    );

    d.cleanup().await;
}

/// An approval-gated batch is held, not handed out, and the request carries the
/// material a human needs to decide plus the hash that binds the decision to
/// these exact calls.
#[tokio::test]
async fn an_approval_gated_tool_is_held_and_requests_approval() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &[], &["send_wire"]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let state = make_state(backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    let requests = approval_requests(&backend, &execution_id).await;
    assert_eq!(requests.len(), 1, "exactly one request; got {requests:?}");
    assert_eq!(requests[0]["gated_tools"], json!(["send_wire"]));
    assert_eq!(
        requests[0]["calls"],
        json!([{"name": "send_wire", "arguments": {}}])
    );
    assert_eq!(
        requests[0]["calls_hash"],
        json!(calls_hash_for(&["send_wire"])),
        "the request must bind the calls it authorises"
    );
    assert!(
        violations(&backend, &execution_id).await.is_empty(),
        "a hold is not a denial"
    );

    // Held settles the item, so it is not re-claimed into a duplicate request.
    assert_withheld(&claim(&state, "python_tool").await);
    assert_eq!(
        approval_requests(&backend, &execution_id).await.len(),
        1,
        "a held item must not stack up approval requests"
    );
}

/// One gated call holds the WHOLE batch. Running the two permitted calls and
/// holding the third would execute a partial turn the approver never saw.
#[tokio::test]
async fn one_gated_call_holds_the_whole_batch() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &[], &["send_wire"]),
        "python_tool",
        dispatch_payload(calls_input(&["read_file", "send_wire", "log_event"])),
    )
    .await;
    let state = make_state(backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    let requests = approval_requests(&backend, &execution_id).await;
    assert_eq!(
        requests[0]["gated_tools"],
        json!(["send_wire"]),
        "only the gated call is named to the approver"
    );
    assert_eq!(
        requests[0]["calls"],
        json!([
            {"name": "read_file", "arguments": {}},
            {"name": "send_wire", "arguments": {}},
            {"name": "log_event", "arguments": {}}
        ]),
        "the whole batch is bound, so approving cannot silently widen it"
    );
}

// ══ 2. Fail closed ════════════════════════════════════════════════════════════

/// The live shape of a malformed dispatch: no `input` key at all. Reading the
/// whole payload instead of `payload["input"]` would make this read as "no calls
/// pending" and allow every tool.
#[tokio::test]
async fn a_payload_with_no_input_key_is_not_handed_out() {
    let backend = memory();
    let mut payload = dispatch_payload(calls_input(&["send_wire"]));
    payload.as_object_mut().unwrap().remove("input");
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        payload,
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    assert_eq!(violations(&backend, &execution_id).await.len(), 1);
}

#[tokio::test]
async fn a_non_object_input_is_not_handed_out() {
    for input in [json!("send_wire"), json!([{"name": "send_wire"}]), json!(7)] {
        let backend = memory();
        let (execution_id, _) = seed(
            &backend,
            dispatch_ir(true, &["send_wire"], &[]),
            "python_tool",
            dispatch_payload(input.clone()),
        )
        .await;

        let resp = claim(&make_state(backend.clone()), "python_tool").await;
        assert_eq!(
            resp.get("work_item"),
            None,
            "input {input} is unreadable and must not be handed out"
        );
        assert_eq!(violations(&backend, &execution_id).await.len(), 1);
    }
}

#[tokio::test]
async fn a_non_array_tool_calls_is_not_handed_out() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(json!({ "tool_calls": "send_wire" })),
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    assert_eq!(violations(&backend, &execution_id).await.len(), 1);
}

/// A call whose name cannot be read cannot be matched against `blocked_tools`,
/// so it must not run — otherwise omitting `name` is a policy bypass.
#[tokio::test]
async fn a_call_with_no_name_is_not_handed_out() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(json!({ "tool_calls": [{ "id": "c1", "arguments": {} }] })),
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    assert_eq!(violations(&backend, &execution_id).await.len(), 1);
}

/// If the IR cannot be resolved, the `agent_tool_dispatch` marker cannot be
/// read — so whether this item needs policy evaluation is UNKNOWN. Handing it
/// out would make "point the payload at a workflow that does not exist" a
/// complete bypass of the guard.
#[tokio::test]
async fn an_item_whose_workflow_cannot_be_resolved_is_not_handed_out() {
    let backend = memory();
    seed_item(
        &backend,
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

/// Same reasoning for an IR that is stored but unparseable.
#[tokio::test]
async fn an_item_whose_workflow_ir_is_corrupt_is_not_handed_out() {
    let backend = memory();
    seed(
        &backend,
        json!("this is not a workflow ir"),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

/// The node id is what selects the policy set and the dispatch marker. A node
/// that is not in the IR has neither, so it cannot be evaluated.
#[tokio::test]
async fn an_item_whose_node_is_absent_from_the_ir_is_not_handed_out() {
    let backend = memory();
    let mut payload = dispatch_payload(calls_input(&["send_wire"]));
    payload["node_id"] = json!("ghost");
    seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        payload,
    )
    .await;
    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

// ══ 2b. Policy authority — the execution, not the payload ════════════════════
//
// `POST /work-items` copies a caller-supplied `payload` and `node_id` verbatim
// into the queue behind the same write role as the claim route. If the payload
// chose which workflow's policy applied, that caller could simply name a
// workflow with no `blocked_tools` and walk the batch straight through a
// legitimate external worker. The execution record is engine-written, so it is
// the only trustworthy answer to "which policy governs this item".

/// The attack: the execution belongs to a policed workflow, but the payload
/// names an unpoliced one. Trusting the payload drops the workflow layer out of
/// the chain and allows `send_wire`.
#[tokio::test]
async fn a_payload_naming_a_different_workflow_than_the_execution_is_not_handed_out() {
    let backend = memory();
    // The workflow the EXECUTION belongs to: blocks send_wire.
    store(
        &backend,
        WF,
        VERSION,
        dispatch_ir(true, &["send_wire"], &[]),
    )
    .await;
    // A real, stored, but UNPOLICED workflow the attacker would rather be judged by.
    store(
        &backend,
        "unpoliced-wf",
        VERSION,
        dispatch_ir(true, &[], &[]),
    )
    .await;

    let mut payload = dispatch_payload(calls_input(&["send_wire"]));
    payload["workflow_id"] = json!("unpoliced-wf");
    seed_item(&backend, "python_tool", payload).await;

    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

/// The version variant: v1.0.0 is unpoliced, the execution is on v2.0.0 which
/// blocks `send_wire`. Trusting the payload evaluates the wrong version's rules.
#[tokio::test]
async fn a_payload_naming_a_different_workflow_version_is_not_handed_out() {
    let backend = memory();
    store(
        &backend,
        WF,
        "2.0.0",
        dispatch_ir(true, &["send_wire"], &[]),
    )
    .await;
    store(&backend, WF, VERSION, dispatch_ir(true, &[], &[])).await;

    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: WF.into(),
            workflow_version: "2.0.0".into(),
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
        .expect("create_execution");
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id,
            node_id: "n1".into(),
            queue_type: "python_tool".into(),
            // Claims v1.0.0, which is unpoliced.
            payload: dispatch_payload(calls_input(&["send_wire"])),
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

    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

/// With the coordinates gone from the payload entirely, the execution still
/// selects the policy. This is what proves the `unwrap_or("unknown")` /
/// `unwrap_or("1.0.0")` defaults are gone: under those, this item resolved no
/// IR at all and could never have been policed by the workflow that owns it.
#[tokio::test]
async fn an_item_with_no_payload_coordinates_is_still_policed_by_its_execution() {
    let backend = memory();
    let mut payload = dispatch_payload(calls_input(&["send_wire"]));
    let obj = payload.as_object_mut().unwrap();
    obj.remove("workflow_id");
    obj.remove("workflow_version");

    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &["send_wire"], &[]),
        "python_tool",
        payload,
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    assert_eq!(
        violations(&backend, &execution_id).await.len(),
        1,
        "the execution's workflow policy must have been the one that decided"
    );
}

/// No execution record means no authoritative coordinates, so nothing can be
/// evaluated.
#[tokio::test]
async fn an_item_whose_execution_is_missing_is_not_handed_out() {
    let backend = memory();
    store(
        &backend,
        WF,
        VERSION,
        dispatch_ir(true, &["send_wire"], &[]),
    )
    .await;
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            // An execution that was never created.
            execution_id: ExecutionId::new(),
            node_id: "n1".into(),
            queue_type: "python_tool".into(),
            payload: dispatch_payload(calls_input(&["send_wire"])),
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("enqueue_work_item");

    assert_withheld(&claim(&make_state(backend), "python_tool").await);
}

// ══ 3. Approval binding ═══════════════════════════════════════════════════════

/// The availability dual. Without this, an implementation that withheld
/// everything would pass every other enforcement test here.
#[tokio::test]
async fn a_matching_approval_releases_the_payload() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &[], &["send_wire"]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    append(
        &backend,
        &execution_id,
        EventKind::ToolApprovalRequired {
            node_id: "n1".into(),
            tool_name: "send_wire".into(),
            approver: "human".into(),
            context: json!({
                "node_id": "n1",
                "gated_tools": ["send_wire"],
                "calls": [{"name": "send_wire", "arguments": {}}],
                "calls_hash": calls_hash_for(&["send_wire"]),
            }),
        },
    )
    .await;
    append(
        &backend,
        &execution_id,
        EventKind::ApprovalReceived {
            node_id: "n1".into(),
            user_id: "risk-officer".into(),
            decision: ApprovalDecision::Approved,
            comment: None,
            state_patch: None,
        },
    )
    .await;

    let resp = claim(&make_state(backend.clone()), "python_tool").await;
    assert_handed_out(&resp, "n1");
    assert_eq!(
        resp["work_item"]["payload"]["input"]["tool_calls"][0]["name"],
        json!("send_wire"),
        "the approved batch must reach the worker intact"
    );
    assert!(violations(&backend, &execution_id).await.is_empty());
}

/// A settled approval authorises one call set, not the node forever. Otherwise
/// a human approving `read_file` would silently authorise a later `send_wire`.
#[tokio::test]
async fn an_approval_for_a_different_call_set_does_not_release_the_payload() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &[], &["send_wire", "read_file"]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    append(
        &backend,
        &execution_id,
        EventKind::ToolApprovalRequired {
            node_id: "n1".into(),
            tool_name: "read_file".into(),
            approver: "human".into(),
            context: json!({
                "node_id": "n1",
                "gated_tools": ["read_file"],
                "calls": [{"name": "read_file", "arguments": {}}],
                "calls_hash": calls_hash_for(&["read_file"]),
            }),
        },
    )
    .await;
    append(
        &backend,
        &execution_id,
        EventKind::ApprovalReceived {
            node_id: "n1".into(),
            user_id: "risk-officer".into(),
            decision: ApprovalDecision::Approved,
            comment: None,
            state_patch: None,
        },
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    let recorded = violations(&backend, &execution_id).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0],
        "approved tool calls do not match pending calls"
    );
}

// ══ 4. Narrowness — the route serves every queue ══════════════════════════════

/// An ordinary `python_fn` (not an ADK dispatch) carries no model-chosen calls,
/// so the guard must not touch it — even though it is on the same queue and its
/// payload holds a name a `blocked_tools` rule would match.
///
/// The coordinates matter: this fixture must NOT be the ADK dispatch coroutine.
/// A node at `jamjet.agents.tool_runtime::dispatch_tool_calls` IS an agent
/// dispatch whether or not it carries the marker, and is guarded on its
/// coordinates — see `an_unmarked_adk_dispatch_is_still_enforced` directly
/// below. Narrowness is about ordinary python functions, not about unmarked
/// dispatch nodes.
#[tokio::test]
async fn an_unmarked_python_fn_item_is_handed_out_untouched() {
    let backend = memory();
    let ordinary = ir_with_node(json!({
        "id": "n1",
        "kind": {
            "type": "python_fn",
            "module": "my_app.tasks",
            "function": "resize_image",
            "output_schema": "",
            "agent_tool_dispatch": false
        }
    }))
    .tap_policy(&["send_wire"], &[]);
    let (execution_id, _) = seed(
        &backend,
        ordinary,
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;

    let resp = claim(&make_state(backend.clone()), "python_tool").await;
    assert_handed_out(&resp, "n1");
    assert_eq!(
        resp["work_item"]["payload"],
        dispatch_payload(calls_input(&["send_wire"])),
        "the payload must come back byte-for-byte"
    );
    assert!(
        backend.get_events(&execution_id).await.unwrap().is_empty(),
        "an untouched item writes nothing"
    );
}

/// A workflow registered before the marker existed deserializes to
/// `agent_tool_dispatch: false`, and nothing re-validates an IR already in the
/// backend — `validate_agent_tool_dispatch` guards the REGISTRATION route only.
/// Keying enforcement on the marker alone would therefore hand this payload
/// straight out, with a `blocked_tools` rule in force and a blocked call inside
/// it: C1 still open, for exactly the workflows that predate the fix.
///
/// The dispatch coordinates identify the node without the marker, so the route
/// blocks it on those instead.
#[tokio::test]
async fn an_unmarked_adk_dispatch_is_still_enforced() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(false, &["send_wire"], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;

    assert_withheld(&claim(&make_state(backend.clone()), "python_tool").await);
    let recorded = violations(&backend, &execution_id).await;
    assert_eq!(
        recorded.len(),
        1,
        "the denial must reach the Prove surface like any other"
    );
    assert!(
        recorded[0].contains("send_wire"),
        "the audit rule must name the blocked tool, got {:?}",
        recorded[0]
    );
}

/// Every other queue — the regression that would break all durable tool
/// execution if the guard were not gated on the dispatch marker.
///
/// The workflow is POLICED and the payload has no `input` key, which is the
/// ordinary shape for a non-dispatch node. Without the marker gate the guard
/// would read the calls out of a `Null` input, fail closed on `NotAnObject`,
/// and terminally fail this item at claim time.
#[tokio::test]
async fn an_ordinary_item_on_another_queue_is_handed_out_untouched() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        policed_condition_ir(),
        "tool",
        json!({ "workflow_id": WF, "workflow_version": VERSION, "node_id": "n1" }),
    )
    .await;

    let resp = claim(&make_state(backend.clone()), "tool").await;
    assert_handed_out(&resp, "n1");
    assert_eq!(resp["work_item"]["queue_type"], json!("tool"));
    assert!(resp["work_item"]["lease_fence"].as_i64().unwrap() > 0);
    assert!(backend.get_events(&execution_id).await.unwrap().is_empty());
}

/// A marked dispatch with nothing to enforce runs. The guard short-circuits an
/// empty policy chain before it reads the payload, so an unpoliced workflow
/// never starts failing on payload shape.
#[tokio::test]
async fn a_marked_dispatch_under_an_empty_policy_chain_is_handed_out() {
    let backend = memory();
    let (execution_id, _) = seed(
        &backend,
        dispatch_ir(true, &[], &[]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;

    assert_handed_out(
        &claim(&make_state(backend.clone()), "python_tool").await,
        "n1",
    );
    assert!(backend.get_events(&execution_id).await.unwrap().is_empty());
}

// ══ 5. Unavailable — infrastructure failure is not a policy denial ════════════

/// An `InMemoryBackend` whose `get_events` fails ONCE, and only `get_events`.
///
/// This drives `DispatchGuardOutcome::Unavailable`: the engine could not read
/// its own approval state. That is infrastructure, not a decision — so the
/// route must withhold the payload WITHOUT auditing a denial no policy made and
/// WITHOUT settling an item it never evaluated. Every other method delegates to
/// a real backend, so "nothing was written" is an observation rather than an
/// artefact of a stub that cannot write.
struct FailFirstGetEvents {
    inner: InMemoryBackend,
    failed: std::sync::atomic::AtomicBool,
}

impl FailFirstGetEvents {
    fn new() -> Self {
        Self {
            inner: InMemoryBackend::new(),
            failed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl StateBackend for FailFirstGetEvents {
    // ── The one injected failure ──────────────────────────────────────────────

    async fn get_events(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(StateBackendError::Database(
                "injected: event log unreadable".into(),
            ));
        }
        self.inner.get_events(execution_id).await
    }

    // ── Everything else is the real backend ───────────────────────────────────

    async fn store_workflow(
        &self,
        def: WorkflowDefinition,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.store_workflow(def).await
    }

    async fn get_workflow(
        &self,
        workflow_id: &str,
        version: &str,
    ) -> jamjet_state::backend::BackendResult<Option<WorkflowDefinition>> {
        self.inner.get_workflow(workflow_id, version).await
    }

    async fn create_execution(
        &self,
        execution: WorkflowExecution,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.create_execution(execution).await
    }

    async fn get_execution(
        &self,
        id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<WorkflowExecution>> {
        self.inner.get_execution(id).await
    }

    async fn update_execution_status(
        &self,
        id: &ExecutionId,
        status: WorkflowStatus,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_execution_status(id, status).await
    }

    async fn update_execution_current_state(
        &self,
        id: &ExecutionId,
        current_state: &Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .update_execution_current_state(id, current_state)
            .await
    }

    async fn patch_append_array(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: Value,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .patch_append_array(execution_id, key, value)
            .await
    }

    async fn list_executions(
        &self,
        status: Option<WorkflowStatus>,
        limit: u32,
        offset: u32,
    ) -> jamjet_state::backend::BackendResult<Vec<WorkflowExecution>> {
        self.inner.list_executions(status, limit, offset).await
    }

    async fn append_event(
        &self,
        event: Event,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner.append_event(event).await
    }

    async fn get_events_since(
        &self,
        execution_id: &ExecutionId,
        since_sequence: jamjet_state::EventSequence,
    ) -> jamjet_state::backend::BackendResult<Vec<Event>> {
        self.inner
            .get_events_since(execution_id, since_sequence)
            .await
    }

    async fn latest_sequence(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner.latest_sequence(execution_id).await
    }

    async fn write_snapshot(
        &self,
        snapshot: jamjet_state::Snapshot,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.write_snapshot(snapshot).await
    }

    async fn latest_snapshot(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::Snapshot>> {
        self.inner.latest_snapshot(execution_id).await
    }

    async fn create_segment_atomic(
        &self,
        execution: WorkflowExecution,
        seed_snapshot: jamjet_state::Snapshot,
        started_event: EventKind,
        scheduled_event: EventKind,
        work_item: WorkItem,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .create_segment_atomic(
                execution,
                seed_snapshot,
                started_event,
                scheduled_event,
                work_item,
            )
            .await
    }

    async fn get_tool_effect(
        &self,
        key: &str,
    ) -> jamjet_state::backend::BackendResult<Option<Value>> {
        self.inner.get_tool_effect(key).await
    }

    async fn put_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::ArtifactRef> {
        self.inner.put_artifact(bytes, media_type).await
    }

    async fn get_artifact(
        &self,
        hash: &str,
    ) -> jamjet_state::backend::BackendResult<Option<Vec<u8>>> {
        self.inner.get_artifact(hash).await
    }

    async fn enqueue_work_item(
        &self,
        item: WorkItem,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::WorkItemId> {
        self.inner.enqueue_work_item(item).await
    }

    async fn claim_work_item(
        &self,
        worker_id: &str,
        queue_types: &[&str],
    ) -> jamjet_state::backend::BackendResult<Option<WorkItem>> {
        self.inner.claim_work_item(worker_id, queue_types).await
    }

    async fn renew_lease(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        worker_id: &str,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .renew_lease(item_id, worker_id, lease_fence)
            .await
    }

    async fn complete_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.complete_work_item(item_id).await
    }

    async fn complete_work_item_fenced(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .complete_work_item_fenced(item_id, lease_fence)
            .await
    }

    async fn fail_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        error: &str,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.fail_work_item(item_id, error).await
    }

    async fn commit_turn(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        terminal_event: Event,
        write_snapshot: bool,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::EventSequence> {
        self.inner
            .commit_turn(item_id, lease_fence, terminal_event, write_snapshot)
            .await
    }

    async fn park_work_item(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
        retry_after: &str,
        next_attempt: u32,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .park_work_item(item_id, lease_fence, retry_after, next_attempt)
            .await
    }

    async fn finalize_rollover_fenced(
        &self,
        execution_id: &ExecutionId,
        work_item_id: jamjet_state::backend::WorkItemId,
        lease_fence: i64,
    ) -> jamjet_state::backend::BackendResult<bool> {
        self.inner
            .finalize_rollover_fenced(execution_id, work_item_id, lease_fence)
            .await
    }

    async fn reclaim_expired_leases(
        &self,
    ) -> jamjet_state::backend::BackendResult<jamjet_state::backend::ReclaimResult> {
        self.inner.reclaim_expired_leases().await
    }

    async fn move_to_dead_letter(
        &self,
        item_id: jamjet_state::backend::WorkItemId,
        last_error: &str,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.move_to_dead_letter(item_id, last_error).await
    }

    async fn create_token(
        &self,
        name: &str,
        role: &str,
    ) -> jamjet_state::backend::BackendResult<(String, jamjet_state::backend::ApiToken)> {
        self.inner.create_token(name, role).await
    }

    async fn validate_token(
        &self,
        token: &str,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::backend::ApiToken>> {
        self.inner.validate_token(token).await
    }

    async fn apply_approval_projection(
        &self,
        row: jamjet_state::ApprovalProjectionRow,
        projection_name: &str,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .apply_approval_projection(row, projection_name, new_checkpoint)
            .await
    }

    async fn apply_approval_projection_batch(
        &self,
        rows: Vec<jamjet_state::ApprovalProjectionRow>,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .apply_approval_projection_batch(rows, projection_name, execution_id, new_checkpoint)
            .await
    }

    async fn get_approval_projection(
        &self,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<Vec<jamjet_state::ApprovalProjectionRow>> {
        self.inner.get_approval_projection(execution_id).await
    }

    async fn get_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
    ) -> jamjet_state::backend::BackendResult<i64> {
        self.inner
            .get_projector_checkpoint(projection_name, execution_id)
            .await
    }

    async fn set_projector_checkpoint(
        &self,
        projection_name: &str,
        execution_id: &ExecutionId,
        new_checkpoint: i64,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner
            .set_projector_checkpoint(projection_name, execution_id, new_checkpoint)
            .await
    }

    async fn create_tenant(
        &self,
        tenant: jamjet_state::Tenant,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.create_tenant(tenant).await
    }

    async fn get_tenant(
        &self,
        id: &jamjet_state::TenantId,
    ) -> jamjet_state::backend::BackendResult<Option<jamjet_state::Tenant>> {
        self.inner.get_tenant(id).await
    }

    async fn list_tenants(
        &self,
    ) -> jamjet_state::backend::BackendResult<Vec<jamjet_state::Tenant>> {
        self.inner.list_tenants().await
    }

    async fn update_tenant(
        &self,
        tenant: jamjet_state::Tenant,
    ) -> jamjet_state::backend::BackendResult<()> {
        self.inner.update_tenant(tenant).await
    }
}

/// A backend failure must withhold the payload, audit NOTHING, and settle
/// NOTHING.
///
/// Auditing here would put a denial on the Prove surface that no policy made,
/// and failing the item would make a transient outage permanently kill work the
/// engine never evaluated. Leaving the item leased is the fail-closed choice:
/// the lease expires, the reclaimer returns it to the queue, and the next claim
/// decides properly.
#[tokio::test]
async fn a_backend_failure_withholds_without_auditing_or_settling() {
    let backend: Arc<dyn StateBackend> = Arc::new(FailFirstGetEvents::new());
    let (execution_id, item_id) = seed(
        &backend,
        dispatch_ir(true, &[], &["send_wire"]),
        "python_tool",
        dispatch_payload(calls_input(&["send_wire"])),
    )
    .await;
    let state = make_state(backend.clone());

    assert_withheld(&claim(&state, "python_tool").await);

    assert!(
        violations(&backend, &execution_id).await.is_empty(),
        "no policy denied — a PolicyViolation here is a false denial"
    );
    assert!(
        approval_requests(&backend, &execution_id).await.is_empty(),
        "an undecided dispatch requests nothing"
    );

    // The item must still exist (not completed). `renew_lease` reports
    // `NotFound` only for an item that is gone; a live item held by another
    // worker reports `FenceLost`, so this distinguishes the two.
    if let Err(StateBackendError::NotFound(_)) =
        backend.renew_lease(item_id, "someone-else", 0).await
    {
        panic!("the item was settled as complete despite never being evaluated");
    }

    // … and must still be leased, not returned to the queue as failed. The
    // injected failure has healed, so a re-claimable item would now produce an
    // approval request; silence proves the lease was left intact.
    assert_withheld(&claim(&state, "python_tool").await);
    assert!(
        approval_requests(&backend, &execution_id).await.is_empty(),
        "the item was failed back onto the queue instead of being left leased"
    );
}

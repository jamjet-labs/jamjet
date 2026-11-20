//! What the store keeps is what it was given. A JSON value reads back equal,
//! and where the store keeps it as text, the text is byte for byte what
//! `serde_json` wrote: an event of every kind, a NUL inside a string, an
//! event of 16 MiB. A page of events keeps its byte budget when the text is
//! multibyte.

use crate::conformance::cases::support::in_tenant;
use crate::conformance::{Fixture, StoredJson};
use chrono::{DateTime, TimeZone, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::event::{ApprovalDecision, DEADLINE_PRINCIPAL};
use jamjet_state::{Event, EventKind, ProvenanceMetadata, StateBackend, WorkItem, DEFAULT_TENANT};
use serde_json::json;
use std::sync::Arc;

/// The backend under test, the scoped stores in a tenant other than the
/// default.
async fn backend(fx: &Fixture) -> (String, Arc<dyn StateBackend>) {
    (format!("{:?}", fx.kind), in_tenant(fx, "acme").await)
}

fn run(id: &ExecutionId, input: serde_json::Value) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-payloads".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: input,
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

async fn new_run(b: &dyn StateBackend) -> ExecutionId {
    let id = ExecutionId::new();
    b.create_execution(run(&id, json!({}))).await.unwrap();
    id
}

/// The run's events, each as `serde_json` writes its kind.
async fn kinds_as_text(b: &dyn StateBackend, id: &ExecutionId) -> Vec<(i64, String)> {
    b.get_events(id)
        .await
        .unwrap()
        .iter()
        .map(|e| (e.sequence, serde_json::to_string(&e.kind).unwrap()))
        .collect()
}

fn event_text(id: &ExecutionId, sequence: i64) -> StoredJson {
    StoredJson::Event {
        execution: id.clone(),
        sequence,
    }
}

/// A NUL inside a string of an event, of a work item's payload and of a
/// run's input. `serde_json` writes it as `\u0000`, which a `jsonb` column
/// refuses; the stored text is what `serde_json` wrote, and each value reads
/// back equal.
pub async fn nul_in_json_round_trips_byte_equal(fx: &Fixture) {
    let (name, b) = backend(fx).await;
    let with_nul = "before\u{0}after";
    let value = json!({ "note": with_nul });
    let text = serde_json::to_string(&value).unwrap();
    assert!(
        text.contains("\\u0000"),
        "serde_json escapes the NUL: {text}"
    );

    let id = ExecutionId::new();
    b.create_execution(run(&id, value.clone())).await.unwrap();
    let read = b.get_execution(&id).await.unwrap().expect("the run");
    assert_eq!(read.initial_input, value, "{name}: the run's input");
    if let Some(stored) = fx.hooks.stored_json(StoredJson::RunInput(id.clone())).await {
        assert_eq!(stored, text, "{name}: the run's input as stored");
    }

    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "n".into(),
        queue_type: "general".into(),
        payload: value.clone(),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    let item_id = b.enqueue_work_item(item).await.unwrap();
    let read = b.get_work_item(item_id).await.unwrap().expect("the item");
    assert_eq!(read.payload, value, "{name}: the item's payload");
    if let Some(stored) = fx.hooks.stored_json(StoredJson::ItemPayload(item_id)).await {
        assert_eq!(stored, text, "{name}: the item's payload as stored");
    }

    let kind = EventKind::NodeSkipped {
        node_id: "n".into(),
        reason: with_nul.into(),
    };
    let kind_text = serde_json::to_string(&kind).unwrap();
    assert!(kind_text.contains("\\u0000"), "{kind_text}");
    let seq = b
        .append_event(Event::new(id.clone(), 0, kind))
        .await
        .unwrap();
    assert_eq!(
        kinds_as_text(&*b, &id).await,
        vec![(seq, kind_text.clone())],
        "{name}: the event"
    );
    if let Some(stored) = fx.hooks.stored_json(event_text(&id, seq)).await {
        assert_eq!(stored, kind_text, "{name}: the event as stored");
    }
}

/// A time with nanoseconds, which RFC 3339 text keeps.
fn at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::nanoseconds(123_456_789)
}

fn an_id() -> uuid::Uuid {
    uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)
}

/// `every_kind()`, from one sample per struct variant of `EventKind`. The
/// macro also writes a `match` over `EventKind` with one arm per sample and
/// no catch-all, so a variant added to `EventKind` and not here does not
/// compile.
macro_rules! one_of_each {
    ($($variant:ident $fields:tt),* $(,)?) => {
        fn every_kind() -> Vec<EventKind> {
            fn listed(kind: &EventKind) {
                match kind {
                    $(EventKind::$variant { .. } => {})*
                    // The one unit variant.
                    EventKind::Unknown => {}
                }
            }
            let kinds = vec![$(EventKind::$variant $fields,)* EventKind::Unknown];
            kinds.iter().for_each(listed);
            kinds
        }
    };
}

one_of_each! {
    WorkflowStarted {
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        initial_input: json!({ "order": 7 }),
        initiated_by: Some("token:t1".into()),
        run_key: None,
    },
    WorkflowCompleted { final_state: json!({ "done": true }) },
    WorkflowFailed { error: "boom".into() },
    WorkflowCancelled { reason: Some("operator".into()) },
    NodeScheduled { node_id: "n".into(), queue_type: "general".into() },
    NodeStarted { node_id: "n".into(), worker_id: "w1".into(), attempt: 1 },
    NodeCompleted {
        node_id: "n".into(),
        output: json!({ "answer": 42, "items": [1, 2.5, "three", null] }),
        state_patch: json!({ "k": "v" }),
        duration_ms: 12,
        gen_ai_system: Some("provider".into()),
        gen_ai_model: Some("model-1".into()),
        input_tokens: Some(10),
        output_tokens: Some(20),
        finish_reason: Some("stop".into()),
        cost_usd: Some(0.25),
        provenance: Some(Box::new(ProvenanceMetadata {
            model_id: Some("model-1".into()),
            model_version: Some("1".into()),
            confidence: Some(0.5),
            verified: true,
            source: Some("tool:search".into()),
            trust_domain: Some("internal".into()),
            evidence_refs: vec!["doc:1".into()],
        })),
        idempotency_key: Some("key-1".into()),
    },
    NodeFailed { node_id: "n".into(), error: "boom".into(), attempt: 2, retryable: true },
    NodeParked { node_id: "n".into(), attempt: 1, retry_after: at().to_rfc3339() },
    NodeSkipped { node_id: "n".into(), reason: "branch not taken".into() },
    WorkItemRefused { node_id: "n".into(), work_item_id: an_id(), reason: "stray".into() },
    NodeCancelled { node_id: "n".into() },
    RetryScheduled { node_id: "n".into(), attempt: 2, delay_ms: 500 },
    InterruptRaised {
        node_id: "n".into(),
        reason: "review".into(),
        state_for_review: json!({ "draft": "text" }),
    },
    ApprovalReceived {
        node_id: "n".into(),
        user_id: "alice".into(),
        decision: ApprovalDecision::Approved,
        comment: Some("fine".into()),
        state_patch: Some(json!({ "k": 1 })),
        principal: Some("token:t1".into()),
        approved_digest: Some("digest".into()),
        request_sequence: Some(3),
        choice: Some("option_a".into()),
    },
    TimerCreated { node_id: "n".into(), fire_at: at(), correlation_key: Some("ck".into()) },
    TimerFired { node_id: "n".into(), correlation_key: None },
    ExternalEventReceived { correlation_key: "ck".into(), payload: json!({ "x": 1 }) },
    ChildWorkflowStarted {
        node_id: "n".into(),
        child_execution_id: "child".into(),
        child_workflow_id: "wf-child".into(),
    },
    ChildWorkflowCompleted {
        node_id: "n".into(),
        child_execution_id: "child".into(),
        result: json!({ "ok": true }),
    },
    ChildWorkflowFailed {
        node_id: "n".into(),
        child_execution_id: "child".into(),
        error: "boom".into(),
    },
    BudgetExceeded { node_id: "n".into(), kind: "steps".into(), limit: 10, current: 11 },
    TokenBudgetExceeded {
        node_id: "n".into(),
        kind: "total_tokens".into(),
        limit: 100,
        current: 0,
        reason: Some("tokens_not_reported".into()),
    },
    CostBudgetExceeded {
        node_id: "n".into(),
        limit_usd: 1.5,
        current_usd: 0.75,
        reason: Some("cost_not_reported".into()),
    },
    AutonomyLimitReached {
        node_id: "n".into(),
        agent_ref: "agent".into(),
        limit_type: "max_iterations".into(),
        limit_value: json!(5),
        actual_value: json!(6),
    },
    CircuitBreakerTripped {
        node_id: "n".into(),
        agent_ref: "agent".into(),
        consecutive_errors: 3,
        threshold: 3,
    },
    AgentStepAdmitted {
        node_id: "n".into(),
        work_item_id: Some(an_id()),
        agents: vec!["agent".into()],
        tool_calls: 1,
        approval_sequence: Some(4),
    },
    EscalationRequired {
        node_id: "n".into(),
        agent_ref: "agent".into(),
        reason: "circuit_breaker".into(),
        escalation_target: "human_approval".into(),
    },
    PolicyViolation {
        node_id: "n".into(),
        rule: "block_tool:payments.*".into(),
        decision: "blocked".into(),
        policy_scope: "global".into(),
    },
    ToolApprovalRequired {
        node_id: "n".into(),
        tool_name: "tool".into(),
        approver: "human".into(),
        context: json!({ "expires_at": at() }),
    },
    ApprovalExpired {
        node_id: "n".into(),
        request_sequence: 3,
        expires_at: at(),
        principal: DEADLINE_PRINCIPAL.into(),
    },
    ApprovalEscalated {
        node_id: "n".into(),
        request_sequence: 3,
        escalate_at: at(),
        expires_at: at(),
    },
    AuditSealFailed {
        event_id: an_id(),
        sequence: 2,
        event_type: "node_completed".into(),
        error: "disk full".into(),
    },
    RunStalled { reason: "an event kind this build cannot read".into() },
    StrategyStarted { strategy: "react".into(), config: json!({ "max_iterations": 3 }) },
    PlanGenerated { steps: vec!["one".into(), "two".into()] },
    IterationStarted { iteration: 1 },
    ToolCalled { node_id: "n".into(), tool: "search".into() },
    CriticVerdict { node_id: "n".into(), score: 0.75, passed: true, feedback: Some("ok".into()) },
    IterationCompleted {
        iteration: 1,
        cost_delta_usd: Some(0.125),
        input_tokens: 10,
        output_tokens: 20,
    },
    StrategyLimitHit {
        limit_type: "max_iterations".into(),
        limit_value: json!(3),
        actual_value: json!(4),
    },
    StrategyCompleted { iterations: 3, total_cost_usd: None },
    CoordinatorDiscovery {
        node_id: "n".into(),
        query_skills: vec!["search".into()],
        query_trust_domain: Some("internal".into()),
        candidates: vec![json!({ "id": "a" })],
        filtered_out: vec![json!({ "id": "b" })],
    },
    CoordinatorScoring {
        node_id: "n".into(),
        rankings: vec![json!({ "id": "a", "score": 1 })],
        spread: 0.5,
        weights: json!({ "cost": 1 }),
    },
    CoordinatorDecision {
        node_id: "n".into(),
        selected: Some("a".into()),
        method: "score".into(),
        reasoning: Some("best".into()),
        confidence: 0.75,
        rejected: vec![json!({ "id": "b" })],
        tiebreaker_tokens: Some(json!({ "in": 1 })),
        tiebreaker_cost: Some(0.25),
    },
    AgentToolInvoked {
        node_id: "n".into(),
        agent_uri: "agent://a".into(),
        mode: "sync".into(),
        protocol: "http".into(),
        input_hash: "hash".into(),
    },
    AgentToolProgress {
        node_id: "n".into(),
        chunk_index: 1,
        partial_output_summary: "half".into(),
    },
    AgentToolTurn {
        node_id: "n".into(),
        turn_number: 1,
        direction: "out".into(),
        content_summary: "hello".into(),
        tokens: 5,
        cost: 0.25,
    },
    AgentToolCompleted {
        node_id: "n".into(),
        output: json!({ "ok": true }),
        provenance: Some(json!({ "by": "a" })),
        total_cost: 0.5,
        latency_ms: 30,
        total_turns: Some(2),
    },
    AgentToolTerminated {
        node_id: "n".into(),
        reason: "timeout".into(),
        chunks_received: 2,
        partial_output: Some(json!("part")),
        cost: 0.25,
    },
    AgentToolFailed {
        node_id: "n".into(),
        failure_type: "timeout".into(),
        message: "slow".into(),
        retryable: true,
    },
    SegmentBoundary { segment_number: 1, next_execution_id: "next".into() },
}

/// One event of every `EventKind` variant: the stored text is what
/// `serde_json` wrote for it, and the event reads back as it went in. On
/// SQLite this pins the stored event format.
pub async fn event_json_reads_back_byte_equal(fx: &Fixture) {
    let (name, b) = backend(fx).await;
    let id = new_run(&*b).await;
    let mut sent = Vec::new();
    for kind in every_kind() {
        let text = serde_json::to_string(&kind).unwrap();
        let seq = b
            .append_event(Event::new(id.clone(), 0, kind))
            .await
            .unwrap();
        sent.push((seq, text));
    }
    assert_eq!(kinds_as_text(&*b, &id).await, sent, "{name}: read back");
    for (seq, text) in &sent {
        if let Some(stored) = fx.hooks.stored_json(event_text(&id, *seq)).await {
            assert_eq!(&stored, text, "{name}: the event at {seq} as stored");
        }
    }
}

/// An event of 16 MiB is stored and read back whole.
pub async fn a_16_mib_event_round_trips(fx: &Fixture) {
    const SIZE: usize = 16 * 1024 * 1024;
    let (name, b) = backend(fx).await;
    let id = new_run(&*b).await;
    let kind = EventKind::NodeSkipped {
        node_id: "n".into(),
        reason: "x".repeat(SIZE),
    };
    let text = serde_json::to_string(&kind).unwrap();
    assert!(text.len() > SIZE);
    let seq = b
        .append_event(Event::new(id.clone(), 0, kind))
        .await
        .unwrap();
    // Compared without printing 16 MiB on a failure.
    let read = kinds_as_text(&*b, &id).await;
    let lengths: Vec<(i64, usize)> = read.iter().map(|(s, t)| (*s, t.len())).collect();
    assert_eq!(lengths, vec![(seq, text.len())], "{name}: one event, whole");
    assert!(read[0].1 == text, "{name}: the event read back differs");
    if let Some(stored) = fx.hooks.stored_json(event_text(&id, seq)).await {
        assert_eq!(stored.len(), text.len(), "{name}: stored length");
        assert!(stored == text, "{name}: the stored text differs");
    }
}

/// Twenty events of 100 KiB each, all in three-byte characters, read as a
/// page with a 512 KiB budget: the page stops at the event at which its
/// serialized size reaches the budget, so the page without its last event
/// is under the budget. An estimate that counts characters and not bytes
/// reads three times too far.
pub async fn events_page_keeps_its_byte_budget_with_multibyte_text(fx: &Fixture) {
    const BUDGET: usize = 512 * 1024;
    let (name, b) = backend(fx).await;
    let id = new_run(&*b).await;
    // U+20AC is three bytes in UTF-8.
    let text: String = std::iter::repeat_n('\u{20ac}', 100 * 1024 / 3).collect();
    for n in 0..20 {
        let kind = EventKind::NodeSkipped {
            node_id: format!("n{n}"),
            reason: text.clone(),
        };
        b.append_event(Event::new(id.clone(), 0, kind))
            .await
            .unwrap();
    }
    let page = b.get_events_page(&id, 0, 100, BUDGET).await.unwrap();
    let sizes: Vec<usize> = page
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap().len())
        .collect();
    let (last, before) = sizes.split_last().expect("a page holds at least one event");
    let before: usize = before.iter().sum();
    assert!(
        before < BUDGET,
        "{name}: {} events, {before} bytes before the last ({last} bytes), budget {BUDGET}",
        page.len()
    );
}

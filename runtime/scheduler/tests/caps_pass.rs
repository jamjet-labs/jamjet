//! The scheduler and a run's caps.
//!
//! A step that is claimed under the cap may report more than the cap has
//! left. Its completion is recorded, since the step ran, and then the run is
//! past its cap. The scheduler's next pass must fail the run before it
//! dispatches anything after that step, whichever transport ran it: the
//! in-process worker reports a model, tokens and its idempotency key, an
//! HTTP worker reports what it reports (tokens, a cost, or both).

mod support;

use jamjet_core::workflow::WorkflowStatus;
use jamjet_scheduler::Scheduler;
use jamjet_state::{Event, EventKind};
use serde_json::{json, Value};
use support::{kinds, linear, start_run, status, stores, Worker, QUEUE};

/// Which cap the run carries.
#[derive(Clone, Copy, Debug)]
enum Cap {
    /// `token_budget: {total_tokens: 5}`.
    Tokens,
    /// `cost_budget_usd: 1.0`.
    Dollars,
}

/// How the step's completion reads in the log.
#[derive(Clone, Copy, Debug)]
enum Transport {
    /// As the in-process worker writes it.
    InProcess,
    /// As `POST /work-items/:id/complete` writes it.
    Http,
}

fn capped(cap: Cap, mut ir: Value) -> Value {
    match cap {
        Cap::Tokens => ir["token_budget"] = json!({ "total_tokens": 5 }),
        Cap::Dollars => ir["cost_budget_usd"] = json!(1.0),
    }
    ir
}

/// The completion of `node`, spending past the cap: 10 tokens against 5,
/// or $1.50 against $1.00.
fn spent(cap: Cap, transport: Transport, node: &str, key: String) -> EventKind {
    let cost = match cap {
        Cap::Tokens => None,
        Cap::Dollars => Some(1.5),
    };
    let (system, model, input, output) = match transport {
        Transport::InProcess => (
            Some("anthropic".to_string()),
            Some("claude-sonnet-4-6".to_string()),
            Some(7),
            Some(3),
        ),
        // An HTTP worker under a dollar cap may report a cost and no tokens.
        Transport::Http => match cap {
            Cap::Tokens => (None, None, Some(10), None),
            Cap::Dollars => (None, None, None, None),
        },
    };
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({}),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: system,
        gen_ai_model: model,
        input_tokens: input,
        output_tokens: output,
        finish_reason: None,
        cost_usd: cost,
        provenance: None,
        idempotency_key: Some(key),
    }
}

/// The trip event the pass records, as JSON.
fn trip(cap: Cap) -> Value {
    match cap {
        Cap::Tokens => json!({
            "type": "token_budget_exceeded",
            "node_id": "a",
            "kind": "total_tokens",
            "limit": 5,
            "current": 10
        }),
        Cap::Dollars => json!({
            "type": "cost_budget_exceeded",
            "node_id": "a",
            "limit_usd": 1.0,
            "current_usd": 1.5
        }),
    }
}

/// `a -> b`. `a` is claimed under the cap and completes past it. The next
/// pass records the trip and fails the run in the same write: nothing is
/// dispatched, failed or skipped after `a`'s completion, and `b` never gets
/// an item.
#[tokio::test]
async fn a_run_past_its_cap_fails_before_any_successor_is_dispatched() {
    for cap in [Cap::Tokens, Cap::Dollars] {
        for transport in [Transport::InProcess, Transport::Http] {
            for store in stores().await {
                let case = format!("{} {cap:?} {transport:?}", store.name());
                let backend = store.backend();
                let eid = start_run(backend.as_ref(), capped(cap, linear(&["a", "b"]))).await;

                let item = backend
                    .claim_work_item("test-worker", &[QUEUE])
                    .await
                    .unwrap()
                    .expect("a's item");
                assert_eq!(item.node_id, "a", "{case}");
                let key = jamjet_state::derive_idempotency_key(backend.as_ref(), &eid, "a")
                    .await
                    .unwrap();
                backend
                    .commit_turn(
                        item.id,
                        item.lease_fence,
                        Event::new(eid.clone(), 0, spent(cap, transport, "a", key)),
                        false,
                    )
                    .await
                    .unwrap();

                let scheduler = Scheduler::new(backend.clone());
                let worker = Worker::default();
                for _ in 0..4 {
                    scheduler.tick().await.unwrap();
                    worker.complete_all(backend.as_ref(), |_| json!({})).await;
                }

                let events = backend.get_events(&eid).await.unwrap();
                let done = events
                    .iter()
                    .position(|e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "a"))
                    .expect("a's completion");
                let after: Vec<Value> = events[done + 1..]
                    .iter()
                    .map(|e| serde_json::to_value(&e.kind).unwrap())
                    .collect();
                assert_eq!(
                    after.iter().map(|k| k["type"].clone()).collect::<Vec<_>>(),
                    vec![trip(cap)["type"].clone(), json!("workflow_failed")],
                    "{case}: after a run passes its cap the next pass fails it and does nothing \
                     else; the log was {:?}",
                    kinds(&events)
                );
                assert_eq!(after[0], trip(cap), "{case}");
                assert_eq!(
                    status(backend.as_ref(), &eid).await,
                    WorkflowStatus::Failed,
                    "{case}"
                );
                assert!(
                    worker.items_of("b").is_empty(),
                    "{case}: b was dispatched on a run past its cap"
                );
            }
        }
    }
}

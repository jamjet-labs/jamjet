//! The snapshot fold without a backend: `Snapshot::new`'s defaults, and the
//! replay-equivalence property (a fold seeded from any mid-run snapshot
//! equals the fold over the whole log).
//!
//! The `commit_turn` cases, which write and read a store, are in the
//! backend suite (`runtime/testkit`, module `commit_turn`).

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::{
    apply_events, apply_events_seeded,
    event::{Event, EventKind},
    snapshot::Snapshot,
    MaterializedState,
};
use proptest::prelude::*;
use serde_json::json;

/// Snapshot::new (compat path) still compiles and returns sane defaults.
#[tokio::test]
async fn test_snapshot_new_compat_defaults() {
    let exec_id = ExecutionId::new();
    let snap = Snapshot::new(exec_id.clone(), 5, json!({"nodes_completed": ["a", "b"]}));
    assert_eq!(snap.at_sequence, 5);
    assert_eq!(snap.last_sequence, 5);
    assert_eq!(snap.status, WorkflowStatus::Pending);
    assert!(snap.completed_nodes.is_empty());
    assert!(snap.active_nodes.is_empty());
}

// ── Task 5 helpers ────────────────────────────────────────────────────────────

fn arb_nc_spec() -> impl Strategy<Value = (usize, usize, i64)> {
    // (node_pool_index 0-3, key_pool_index 0-3, patch_value 0-99)
    (0..4usize, 0..4usize, 0i64..100i64)
}

/// Build a NodeCompleted event kind for use in the proptest.
fn nc_kind_for_spec(ni: usize, ki: usize, pv: i64) -> EventKind {
    let node_pool = ["n0", "n1", "n2", "n3"];
    let key_pool = ["a", "b", "c", "d"];
    EventKind::NodeCompleted {
        node_id: node_pool[ni].to_string(),
        output: json!({"ok": true}),
        state_patch: json!({key_pool[ki]: pv}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

// ── Group 1: Replay-equivalence proptest ─────────────────────────────────────
//
// Generates random sequences of NodeCompleted events with repeated node ids
// and small JSON patches. For EVERY cut K in 0..=N, asserts that:
//   apply_events_seeded(Snapshot::from_materialized(fold[0..K]), events[K..])
//       == apply_events(initial, &all_events)
// on all four fields: current_state, status, completed_nodes, active_nodes.
//
// This is a pure fold (no DB), so it can run synchronously.
proptest! {
    #[test]
    fn replay_equivalence(
        specs in prop::collection::vec(arb_nc_spec(), 1..=12)
    ) {
        let exec_id = ExecutionId::new();
        let events: Vec<Event> = specs
            .iter()
            .enumerate()
            .map(|(i, &(ni, ki, pv))| {
                Event::new(exec_id.clone(), (i + 1) as i64, nc_kind_for_spec(ni, ki, pv))
            })
            .collect();

        let initial = json!({});
        let full = apply_events(initial.clone(), &events, &WorkflowStatus::Running);

        for k in 0..=events.len() {
            // Fold events[0..k] to get the mid-run materialized state.
            let mid = apply_events(initial.clone(), &events[0..k], &WorkflowStatus::Running);

            // Mirror what the worker does: build a Snapshot from that state.
            let snap = Snapshot::from_materialized(exec_id.clone(), &mid);

            // Seed a fresh MaterializedState from the snapshot and fold the tail.
            let seed = MaterializedState {
                current_state: snap.state,
                status: snap.status,
                completed_nodes: snap.completed_nodes,
                active_nodes: snap.active_nodes,
                last_sequence: snap.last_sequence,
            };
            let resumed = apply_events_seeded(seed, &events[k..]);

            prop_assert_eq!(
                &resumed.current_state,
                &full.current_state,
                "current_state mismatch at k={}",
                k
            );
            prop_assert_eq!(
                resumed.status,
                full.status.clone(),
                "status mismatch at k={}",
                k
            );
            prop_assert_eq!(
                &resumed.completed_nodes,
                &full.completed_nodes,
                "completed_nodes mismatch at k={} (pre-snapshot node lost?)",
                k
            );
            prop_assert_eq!(
                &resumed.active_nodes,
                &full.active_nodes,
                "active_nodes mismatch at k={}",
                k
            );
        }
    }
}

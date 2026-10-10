//! `POST /workflows` cannot change a registered version.
//!
//! Re-registering a version used to overwrite its IR, so a developer could
//! drop the approval gate from a live workflow (the worker loads the IR per
//! item) or swap a node's body under an approval that still matched
//! (approval-integrity review, C2). A byte-identical re-register is still
//! accepted as a no-op, because deployments re-register on every boot.

use axum::http::StatusCode;
use serde_json::{json, Value};

mod support;

fn ir(policy: Value) -> Value {
    json!({
        "workflow_id": "wf-imm",
        "version": "1.0.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gate",
        "nodes": {
            "gate": {
                "id": "gate",
                "kind": { "type": "tool", "tool_ref": "payments.transfer", "input_mapping": {}, "output_schema": "" },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": policy
            }
        },
        "edges": [ { "from": "gate", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

fn gated() -> Value {
    ir(
        json!({ "blocked_tools": [], "require_approval_for": ["payments.*"], "model_allowlist": [] }),
    )
}

#[tokio::test]
async fn an_identical_re_register_is_accepted() {
    for tb in support::backends().await {
        let state = support::make_state(tb.dyn_backend());
        let (st, body) = support::post(&state, "/workflows", json!({ "ir": gated() })).await;
        assert!(st.is_success(), "{}: {st} {body}", tb.name());
        let (st, body) = support::post(&state, "/workflows", json!({ "ir": gated() })).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", tb.name());
        assert_eq!(body["workflow_id"], "wf-imm");
    }
}

#[tokio::test]
async fn re_registering_without_the_policy_is_refused_and_the_gate_still_holds() {
    for tb in support::backends().await {
        let backend = tb.dyn_backend();
        let state = support::make_state(backend.clone());
        let (st, _) = support::post(&state, "/workflows", json!({ "ir": gated() })).await;
        assert!(st.is_success());

        let (st, body) =
            support::post(&state, "/workflows", json!({ "ir": ir(json!(null)) })).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", tb.name());

        let stored = backend
            .get_workflow("wf-imm", "1.0.0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.ir,
            gated(),
            "{}: the stored IR must be unchanged",
            tb.name()
        );

        // The live definition still gates the node.
        let (st, body) = support::post(
            &state,
            "/executions",
            json!({ "workflow_id": "wf-imm", "workflow_version": "1.0.0", "input": {} }),
        )
        .await;
        assert!(st.is_success(), "{body}");
        let (st, claimed) = support::post(
            &state,
            "/work-items/claim",
            json!({ "worker_id": "w", "queue_types": ["tool"] }),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            claimed,
            json!({ "claimed": false }),
            "{}: the gate was dropped",
            tb.name()
        );
    }
}

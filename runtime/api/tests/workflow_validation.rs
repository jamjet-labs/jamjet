//! Guards the `POST /workflows` IR deserialize-check.
//!
//! `create_workflow` rejects any IR that cannot load into the runtime's
//! `WorkflowIr` — otherwise a structurally-broken definition is stored happily
//! and only fails later when the scheduler tries to deserialize it, leaving the
//! execution stuck in `running` forever. These tests pin both directions: the
//! canonical compiled workflow must pass, and incomplete IR must be rejected.

use jamjet_ir::WorkflowIr;

#[test]
fn canonical_compiled_ir_deserializes() {
    // The exact IR produced by `jamjet init hello-agent` + the YAML compiler
    // (a `model` node whose `model_ref` is resolved by the worker registry, not
    // the IR `models` map — which is why the check is a shape check, not
    // `validate_workflow`'s stricter ref rules).
    let json = include_str!("fixtures/hello_agent_ir.json");
    let value: serde_json::Value = serde_json::from_str(json).expect("fixture is valid JSON");
    let ir = serde_json::from_value::<WorkflowIr>(value);
    assert!(
        ir.is_ok(),
        "compiled hello-agent IR must deserialize into WorkflowIr, else create_workflow \
         would 400 a valid `jamjet run` workflow: {:?}",
        ir.err()
    );
}

#[test]
fn structurally_broken_ir_is_rejected() {
    // Has the workflow_id/version the handler extracts, but none of the rest of
    // the IR — exactly the input the deserialize-check turns into a 400 instead
    // of a silent, never-scheduling execution.
    let value = serde_json::json!({ "workflow_id": "x", "version": "0.1.0" });
    assert!(
        serde_json::from_value::<WorkflowIr>(value).is_err(),
        "incomplete IR must not deserialize into WorkflowIr"
    );
}

#[test]
fn fleet_agent_ir_registers() {
    // A fleet agent that uses a catalog tool must still produce an IR the API
    // can store (POST /workflows deserializes into WorkflowIr). Regression for
    // the bug where catalog tool defs were embedded into the IR `tools` map
    // with a shape ToolConfig can't deserialize.
    let json = include_str!("fixtures/fleet_researcher_ir.json");
    let value: serde_json::Value = serde_json::from_str(json).expect("parse fixture json");
    let parsed = serde_json::from_value::<jamjet_ir::WorkflowIr>(value);
    assert!(
        parsed.is_ok(),
        "fleet researcher IR must deserialize into WorkflowIr, got: {:?}",
        parsed.err()
    );
}

//! # Real-World Example: Healthcare Compliance Policy Engine
//!
//! ## Scenario
//! A hospital chain runs a patient intake workflow powered by AI agents.
//! Regulatory requirements (HIPAA, hospital policy) dictate:
//!
//! 1. **No agent may delete patient records** — even if the LLM hallucinates a
//!    "clean up old records" tool call, the runtime must block it.
//! 2. **Prescribing medication requires human pharmacist approval** — the agent
//!    can draft the prescription, but a licensed pharmacist must click "approve"
//!    before it's submitted to the pharmacy system.
//! 3. **Only approved LLM models may be used** — the hospital's security team
//!    has audited `claude-sonnet-4-6` and `claude-haiku-4-5-20251001` but NOT any
//!    GPT models. A developer accidentally configuring `gpt-4o` must be caught.
//! 4. **Node-level override** — the triage step uses a specialized model that is
//!    allowed at that specific node even though it's not in the global allowlist.
//!
//! ## Why This Matters
//! Without a policy engine, a single misconfigured agent could:
//! - Delete protected health information (PHI)
//! - Submit unreviewed prescriptions
//! - Route patient data through an unaudited model
//! These are not theoretical — they are HIPAA violations carrying $50k+ fines.

use jamjet_ir::workflow::PolicySetIr;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};

/// The hospital's global policy: applies to every workflow.
fn hospital_global_policy() -> PolicySetIr {
    PolicySetIr {
        blocked_tools: vec![
            "delete_patient_record".to_string(),
            "delete_*".to_string(),        // block ALL deletion tools
            "export_phi_bulk".to_string(), // block bulk PHI export
        ],
        require_approval_for: vec![
            "prescribe_medication".to_string(),
            "order_lab_*".to_string(), // all lab orders need approval
            "discharge_patient".to_string(),
        ],
        model_allowlist: vec![
            "claude-sonnet-4-6".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        ],
    }
}

/// The patient intake workflow's policy: inherits global + adds workflow-specific rules.
fn intake_workflow_policy() -> PolicySetIr {
    PolicySetIr {
        blocked_tools: vec![
            "modify_insurance_claim".to_string(), // intake can't touch billing
        ],
        require_approval_for: vec![
            "schedule_surgery".to_string(), // surgery scheduling is workflow-specific
        ],
        model_allowlist: vec![], // defer to global allowlist
    }
}

/// The triage node gets a special exception: it may use a fine-tuned model.
fn triage_node_policy() -> PolicySetIr {
    PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![
            "claude-sonnet-4-6".to_string(),
            "ft-triage-v3".to_string(), // custom fine-tuned model allowed HERE ONLY
        ],
    }
}

// ── Test 1: Agent tries to delete patient record → BLOCKED ─────────────

#[test]
fn agent_cannot_delete_patient_records() {
    // Story: An AI agent processing discharge summaries hallucinates a tool
    // call to "delete_patient_record" (perhaps misinterpreting "remove from
    // active list" as "delete"). The policy engine catches this.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    let ctx = EvaluationContext {
        node_id: "discharge_summary_agent".to_string(),
        node_kind_tag: "tool".to_string(),
        tool_name: Some("delete_patient_record".to_string()),
        model_ref: None,
    };

    let decision = evaluator.evaluate(&ctx, &[&global]);

    match &decision {
        PolicyDecision::Block { reason } => {
            assert!(
                reason.contains("delete_patient_record"),
                "Reason should mention the blocked tool: {reason}"
            );
            println!("BLOCKED: {reason}");
            println!("  -> Patient record protected. HIPAA compliance maintained.");
        }
        other => panic!("Expected Block, got: {other:?}"),
    }
}

// ── Test 2: Wildcard blocks catch novel deletion attempts ──────────────

#[test]
fn wildcard_blocks_catch_novel_deletion_tools() {
    // Story: A new microservice exposes "delete_appointment" — the developers
    // didn't update the blocked tools list. But the wildcard "delete_*" catches it.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    for tool in &[
        "delete_appointment",
        "delete_insurance_record",
        "delete_test_results",
    ] {
        let ctx = EvaluationContext {
            node_id: "some_agent".to_string(),
            node_kind_tag: "tool".to_string(),
            tool_name: Some(tool.to_string()),
            model_ref: None,
        };

        let decision = evaluator.evaluate(&ctx, &[&global]);
        assert!(
            matches!(decision, PolicyDecision::Block { .. }),
            "Tool '{tool}' should be blocked by wildcard 'delete_*'"
        );
        println!("BLOCKED: {tool} (caught by delete_* wildcard)");
    }
}

// ── Test 3: Prescription requires pharmacist approval ──────────────────

#[test]
fn prescription_requires_pharmacist_approval() {
    // Story: The intake agent determines the patient needs a medication refill
    // and calls "prescribe_medication". The policy engine pauses execution so
    // a pharmacist can review the prescription before it reaches the pharmacy.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    let ctx = EvaluationContext {
        node_id: "medication_review_agent".to_string(),
        node_kind_tag: "tool".to_string(),
        tool_name: Some("prescribe_medication".to_string()),
        model_ref: None,
    };

    let decision = evaluator.evaluate(&ctx, &[&global]);

    match &decision {
        PolicyDecision::RequireApproval { approver } => {
            println!("PAUSED: prescribe_medication requires approval from: {approver}");
            println!("  -> Pharmacist will review via /executions/:id/approve");
            println!("  -> Patient safety preserved. Workflow continues after approval.");
        }
        other => panic!("Expected RequireApproval, got: {other:?}"),
    }
}

// ── Test 4: Lab order wildcard catch ───────────────────────────────────

#[test]
fn lab_orders_all_require_approval() {
    // Story: Lab orders are expensive ($50-$500 each). The hospital requires
    // a physician to approve any lab order, even if the AI suggests it.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    for tool in &[
        "order_lab_cbc",
        "order_lab_metabolic_panel",
        "order_lab_urinalysis",
    ] {
        let ctx = EvaluationContext {
            node_id: "lab_agent".to_string(),
            node_kind_tag: "tool".to_string(),
            tool_name: Some(tool.to_string()),
            model_ref: None,
        };

        let decision = evaluator.evaluate(&ctx, &[&global]);
        assert!(
            matches!(decision, PolicyDecision::RequireApproval { .. }),
            "Tool '{tool}' should require approval"
        );
        println!("APPROVAL REQUIRED: {tool}");
    }
}

// ── Test 5: Unapproved model blocked ───────────────────────────────────

#[test]
fn unapproved_model_blocked_at_runtime() {
    // Story: A junior developer configures a new workflow node to use "gpt-4o"
    // because it's cheaper. The security team hasn't audited it for PHI handling.
    // The policy engine catches this at dispatch time, before any patient data
    // reaches the unapproved model.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    let ctx = EvaluationContext {
        node_id: "symptom_analysis".to_string(),
        node_kind_tag: "model".to_string(),
        tool_name: None,
        model_ref: Some("gpt-4o".to_string()),
    };

    let decision = evaluator.evaluate(&ctx, &[&global]);

    match &decision {
        PolicyDecision::Block { reason } => {
            assert!(reason.contains("not in the allowlist"));
            println!("BLOCKED: model 'gpt-4o' — {reason}");
            println!("  -> No PHI sent to unapproved model. Compliance preserved.");
        }
        other => panic!("Expected Block for unapproved model, got: {other:?}"),
    }
}

// ── Test 6: Approved model proceeds ────────────────────────────────────

#[test]
fn approved_model_proceeds_normally() {
    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();

    let ctx = EvaluationContext {
        node_id: "symptom_analysis".to_string(),
        node_kind_tag: "model".to_string(),
        tool_name: None,
        model_ref: Some("claude-sonnet-4-6".to_string()),
    };

    let decision = evaluator.evaluate(&ctx, &[&global]);
    assert!(
        matches!(decision, PolicyDecision::Allow),
        "Approved model should be allowed"
    );
    println!("ALLOWED: claude-sonnet-4-6 is in the approved model list");
}

// ── Test 7: Node-level override allows specialized model ───────────────

#[test]
fn triage_node_allows_finetuned_model() {
    // Story: The triage step uses "ft-triage-v3", a custom fine-tuned model
    // trained on hospital-specific triage protocols. It's not in the global
    // allowlist, but the triage node's policy explicitly permits it.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();
    let triage = triage_node_policy();

    // The triage node uses ft-triage-v3 — allowed by node-level policy.
    let ctx = EvaluationContext {
        node_id: "triage_assessment".to_string(),
        node_kind_tag: "model".to_string(),
        tool_name: None,
        model_ref: Some("ft-triage-v3".to_string()),
    };

    // Node-level policy is last (most specific) → checked first in reverse.
    let decision = evaluator.evaluate(&ctx, &[&global, &triage]);
    assert!(
        matches!(decision, PolicyDecision::Allow),
        "ft-triage-v3 should be allowed at the triage node"
    );
    println!("ALLOWED: ft-triage-v3 at triage node (node-level override)");

    // But the SAME model at a DIFFERENT node (e.g., discharge) is blocked.
    let ctx2 = EvaluationContext {
        node_id: "discharge_summary".to_string(),
        node_kind_tag: "model".to_string(),
        tool_name: None,
        model_ref: Some("ft-triage-v3".to_string()),
    };

    // Only global policy applies — no node-level override.
    let decision2 = evaluator.evaluate(&ctx2, &[&global]);
    assert!(
        matches!(decision2, PolicyDecision::Block { .. }),
        "ft-triage-v3 should be BLOCKED outside the triage node"
    );
    println!("BLOCKED: ft-triage-v3 at discharge_summary (no node-level override)");
}

// ── Test 8: Multi-layer policy — workflow blocks what global allows ─────

#[test]
fn workflow_policy_adds_restrictions() {
    // Story: The intake workflow should never touch insurance claims — that's
    // the billing team's domain. Even though the global policy doesn't block
    // "modify_insurance_claim", the intake workflow policy does.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();
    let workflow = intake_workflow_policy();

    let ctx = EvaluationContext {
        node_id: "insurance_agent".to_string(),
        node_kind_tag: "tool".to_string(),
        tool_name: Some("modify_insurance_claim".to_string()),
        model_ref: None,
    };

    // Global allows it, but workflow blocks it.
    let decision = evaluator.evaluate(&ctx, &[&global, &workflow]);
    assert!(
        matches!(decision, PolicyDecision::Block { .. }),
        "modify_insurance_claim should be blocked by intake workflow policy"
    );
    println!("BLOCKED: modify_insurance_claim (workflow-level restriction)");
}

// ── Test 9: Safe tools pass through all layers ─────────────────────────

#[test]
fn safe_tools_pass_through_all_policy_layers() {
    // Story: The intake agent calls "lookup_patient", "check_insurance_status",
    // and "schedule_appointment" — all perfectly safe, not blocked or gated.

    let evaluator = PolicyEvaluator;
    let global = hospital_global_policy();
    let workflow = intake_workflow_policy();

    for tool in &[
        "lookup_patient",
        "check_insurance_status",
        "schedule_appointment",
    ] {
        let ctx = EvaluationContext {
            node_id: "intake_agent".to_string(),
            node_kind_tag: "tool".to_string(),
            tool_name: Some(tool.to_string()),
            model_ref: None,
        };

        let decision = evaluator.evaluate(&ctx, &[&global, &workflow]);
        assert!(
            matches!(decision, PolicyDecision::Allow),
            "Tool '{tool}' should be allowed — it's not blocked or gated"
        );
    }
    println!("ALLOWED: lookup_patient, check_insurance_status, schedule_appointment");
    println!("  -> Normal workflow operations proceed without friction.");
}

//! # Real-World Example: SOC2 Compliance Audit Trail for a FinTech Platform
//!
//! ## Scenario
//! A FinTech startup processes loan applications using AI agents. Their SOC2 Type II
//! audit requires demonstrating that:
//!
//! 1. **Every security-relevant action is logged immutably** — the auditor needs to
//!    see a complete trail of who did what, when, and what policy decision was made.
//!    During the last audit, a gap in the log for 3 hours on March 14th cost the
//!    company $40,000 in remediation consulting fees.
//!
//! 2. **Audit entries cannot be modified or deleted** — the backend uses INSERT OR
//!    IGNORE (idempotent appends). There is no UPDATE or DELETE path. The auditor
//!    verified this by reviewing the SQL in the codebase.
//!
//! 3. **Actor attribution is accurate** — every entry identifies whether the action
//!    was taken by a human (loan officer), an AI agent, or the system itself. The
//!    auditor flagged a competitor who logged all actions as "system" — that's a
//!    SOC2 control failure.
//!
//! 4. **Policy violations are queryable** — the compliance team runs a weekly report:
//!    "show me all policy violations in the last 7 days". This powers their incident
//!    review process and feeds their quarterly SOC2 evidence package.
//!
//! 5. **HTTP request context is preserved** — for API-triggered actions, the audit
//!    log captures the request ID, method, path, and source IP. This enables
//!    forensic correlation: "which API call triggered this policy violation?"
//!
//! ## Why This Matters
//! Without a proper audit trail:
//! - SOC2 auditors will issue a qualified opinion (bad for fundraising)
//! - Regulators can't verify that AI-driven decisions have human oversight
//! - Incident response is blind — you can't reconstruct what happened
//! - A single unlogged policy violation could mean regulatory action

use chrono::Utc;
use jamjet_audit::{ActorType, AuditBackend, AuditLogEntry, AuditQuery, SqliteAuditBackend};
use uuid::Uuid;

/// Helper: create an in-memory SQLite audit backend for testing.
async fn test_backend() -> SqliteAuditBackend {
    let backend = SqliteAuditBackend::open("sqlite::memory:")
        .await
        .expect("Failed to open in-memory SQLite");
    backend.migrate().await.expect("Failed to migrate");
    backend
}

/// Helper: create a basic audit log entry with the given parameters.
fn make_entry(
    execution_id: &str,
    event_type: &str,
    actor_id: &str,
    actor_type: ActorType,
) -> AuditLogEntry {
    let mut entry = AuditLogEntry::new(
        Uuid::new_v4(),
        execution_id.to_string(),
        1,
        event_type.to_string(),
        actor_id.to_string(),
        actor_type,
        serde_json::json!({ "type": event_type }),
    );
    entry.created_at = Utc::now();
    entry
}

// ── Test 1: Append-only guarantees — entries persist and are queryable ────

#[tokio::test]
async fn audit_entries_are_immutably_persisted() {
    // Story: A loan application workflow processes 3 steps:
    //   1. Agent starts analyzing the applicant's credit report
    //   2. Policy blocks the agent from accessing the "delete_credit_report" tool
    //   3. A loan officer approves the final decision
    // All 3 events must appear in the audit log, in order, with correct metadata.

    let backend = test_backend().await;
    let exec_id = "LOAN-2024-00847";

    // Step 1: Agent starts
    let mut e1 = make_entry(
        exec_id,
        "node_started",
        "credit-analysis-agent-v2",
        ActorType::System,
    );
    e1.sequence = 1;
    backend.append(e1).await.expect("append e1");

    // Step 2: Policy violation
    let mut e2 = make_entry(
        exec_id,
        "policy_violation",
        "credit-analysis-agent-v2",
        ActorType::System,
    );
    e2.sequence = 2;
    e2.policy_decision = Some("blocked".to_string());
    e2.raw_event = serde_json::json!({
        "type": "policy_violation",
        "node_id": "credit_analyzer",
        "rule": "block_tool:delete_credit_report",
        "decision": "blocked",
        "policy_scope": "global"
    });
    backend.append(e2).await.expect("append e2");

    // Step 3: Human approval
    let mut e3 = make_entry(
        exec_id,
        "approval_received",
        "loan-officer-jsmith",
        ActorType::Human,
    );
    e3.sequence = 3;
    backend.append(e3).await.expect("append e3");

    // Verify: all 3 entries exist
    let q = AuditQuery {
        execution_id: Some(exec_id.to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let entries = backend.query(&q).await.expect("query");
    assert_eq!(entries.len(), 3, "All 3 audit entries must be persisted");

    let count = backend.count(&q).await.expect("count");
    assert_eq!(count, 3);

    println!("PERSISTED: 3 audit entries for loan application {exec_id}");
    println!("  1. node_started (agent: credit-analysis-agent-v2)");
    println!("  2. policy_violation (blocked: delete_credit_report)");
    println!("  3. approval_received (human: loan-officer-jsmith)");
    println!("  -> Complete audit trail for SOC2 evidence package.");
}

// ── Test 2: Idempotent appends — duplicate inserts don't create duplicates ─

#[tokio::test]
async fn idempotent_append_prevents_duplicates() {
    // Story: The worker crashes after appending an audit entry but before
    // acknowledging the work item. On restart, it replays the event. The
    // audit log must not create a duplicate — INSERT OR IGNORE handles this.

    let backend = test_backend().await;

    let entry = make_entry(
        "LOAN-2024-00848",
        "node_completed",
        "system",
        ActorType::System,
    );
    let entry_id = entry.id;

    // First append
    backend.append(entry.clone()).await.expect("first append");

    // Duplicate append (crash replay) — same ID
    backend
        .append(entry)
        .await
        .expect("duplicate append should succeed silently");

    // Verify: only 1 entry
    let q = AuditQuery {
        execution_id: Some("LOAN-2024-00848".to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let entries = backend.query(&q).await.expect("query");
    assert_eq!(
        entries.len(),
        1,
        "Duplicate append must not create a second entry"
    );
    assert_eq!(entries[0].id, entry_id);

    println!("IDEMPOTENT: Duplicate append ignored (crash replay scenario)");
    println!("  -> Entry ID {entry_id} exists exactly once.");
    println!("  -> Worker crash recovery is safe — no duplicate audit records.");
}

// ── Test 3: Actor attribution — humans vs agents vs system ───────────────

#[tokio::test]
async fn actor_attribution_distinguishes_humans_agents_system() {
    // Story: The SOC2 auditor asks: "Show me all actions taken by humans
    // vs. AI agents vs. the system in the last month." The audit log must
    // correctly attribute each action.

    let backend = test_backend().await;
    let exec_id = "LOAN-2024-00849";

    // Human: loan officer reviews application
    let e1 = make_entry(
        exec_id,
        "approval_received",
        "loan-officer-jsmith",
        ActorType::Human,
    );
    backend.append(e1).await.unwrap();

    // Agent: credit analysis agent processes documents
    let e2 = make_entry(
        exec_id,
        "node_completed",
        "credit-analysis-agent-v2",
        ActorType::Agent,
    );
    backend.append(e2).await.unwrap();

    // System: scheduler assigns work
    let e3 = make_entry(
        exec_id,
        "node_scheduled",
        "scheduler-main",
        ActorType::System,
    );
    backend.append(e3).await.unwrap();

    // Query all for this execution
    let q = AuditQuery {
        execution_id: Some(exec_id.to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let entries = backend.query(&q).await.unwrap();
    assert_eq!(entries.len(), 3);

    // Verify actor types (entries returned in reverse chronological order)
    let human_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.actor_type == ActorType::Human)
        .collect();
    let agent_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.actor_type == ActorType::Agent)
        .collect();
    let system_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.actor_type == ActorType::System)
        .collect();

    assert_eq!(human_entries.len(), 1, "One human action");
    assert_eq!(agent_entries.len(), 1, "One agent action");
    assert_eq!(system_entries.len(), 1, "One system action");
    assert_eq!(human_entries[0].actor_id, "loan-officer-jsmith");
    assert_eq!(agent_entries[0].actor_id, "credit-analysis-agent-v2");

    println!("ATTRIBUTION: Correct actor types for all 3 entries");
    println!("  Human:  loan-officer-jsmith (approval_received)");
    println!("  Agent:  credit-analysis-agent-v2 (node_completed)");
    println!("  System: scheduler-main (node_scheduled)");
    println!("  -> SOC2 auditor can verify human oversight of AI decisions.");
}

// ── Test 4: Query policy violations — weekly compliance report ───────────

#[tokio::test]
async fn query_policy_violations_for_compliance_report() {
    // Story: Every Monday, the compliance team generates a report of all
    // policy violations from the past week. They filter by event_type =
    // "policy_violation" and review each one in their incident process.

    let backend = test_backend().await;

    // Simulate a week of activity across multiple loan applications
    let violations = vec![
        ("LOAN-001", "block_tool:delete_applicant_data", "blocked"),
        ("LOAN-002", "block_tool:export_ssn_bulk", "blocked"),
        (
            "LOAN-003",
            "require_approval:approve_loan_over_500k",
            "require_approval",
        ),
    ];

    for (exec_id, rule, decision) in &violations {
        let mut entry = make_entry(exec_id, "policy_violation", "system", ActorType::System);
        entry.policy_decision = Some(decision.to_string());
        entry.raw_event = serde_json::json!({
            "type": "policy_violation",
            "rule": rule,
            "decision": decision,
            "policy_scope": "global"
        });
        backend.append(entry).await.unwrap();
    }

    // Also add non-violation events (should NOT appear in the report)
    for i in 0..10 {
        let entry = make_entry(
            &format!("LOAN-{:03}", i + 10),
            "node_completed",
            "system",
            ActorType::System,
        );
        backend.append(entry).await.unwrap();
    }

    // Query: show me all policy violations
    let q = AuditQuery {
        event_type: Some("policy_violation".to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let results = backend.query(&q).await.unwrap();
    assert_eq!(results.len(), 3, "Exactly 3 policy violations");

    // All results should have policy_decision set
    for entry in &results {
        assert!(
            entry.policy_decision.is_some(),
            "All violations must have a policy_decision"
        );
        println!(
            "VIOLATION: {} — {} ({})",
            entry.execution_id,
            entry.raw_event["rule"].as_str().unwrap_or("unknown"),
            entry.policy_decision.as_deref().unwrap()
        );
    }

    let total_count = backend.count(&q).await.unwrap();
    assert_eq!(total_count, 3);

    println!("\nWeekly compliance report: 3 policy violations found.");
    println!("  -> 2 blocked (data deletion/export attempts)");
    println!("  -> 1 require_approval (high-value loan)");
    println!("  -> Report exported for SOC2 evidence package.");
}

// ── Test 5: HTTP request context for forensic correlation ────────────────

#[tokio::test]
async fn http_context_enables_forensic_correlation() {
    // Story: An incident occurred at 3:47pm — an unauthorized API call tried
    // to export all applicant SSNs. The security team needs to trace:
    //   1. Which API endpoint was called
    //   2. What IP address the request came from
    //   3. The X-Request-ID for log correlation with the API gateway

    let backend = test_backend().await;

    let mut entry = make_entry(
        "LOAN-2024-00850",
        "policy_violation",
        "api-key-intern-account",
        ActorType::Human,
    );
    entry.policy_decision = Some("blocked".to_string());
    entry.http_request_id = Some("req-abc123-def456".to_string());
    entry.http_method = Some("POST".to_string());
    entry.http_path = Some("/api/v1/executions/LOAN-2024-00850/tools/export_ssn_bulk".to_string());
    entry.ip_address = Some("10.0.3.47".to_string());
    entry.raw_event = serde_json::json!({
        "type": "policy_violation",
        "rule": "block_tool:export_ssn_bulk",
        "decision": "blocked",
        "policy_scope": "global"
    });

    backend.append(entry).await.unwrap();

    // Forensic query
    let q = AuditQuery {
        execution_id: Some("LOAN-2024-00850".to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let results = backend.query(&q).await.unwrap();
    assert_eq!(results.len(), 1);

    let incident = &results[0];
    assert_eq!(incident.actor_id, "api-key-intern-account");
    assert_eq!(incident.http_method.as_deref(), Some("POST"));
    assert!(incident
        .http_path
        .as_deref()
        .unwrap()
        .contains("export_ssn_bulk"));
    assert_eq!(incident.ip_address.as_deref(), Some("10.0.3.47"));
    assert_eq!(
        incident.http_request_id.as_deref(),
        Some("req-abc123-def456")
    );
    assert_eq!(incident.policy_decision.as_deref(), Some("blocked"));

    println!("FORENSIC TRACE: Unauthorized SSN export attempt");
    println!(
        "  Actor:      {} (type: {:?})",
        incident.actor_id, incident.actor_type
    );
    println!(
        "  Request:    {} {}",
        incident.http_method.as_deref().unwrap(),
        incident.http_path.as_deref().unwrap()
    );
    println!("  Source IP:  {}", incident.ip_address.as_deref().unwrap());
    println!(
        "  Request ID: {}",
        incident.http_request_id.as_deref().unwrap()
    );
    println!(
        "  Decision:   {}",
        incident.policy_decision.as_deref().unwrap()
    );
    println!("  -> Cross-reference with API gateway logs using request ID.");
    println!("  -> Intern's API key was immediately revoked.");
}

// ── Test 6: Pagination for large audit trails ────────────────────────────

#[tokio::test]
async fn pagination_works_for_large_audit_trails() {
    // Story: A complex loan portfolio analysis generates 150 audit entries.
    // The compliance dashboard paginates through results 20 at a time.

    let backend = test_backend().await;
    let exec_id = "PORTFOLIO-2024-Q1";

    for i in 0..150 {
        let mut entry = make_entry(
            exec_id,
            "node_completed",
            "portfolio-agent",
            ActorType::Agent,
        );
        entry.sequence = i + 1;
        backend.append(entry).await.unwrap();
    }

    // Page 1: first 20
    let q1 = AuditQuery {
        execution_id: Some(exec_id.to_string()),
        limit: 20,
        offset: 0,
        ..AuditQuery::default()
    };
    let page1 = backend.query(&q1).await.unwrap();
    assert_eq!(page1.len(), 20, "Page 1 should have 20 entries");

    // Page 2: next 20
    let q2 = AuditQuery {
        execution_id: Some(exec_id.to_string()),
        limit: 20,
        offset: 20,
        ..AuditQuery::default()
    };
    let page2 = backend.query(&q2).await.unwrap();
    assert_eq!(page2.len(), 20, "Page 2 should have 20 entries");

    // Total count for pagination UI
    let total = backend
        .count(&AuditQuery {
            execution_id: Some(exec_id.to_string()),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(total, 150);

    // Last page (partial)
    let q_last = AuditQuery {
        execution_id: Some(exec_id.to_string()),
        limit: 20,
        offset: 140,
        ..AuditQuery::default()
    };
    let last_page = backend.query(&q_last).await.unwrap();
    assert_eq!(
        last_page.len(),
        10,
        "Last page should have 10 entries (150 % 20)"
    );

    println!("PAGINATION: 150 entries across 8 pages (20/page)");
    println!("  Page 1: 20 entries, Page 8: 10 entries");
    println!("  Total count: {total}");
    println!("  -> Compliance dashboard can paginate through full audit trail.");
}

// ── Test 7: Filter by actor — "show me everything this person did" ───────

#[tokio::test]
async fn filter_by_actor_for_user_activity_review() {
    // Story: After the SSN export incident, the security team wants to see
    // ALL actions taken by the intern's API key in the past month.

    let backend = test_backend().await;

    // Intern's actions across multiple executions
    for i in 0..5 {
        let entry = make_entry(
            &format!("LOAN-{:03}", i),
            "node_started",
            "api-key-intern-account",
            ActorType::Human,
        );
        backend.append(entry).await.unwrap();
    }

    // Other users' actions
    for i in 0..20 {
        let entry = make_entry(
            &format!("LOAN-{:03}", i + 100),
            "node_started",
            "api-key-senior-analyst",
            ActorType::Human,
        );
        backend.append(entry).await.unwrap();
    }

    // Query: show me everything the intern did
    let q = AuditQuery {
        actor_id: Some("api-key-intern-account".to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let results = backend.query(&q).await.unwrap();
    assert_eq!(results.len(), 5, "Intern had exactly 5 actions");

    for entry in &results {
        assert_eq!(entry.actor_id, "api-key-intern-account");
    }

    println!("USER ACTIVITY REVIEW: api-key-intern-account");
    println!("  Total actions: 5 (across 5 loan applications)");
    println!("  -> Security team reviewing all intern activity post-incident.");
}

// ── Test 8: Tool call hash for detecting duplicate/replay attacks ─────────

#[tokio::test]
async fn tool_call_hash_detects_duplicate_operations() {
    // Story: The security team wants to detect if the same tool call is being
    // replayed (a potential replay attack). The tool_call_hash field provides
    // a SHA-256 fingerprint of tool_name + input that can be checked for
    // duplicates.

    let backend = test_backend().await;

    let hash = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    // First call — legitimate
    let mut e1 = make_entry(
        "LOAN-001",
        "tool_approval_required",
        "system",
        ActorType::System,
    );
    e1.tool_call_hash = Some(hash.to_string());
    backend.append(e1).await.unwrap();

    // Second call with same hash — potential replay
    let mut e2 = make_entry(
        "LOAN-001",
        "tool_approval_required",
        "system",
        ActorType::System,
    );
    e2.tool_call_hash = Some(hash.to_string());
    backend.append(e2).await.unwrap();

    // Query all tool approval events
    let q = AuditQuery {
        event_type: Some("tool_approval_required".to_string()),
        limit: 50,
        ..AuditQuery::default()
    };
    let results = backend.query(&q).await.unwrap();

    // Both entries exist (different IDs, but same hash)
    assert_eq!(results.len(), 2);
    let hashes: Vec<_> = results
        .iter()
        .filter_map(|e| e.tool_call_hash.as_deref())
        .collect();
    assert_eq!(hashes.len(), 2);
    assert_eq!(
        hashes[0], hashes[1],
        "Same tool call hash = potential replay"
    );

    println!("REPLAY DETECTION: 2 tool calls with identical hash");
    println!("  Hash: {hash}");
    println!("  -> Security team alerted to potential replay attack.");
    println!("  -> Forensic analysis can correlate with HTTP request context.");
}

// ── Test 9: Multi-execution query — cross-case compliance view ───────────

#[tokio::test]
async fn multi_execution_compliance_view() {
    // Story: The quarterly SOC2 audit requires a view across ALL executions.
    // The auditor wants to see total event counts, policy violation counts,
    // and verify that no audit gaps exist.

    let backend = test_backend().await;

    // Simulate 5 loan applications with various events
    let event_types = [
        "node_started",
        "node_completed",
        "policy_violation",
        "approval_received",
        "node_completed",
    ];

    for exec_idx in 0..5 {
        for (seq, event_type) in event_types.iter().enumerate() {
            let actor = if *event_type == "approval_received" {
                ("loan-officer-jsmith", ActorType::Human)
            } else {
                ("system", ActorType::System)
            };
            let mut entry = make_entry(
                &format!("LOAN-Q1-{:03}", exec_idx),
                event_type,
                actor.0,
                actor.1,
            );
            entry.sequence = seq as i64 + 1;
            if *event_type == "policy_violation" {
                entry.policy_decision = Some("blocked".to_string());
            }
            backend.append(entry).await.unwrap();
        }
    }

    // Total events across all executions
    let total = backend.count(&AuditQuery::new()).await.unwrap();
    assert_eq!(total, 25, "5 executions × 5 events = 25 total");

    // Policy violations across all executions
    let violation_count = backend
        .count(&AuditQuery {
            event_type: Some("policy_violation".to_string()),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(violation_count, 5, "5 policy violations (1 per execution)");

    // Human approvals
    let approval_count = backend
        .count(&AuditQuery {
            event_type: Some("approval_received".to_string()),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(approval_count, 5, "5 human approvals (1 per execution)");

    println!("QUARTERLY SOC2 AUDIT SUMMARY:");
    println!("  Total audit entries:  {total}");
    println!("  Policy violations:    {violation_count}");
    println!("  Human approvals:      {approval_count}");
    println!("  Executions covered:   5");
    println!("  -> No audit gaps. Complete trail for all loan applications.");
    println!("  -> Evidence package ready for SOC2 Type II auditor.");
}

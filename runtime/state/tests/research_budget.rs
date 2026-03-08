//! # Real-World Example: AI Research Assistant — Token & Cost Budget Enforcement
//!
//! ## Scenario
//! A law firm uses an AI research assistant to analyze legal precedents for cases.
//! Each research task is a workflow execution. The firm's managing partner mandates:
//!
//! 1. **Per-case token budget: 500k total tokens** — paralegal time is expensive,
//!    but so are tokens. A single complex patent case consumed 4.7M tokens ($120)
//!    when the agent kept "finding one more relevant precedent" in an infinite loop.
//!
//! 2. **Per-case cost budget: $15 USD** — hard cap. The firm bills clients $800/hr
//!    for attorney time, but the AI costs are overhead. $15 per research task is
//!    the approved budget. Beyond that, a human paralegal takes over.
//!
//! 3. **Budget state persists across node restarts** — if the worker crashes mid-
//!    analysis (OOM on a 200-page contract), the accumulated budget must not reset.
//!    The BudgetState is embedded in the snapshot, so crash recovery picks up where
//!    it left off.
//!
//! ## Why This Matters
//! Without budget enforcement:
//! - One "just one more search" loop cost the firm $120 on a $2,000 case
//! - Monthly AI spend spiked 8× before anyone noticed
//! - The managing partner now requires per-execution cost caps
//! - Budget state in snapshots ensures crash resilience — no silent budget resets

use jamjet_state::budget::BudgetState;
use serde_json::json;

// ── Test 1: Accumulate usage across multiple model calls ───────────────

#[test]
fn budget_accumulates_across_multiple_model_calls() {
    // Story: The research agent makes 5 sequential calls:
    //   1. "Summarize the complaint" — 2k in, 1k out
    //   2. "Find relevant precedents for patent infringement" — 5k in, 3k out
    //   3. "Analyze Smith v. Jones (2019)" — 8k in, 4k out
    //   4. "Analyze Oracle v. Google (2021)" — 12k in, 6k out
    //   5. "Draft memo comparing precedents" — 15k in, 8k out

    let mut budget = BudgetState::default();

    let calls = vec![
        (2_000u64, 1_000u64, 0.003),    // summarize
        (5_000, 3_000, 0.008),           // find precedents
        (8_000, 4_000, 0.012),           // analyze case 1
        (12_000, 6_000, 0.018),          // analyze case 2
        (15_000, 8_000, 0.023),          // draft memo
    ];

    for (i, (input, output, cost)) in calls.iter().enumerate() {
        budget.accumulate(Some(*input), Some(*output), Some(*cost));
        budget.record_success();
        println!(
            "Call {}: +{}in/{}out/${:.3} → Total: {}in/{}out/${:.3} ({}tok total)",
            i + 1,
            input,
            output,
            cost,
            budget.total_input_tokens,
            budget.total_output_tokens,
            budget.total_cost_usd,
            budget.total_tokens()
        );
    }

    assert_eq!(budget.total_input_tokens, 42_000);
    assert_eq!(budget.total_output_tokens, 22_000);
    assert_eq!(budget.total_tokens(), 64_000);
    assert!((budget.total_cost_usd - 0.064).abs() < 1e-9);
    assert_eq!(budget.consecutive_error_count, 0);

    println!("\nFinal: 64k tokens, $0.064 — well within the 500k / $15 budget.");
}

// ── Test 2: Detect when token budget is exceeded ───────────────────────

#[test]
fn detect_token_budget_exceeded() {
    // Story: The agent is analyzing a massive patent portfolio (50 patents).
    // After processing 30 patents, token usage has crept to 480k. The next
    // patent analysis pushes it over 500k.

    let mut budget = BudgetState::default();

    // Simulate 30 patent analyses, each ~16k tokens
    for _ in 0..30 {
        budget.accumulate(Some(10_000), Some(6_000), Some(0.016));
    }
    println!("After 30 patents: {} tokens, ${:.2}", budget.total_tokens(), budget.total_cost_usd);
    assert_eq!(budget.total_tokens(), 480_000);

    // The 31st patent pushes over the 500k limit
    budget.accumulate(Some(15_000), Some(10_000), Some(0.025));
    println!("After 31 patents: {} tokens, ${:.2}", budget.total_tokens(), budget.total_cost_usd);

    let token_limit = 500_000u64;
    let exceeded = budget.total_tokens() > token_limit;
    assert!(exceeded, "Should detect token budget exceeded");
    println!(
        "\nBUDGET EXCEEDED: {} tokens > {} limit (+{} over)",
        budget.total_tokens(),
        token_limit,
        budget.total_tokens() - token_limit
    );
    println!("  -> Execution pauses. Paralegal takes over remaining 20 patents.");
}

// ── Test 3: Detect when cost budget is exceeded ────────────────────────

#[test]
fn detect_cost_budget_exceeded() {
    // Story: The agent uses claude-opus (expensive) for deep analysis of a
    // complex antitrust case. Each call costs ~$0.50. After 30 calls,
    // cost is $15.00 — right at the limit. The 31st call pushes over.

    let mut budget = BudgetState::default();

    for i in 0..30 {
        budget.accumulate(Some(50_000), Some(20_000), Some(0.50));
        if i % 10 == 9 {
            println!(
                "After {} calls: {} tokens, ${:.2}",
                i + 1,
                budget.total_tokens(),
                budget.total_cost_usd
            );
        }
    }

    assert!((budget.total_cost_usd - 15.0).abs() < 1e-9);
    println!("At $15.00 exactly — right at the limit.");

    // One more call
    budget.accumulate(Some(50_000), Some(20_000), Some(0.50));

    let cost_limit = 15.0f64;
    let exceeded = budget.total_cost_usd > cost_limit;
    assert!(exceeded);
    println!(
        "COST EXCEEDED: ${:.2} > ${:.2} limit",
        budget.total_cost_usd, cost_limit
    );
    println!("  -> Managing partner's $15/case cap enforced.");
    println!("  -> Junior associate reviews partial findings and decides next steps.");
}

// ── Test 4: Budget survives crash via snapshot persistence ─────────────

#[test]
fn budget_survives_crash_via_snapshot() {
    // Story: The research agent is processing a 200-page contract. At call #15,
    // the worker process crashes (OOM). When it restarts, it loads the latest
    // snapshot which contains the BudgetState. The accumulated usage is preserved.

    let mut budget = BudgetState::default();

    // Simulate 15 calls before crash
    for _ in 0..15 {
        budget.accumulate(Some(8_000), Some(4_000), Some(0.012));
    }
    budget.iteration_count = 15;

    // Save to "snapshot" (simulated as JSON)
    let mut snapshot_state = json!({
        "case_id": "PATENT-2024-0847",
        "precedents_found": 12,
        "memo_draft": "Analysis in progress..."
    });
    budget.patch_into_snapshot_state(&mut snapshot_state);

    println!("=== CRASH ===");
    println!("Worker process killed (OOM on 200-page contract)");
    println!("Snapshot saved to SQLite with BudgetState embedded.\n");

    // === RECOVERY ===
    // The worker restarts and loads the snapshot.
    let recovered_budget = BudgetState::from_snapshot_state(&snapshot_state);

    assert_eq!(recovered_budget.total_input_tokens, 120_000);
    assert_eq!(recovered_budget.total_output_tokens, 60_000);
    assert_eq!(recovered_budget.total_tokens(), 180_000);
    assert!((recovered_budget.total_cost_usd - 0.18).abs() < 1e-9);
    assert_eq!(recovered_budget.iteration_count, 15);

    println!("=== RECOVERY ===");
    println!("Worker restarted. BudgetState loaded from snapshot:");
    println!(
        "  Tokens: {} (in: {}, out: {})",
        recovered_budget.total_tokens(),
        recovered_budget.total_input_tokens,
        recovered_budget.total_output_tokens
    );
    println!("  Cost: ${:.3}", recovered_budget.total_cost_usd);
    println!("  Iterations: {}", recovered_budget.iteration_count);
    println!("\nBudget NOT reset — crash resilience confirmed.");
    println!("Agent resumes from iteration 16 with full budget awareness.");

    // Original business data is also preserved
    assert_eq!(snapshot_state["case_id"], "PATENT-2024-0847");
    assert_eq!(snapshot_state["precedents_found"], 12);
}

// ── Test 5: Error tracking for circuit breaker integration ─────────────

#[test]
fn error_tracking_integrates_with_circuit_breaker() {
    // Story: The legal research API (Westlaw) goes down. The agent fails
    // 3 times trying to search for precedents. BudgetState tracks the
    // consecutive errors — the autonomy enforcer reads this to trip the
    // circuit breaker.

    let mut budget = BudgetState::default();

    // Successful calls
    budget.accumulate(Some(5_000), Some(2_000), Some(0.007));
    budget.record_success();
    assert_eq!(budget.consecutive_error_count, 0);

    // Westlaw goes down — 3 consecutive errors
    budget.record_error();
    println!("Error 1: consecutive_errors = {}", budget.consecutive_error_count);
    budget.record_error();
    println!("Error 2: consecutive_errors = {}", budget.consecutive_error_count);
    budget.record_error();
    println!("Error 3: consecutive_errors = {}", budget.consecutive_error_count);

    assert_eq!(budget.consecutive_error_count, 3);
    println!("\nCircuit breaker threshold reached.");
    println!("Autonomy enforcer will read this from BudgetState and trip the breaker.");

    // After the API comes back up and a call succeeds:
    budget.record_success();
    assert_eq!(budget.consecutive_error_count, 0);
    println!("\nAPI recovered. Success resets consecutive_error_count to 0.");
}

// ── Test 6: Multiple budget dimensions checked simultaneously ──────────

#[test]
fn multiple_budget_dimensions_checked_simultaneously() {
    // Story: A complex class-action case is analyzed. We need to check ALL
    // budget dimensions: input tokens, output tokens, total tokens, AND cost.
    // The first limit hit determines the action.

    let mut budget = BudgetState::default();

    // High output (drafting a long memo) but moderate input
    for _ in 0..50 {
        budget.accumulate(Some(3_000), Some(8_000), Some(0.011));
    }

    println!("Budget state after 50 memo-drafting calls:");
    println!("  Input tokens:  {}", budget.total_input_tokens);
    println!("  Output tokens: {}", budget.total_output_tokens);
    println!("  Total tokens:  {}", budget.total_tokens());
    println!("  Total cost:    ${:.2}", budget.total_cost_usd);

    // Check each dimension independently
    let input_limit = 500_000u64;
    let output_limit = 300_000u64;
    let total_limit = 500_000u64;
    let cost_limit = 15.0f64;

    let input_ok = budget.total_input_tokens <= input_limit;
    let output_ok = budget.total_output_tokens <= output_limit;
    let total_ok = budget.total_tokens() <= total_limit;
    let cost_ok = budget.total_cost_usd <= cost_limit;

    println!("\nBudget checks:");
    println!("  Input  ({} / {}): {}", budget.total_input_tokens, input_limit, if input_ok { "OK" } else { "EXCEEDED" });
    println!("  Output ({} / {}): {}", budget.total_output_tokens, output_limit, if output_ok { "OK" } else { "EXCEEDED" });
    println!("  Total  ({} / {}): {}", budget.total_tokens(), total_limit, if total_ok { "OK" } else { "EXCEEDED" });
    println!("  Cost   (${:.2} / ${:.2}): {}", budget.total_cost_usd, cost_limit, if cost_ok { "OK" } else { "EXCEEDED" });

    // Output tokens exceed first (400k > 300k limit)
    assert!(!output_ok, "Output tokens should exceed the 300k limit");
    // Total tokens also exceed (550k > 500k)
    assert!(!total_ok, "Total tokens should exceed the 500k limit");
    // But cost is still under ($0.55 < $15)
    assert!(cost_ok, "Cost should still be under budget");
    // Input is fine (150k < 500k)
    assert!(input_ok, "Input tokens should be under budget");

    println!("\n-> Output token limit hit FIRST. Execution would stop for that dimension.");
}

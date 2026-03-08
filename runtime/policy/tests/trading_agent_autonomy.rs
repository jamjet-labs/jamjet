//! # Real-World Example: Algorithmic Trading Agent — Autonomy Enforcement
//!
//! ## Scenario
//! A quantitative hedge fund deploys AI agents for pre-trade research:
//! - **Research Agent**: scans filings, news, social sentiment
//! - **Risk Agent**: validates position sizes against risk limits
//! - **Execution Agent**: submits orders to the broker API
//!
//! The fund's CRO (Chief Risk Officer) mandates:
//!
//! 1. **Research agent runs in `bounded_autonomous` mode** — it may iterate up
//!    to 10 times (tool calls: search APIs, parse filings) but must escalate
//!    if it exceeds $2 in LLM costs per analysis. The CRO saw a runaway agent
//!    spend $47 in one night iterating over SEC filings.
//!
//! 2. **Execution agent runs in `guided` mode** — every single trade order
//!    requires a human trader's explicit approval. No exceptions. A rogue agent
//!    placing live orders could cost millions before anyone notices.
//!
//! 3. **Circuit breaker** — if any agent hits 3 consecutive errors (e.g., the
//!    broker API is down), stop immediately and alert the operations team.
//!    In March 2025, a competitor lost $3M when their agent retried failed
//!    orders 200+ times, each time with slightly different parameters that
//!    the broker interpreted as new orders.
//!
//! ## Why This Matters
//! Without autonomy enforcement:
//! - A research agent could consume $1,000+/day in LLM costs chasing rabbit holes
//! - An execution agent could place unauthorized trades
//! - Error loops could generate thousands of duplicate orders

use jamjet_agents::card::{AutonomyConstraints, AutonomyLevel};
use jamjet_policy::autonomy::{AutonomyContext, AutonomyDecision, AutonomyEnforcer};

fn research_agent_context(
    iterations: u32,
    cost: f64,
    tokens: u64,
    consecutive_errors: u32,
) -> AutonomyContext {
    AutonomyContext {
        agent_ref: "quant-research-v2".to_string(),
        autonomy_level: AutonomyLevel::BoundedAutonomous,
        constraints: Some(AutonomyConstraints {
            max_iterations: Some(10),
            max_tool_calls: Some(50),
            token_budget: Some(100_000), // 100k tokens per analysis
            cost_budget_usd: Some(2.0),  // $2 max per analysis run
            allowed_tools: vec![],
            blocked_tools: vec![],
            allowed_delegations: vec![],
            require_approval_for: vec![],
            time_budget_secs: Some(300), // 5 minute timeout
        }),
        current_iterations: iterations,
        current_tool_calls: 0,
        current_cost_usd: cost,
        current_tokens: tokens,
        consecutive_errors,
        circuit_breaker_threshold: 3,
    }
}

fn execution_agent_context(consecutive_errors: u32) -> AutonomyContext {
    AutonomyContext {
        agent_ref: "trade-executor-v1".to_string(),
        autonomy_level: AutonomyLevel::Guided,
        constraints: None, // Guided mode doesn't need constraints — everything is gated
        current_iterations: 0,
        current_tool_calls: 0,
        current_cost_usd: 0.0,
        current_tokens: 0,
        consecutive_errors,
        circuit_breaker_threshold: 3,
    }
}

// ── Test 1: Research agent within limits proceeds normally ──────────────

#[test]
fn research_agent_within_limits_proceeds() {
    // Story: It's 8am. The research agent starts analyzing NVDA's latest
    // 10-K filing. It has used 3 iterations, $0.80, and 40k tokens.
    // All within limits — agent should proceed.

    let enforcer = AutonomyEnforcer;
    let ctx = research_agent_context(3, 0.80, 40_000, 0);

    let decision = enforcer.check(&ctx, None);
    assert!(matches!(decision, AutonomyDecision::Proceed));
    println!("PROCEED: Research agent at iteration 3, $0.80, 40k tokens — within all limits");
}

// ── Test 2: Research agent hits iteration limit → escalate ─────────────

#[test]
fn research_agent_iteration_limit_escalates() {
    // Story: The research agent has been analyzing a complex restructuring.
    // It's now on iteration 10 — the maximum. The LLM keeps wanting to "dig
    // deeper into subsidiary filings" but the CRO says: no. Escalate.

    let enforcer = AutonomyEnforcer;
    let ctx = research_agent_context(10, 1.50, 80_000, 0);

    let decision = enforcer.check(&ctx, None);

    match &decision {
        AutonomyDecision::EscalateLimit {
            limit_type,
            limit_value,
            actual_value,
            ..
        } => {
            assert_eq!(limit_type, "max_iterations");
            println!("ESCALATED: Research agent hit iteration limit");
            println!("  Limit: {limit_value}, Actual: {actual_value}");
            println!("  -> Analyst notified. They can review partial findings and decide");
            println!("     whether to increase the limit or accept current analysis.");
        }
        other => panic!("Expected EscalateLimit for iterations, got: {other:?}"),
    }
}

// ── Test 3: Research agent hits cost budget → escalate ──────────────────

#[test]
fn research_agent_cost_budget_escalates() {
    // Story: The agent is only on iteration 5, but it's been using long-context
    // prompts (full 10-K text = 150k tokens each). Cost has hit $2.10 — over
    // the $2.00 budget. Without this check, last week's runaway spent $47.

    let enforcer = AutonomyEnforcer;
    let ctx = research_agent_context(5, 2.10, 95_000, 0);

    let decision = enforcer.check(&ctx, None);

    match &decision {
        AutonomyDecision::EscalateLimit {
            limit_type,
            limit_value,
            actual_value,
            ..
        } => {
            assert_eq!(limit_type, "cost_budget");
            println!("ESCALATED: Research agent exceeded cost budget");
            println!("  Budget: ${limit_value}, Spent: ${actual_value}");
            println!("  -> Operations team notified before costs spiral further.");
        }
        other => panic!("Expected EscalateLimit for cost, got: {other:?}"),
    }
}

// ── Test 4: Research agent hits token budget → escalate ─────────────────

#[test]
fn research_agent_token_budget_escalates() {
    // Story: The agent has consumed 100k+ tokens processing multiple filings.
    // Token budget enforcement catches this even though cost might still be
    // under limit (e.g., using a cheap model).

    let enforcer = AutonomyEnforcer;
    let ctx = research_agent_context(4, 0.50, 100_000, 0);

    let decision = enforcer.check(&ctx, None);

    match &decision {
        AutonomyDecision::EscalateLimit { limit_type, .. } => {
            assert_eq!(limit_type, "token_budget");
            println!("ESCALATED: Research agent exceeded token budget (100k tokens)");
        }
        other => panic!("Expected EscalateLimit for tokens, got: {other:?}"),
    }
}

// ── Test 5: Execution agent — EVERY trade requires approval ────────────

#[test]
fn execution_agent_guided_mode_requires_approval() {
    // Story: The execution agent wants to submit a market order:
    //   buy 1000 shares of NVDA @ market
    // In guided mode, this call is PAUSED until a human trader approves.

    let enforcer = AutonomyEnforcer;
    let ctx = execution_agent_context(0);

    let decision = enforcer.check(&ctx, Some("submit_market_order"));

    match &decision {
        AutonomyDecision::RequireToolApproval { tool_name } => {
            assert_eq!(tool_name, "submit_market_order");
            println!("APPROVAL REQUIRED: {tool_name}");
            println!("  -> Trade desk notified. Trader reviews order parameters.");
            println!("  -> No order submitted until explicit approval via /approve.");
        }
        other => panic!("Expected RequireToolApproval, got: {other:?}"),
    }
}

// ── Test 6: Execution agent — even cancel orders need approval ─────────

#[test]
fn execution_agent_cancel_also_requires_approval() {
    // Story: The agent decides to cancel a pending limit order. Even cancellations
    // require approval — the CRO saw an agent cancel a hedging position that was
    // protecting against downside risk.

    let enforcer = AutonomyEnforcer;
    let ctx = execution_agent_context(0);

    let decision = enforcer.check(&ctx, Some("cancel_order"));
    assert!(
        matches!(decision, AutonomyDecision::RequireToolApproval { .. }),
        "Cancel orders must also require approval in guided mode"
    );
    println!("APPROVAL REQUIRED: cancel_order (even cancellations are gated)");
}

// ── Test 7: Circuit breaker — broker API goes down ─────────────────────

#[test]
fn circuit_breaker_trips_after_consecutive_errors() {
    // Story: At 2:47pm, the broker's API starts returning 503 errors.
    // The execution agent fails 3 times in a row trying to check positions.
    // The circuit breaker trips — agent STOPS and alerts operations.
    //
    // Without this: the agent at a competitor kept retrying for 2 hours,
    // creating 200+ duplicate position queries that the broker rate-limited,
    // which cascaded into a 24-hour trading suspension.

    let enforcer = AutonomyEnforcer;

    // After 1 error: proceed
    let ctx1 = execution_agent_context(1);
    assert!(matches!(enforcer.check(&ctx1, None), AutonomyDecision::Proceed));
    println!("Error 1/3: Proceed (might be transient)");

    // After 2 errors: proceed
    let ctx2 = execution_agent_context(2);
    assert!(matches!(enforcer.check(&ctx2, None), AutonomyDecision::Proceed));
    println!("Error 2/3: Proceed (still within threshold)");

    // After 3 errors: TRIP
    let ctx3 = execution_agent_context(3);
    let decision = enforcer.check(&ctx3, None);

    match &decision {
        AutonomyDecision::TripCircuitBreaker {
            consecutive_errors,
            threshold,
            ..
        } => {
            assert_eq!(*consecutive_errors, 3);
            assert_eq!(*threshold, 3);
            println!("CIRCUIT BREAKER TRIPPED: {consecutive_errors} consecutive errors (threshold: {threshold})");
            println!("  -> Agent STOPPED. Operations team alerted.");
            println!("  -> No further API calls until human resets via /approve.");
            println!("  -> Potential duplicate order storm PREVENTED.");
        }
        other => panic!("Expected TripCircuitBreaker, got: {other:?}"),
    }
}

// ── Test 8: Circuit breaker overrides even bounded_autonomous ──────────

#[test]
fn circuit_breaker_overrides_bounded_autonomous_limits() {
    // Story: The research agent is within all its budgets (iteration 2, $0.50),
    // but the SEC EDGAR API is down and it's hit 3 consecutive errors.
    // Circuit breaker fires BEFORE checking budget limits — safety first.

    let enforcer = AutonomyEnforcer;
    let ctx = research_agent_context(2, 0.50, 20_000, 3); // 3 errors but within budgets

    let decision = enforcer.check(&ctx, None);
    assert!(
        matches!(decision, AutonomyDecision::TripCircuitBreaker { .. }),
        "Circuit breaker should fire regardless of budget limits"
    );
    println!("CIRCUIT BREAKER: fires even though agent is within budget limits");
    println!("  -> Error recovery takes priority over continued analysis.");
}

// ── Test 9: Fully autonomous agent — only circuit breaker applies ──────

#[test]
fn fully_autonomous_agent_only_has_circuit_breaker() {
    // Story: The market data ingestion agent runs in fully_autonomous mode —
    // it streams price data 24/7 with no human interaction needed. The ONLY
    // safety net is the circuit breaker.

    let enforcer = AutonomyEnforcer;

    let ctx = AutonomyContext {
        agent_ref: "market-data-ingest-v1".to_string(),
        autonomy_level: AutonomyLevel::FullyAutonomous,
        constraints: Some(AutonomyConstraints {
            max_iterations: Some(1000), // high limit
            max_tool_calls: Some(10000),
            token_budget: Some(1_000_000),
            cost_budget_usd: Some(100.0),
            allowed_tools: vec![],
            blocked_tools: vec![],
            allowed_delegations: vec![],
            require_approval_for: vec![],
            time_budget_secs: None,
        }),
        current_iterations: 500,  // Way past what bounded would allow
        current_tool_calls: 5000,
        current_cost_usd: 50.0,   // Way past typical budgets
        current_tokens: 500_000,
        consecutive_errors: 0,
        circuit_breaker_threshold: 3,
    };

    // Should proceed — fully autonomous ignores iteration/cost/token limits
    let decision = enforcer.check(&ctx, None);
    assert!(matches!(decision, AutonomyDecision::Proceed));
    println!("PROCEED: Fully autonomous agent at 500 iterations, $50 — no limits enforced");
    println!("  -> Only the circuit breaker (3 consecutive errors) would stop this agent.");
}

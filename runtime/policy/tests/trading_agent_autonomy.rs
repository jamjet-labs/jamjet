//! # Example: an algorithmic trading research agent's autonomy limits
//!
//! A fund runs a research agent that may take 10 steps, spend 100k tokens and
//! $2 per analysis, and must stop after 3 consecutive errors (a broker or
//! filings API that is down should not be hammered). These are the card's
//! `max_iterations`, `token_budget`, `cost_budget_usd` and
//! `max_consecutive_errors`; the runtime folds what the agent used from the
//! run's event log and asks [`check`] before every step.
//!
//! The autonomy level does not switch any of this off: a
//! `fully_autonomous` card's declared limits apply too. Guided mode does not
//! gate every call; use `require_approval_for` for that.

use jamjet_agents::card::AutonomyConstraints;
use jamjet_policy::autonomy::{check, AgentUsage, AutonomyDecision, Step};

fn research_card() -> AutonomyConstraints {
    serde_json::from_value(serde_json::json!({
        "max_iterations": 10,
        "max_tool_calls": 50,
        "token_budget": 100_000,
        "cost_budget_usd": 2.0,
        "time_budget_secs": 300,
        "max_consecutive_errors": 3
    }))
    .unwrap()
}

fn usage(iterations: u64, cost_usd: f64, tokens: u64, consecutive_errors: u32) -> AgentUsage {
    AgentUsage {
        iterations,
        tool_calls: iterations,
        tokens,
        cost_usd,
        consecutive_errors,
        elapsed_ms: Some(60_000),
    }
}

const SEARCH: Step = Step { tool_calls: 1 };

fn limit(d: &AutonomyDecision) -> &'static str {
    match d {
        AutonomyDecision::LimitReached { limit_type, .. } => limit_type,
        other => panic!("expected a limit, got {other:?}"),
    }
}

#[test]
fn within_limits_proceeds() {
    let d = check(Some(&research_card()), &usage(3, 0.80, 40_000, 0), SEARCH);
    assert_eq!(d, AutonomyDecision::Proceed);
}

#[test]
fn the_eleventh_step_is_refused() {
    let card = research_card();
    assert_eq!(
        check(Some(&card), &usage(9, 1.50, 80_000, 0), SEARCH),
        AutonomyDecision::Proceed
    );
    assert_eq!(
        limit(&check(Some(&card), &usage(10, 1.50, 80_000, 0), SEARCH)),
        "max_iterations"
    );
}

#[test]
fn a_spent_cost_budget_is_refused() {
    let d = check(Some(&research_card()), &usage(5, 2.10, 95_000, 0), SEARCH);
    assert_eq!(limit(&d), "cost_budget");
}

#[test]
fn a_spent_token_budget_is_refused_even_when_cost_is_low() {
    let d = check(Some(&research_card()), &usage(4, 0.50, 100_000, 0), SEARCH);
    assert_eq!(limit(&d), "token_budget");
}

#[test]
fn the_breaker_trips_on_the_third_consecutive_error() {
    let card = research_card();
    for errors in [1, 2] {
        assert_eq!(
            check(Some(&card), &usage(2, 0.5, 20_000, errors), SEARCH),
            AutonomyDecision::Proceed,
            "{errors} errors"
        );
    }
    assert_eq!(
        check(Some(&card), &usage(2, 0.5, 20_000, 3), SEARCH),
        AutonomyDecision::CircuitBreakerTripped {
            consecutive_errors: 3,
            threshold: 3
        }
    );
}

#[test]
fn the_breaker_is_checked_before_the_budgets() {
    // Within every budget, but the filings API has failed three times.
    let d = check(Some(&research_card()), &usage(2, 0.50, 20_000, 3), SEARCH);
    assert!(
        matches!(d, AutonomyDecision::CircuitBreakerTripped { .. }),
        "{d:?}"
    );
}

#[test]
fn high_limits_still_bind() {
    // A market data agent with high limits: still refused once past them.
    let card: AutonomyConstraints =
        serde_json::from_value(serde_json::json!({ "max_iterations": 1000 })).unwrap();
    assert_eq!(
        check(Some(&card), &usage(999, 50.0, 500_000, 0), SEARCH),
        AutonomyDecision::Proceed
    );
    assert_eq!(
        limit(&check(Some(&card), &usage(1000, 50.0, 500_000, 0), SEARCH)),
        "max_iterations"
    );
}

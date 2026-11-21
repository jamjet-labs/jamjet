//! A run's caps, and the arithmetic that holds the run to them.
//!
//! Two questions, asked of a run's [`RunUsage`]:
//!
//! - [`may_fire`]: may another spending step start? Refused AT the cap, so a
//!   run that lands exactly on its cap goes on, and only its next spending
//!   step is refused.
//! - [`past_cap`]: is the run past a cap? Then it fails.
//!
//! Each rule is written as the condition that lets the run through, so an
//! unknown count, a NaN or a broken cap refuses:
//!
//! | Cap | `may_fire` is `Ok` only if | `past_cap` trips unless |
//! |---|---|---|
//! | token cap L (total, input or output) | no step left a count the cap reads unknown, and the count is `< L` | no step left a count the cap reads unknown, and the count is `<= L` |
//! | `cost_budget_usd` C | no step left its cost unknown, C is a cost a step may report, and the nano-dollar sum is `< C` | the same with `<= C` |
//!
//! An input cap reads the input tokens, an output cap the output tokens, and
//! a total cap both. A token count already past its cap trips as that cap
//! even when a count it reads is unknown: what a step left out can only add
//! to it. Only otherwise does the unknown count trip `TokensNotReported`.
//!
//! A cap that is not a cost a step may report (NaN, negative, above
//! `MAX_REPORTED_COST_USD`) refuses every spending step and fails the run.

use jamjet_ir::workflow::TokenBudgetIr;

use crate::event::EventKind;
use crate::usage::{cost_to_nanos, RunUsage, MAX_REPORTED_COST_USD};

/// The caps a run is held to: its workflow's `token_budget` and
/// `cost_budget_usd`.
#[derive(Debug, Clone, Default)]
pub struct RunCaps {
    pub token_budget: Option<TokenBudgetIr>,
    pub cost_budget_usd: Option<f64>,
}

/// Which cap a [`CapTrip`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapKind {
    TotalTokens,
    InputTokens,
    OutputTokens,
    /// A token cap is set and a step did not report a token count it reads.
    TokensNotReported,
    CostUsd,
    /// A dollar cap is set and a step reported no usable cost.
    CostNotReported,
}

/// A cap the run has reached or passed.
#[derive(Debug, Clone, PartialEq)]
pub struct CapTrip {
    pub kind: CapKind,
    /// The cap, in tokens or USD. A token count above 2^53 is rounded
    /// here; the trip event and [`CapTrip::describe`] carry it exactly.
    pub limit: f64,
    /// What the run has counted against it, in tokens or USD.
    pub current: f64,
    /// For a token trip, the cap and the count as compared: `(limit, current)`.
    tokens: Option<(u64, u64)>,
    /// The cap's name in a trip event: `total_tokens`, `input_tokens`,
    /// `output_tokens` or `cost_usd`. For `TokensNotReported` it names the
    /// token cap that could not be checked.
    cap: &'static str,
}

/// May another spending step fire? Refused AT the cap.
pub fn may_fire(caps: &RunCaps, u: &RunUsage) -> Result<(), CapTrip> {
    match first_trip(caps, u, Bound::Under) {
        None => Ok(()),
        Some(trip) => Err(trip),
    }
}

/// Is the run past a cap? Then it fails.
pub fn past_cap(caps: &RunCaps, u: &RunUsage) -> Option<CapTrip> {
    first_trip(caps, u, Bound::AtMost)
}

/// How a count must stand against its cap for the run to go on.
#[derive(Clone, Copy)]
enum Bound {
    /// Below it: another spending step may fire.
    Under,
    /// At most at it: the run is not past its cap.
    AtMost,
}

impl Bound {
    fn holds(self, count: u64, cap: u64) -> bool {
        match self {
            Bound::Under => count < cap,
            Bound::AtMost => count <= cap,
        }
    }
}

/// The first cap that does not let the run through, token caps first.
fn first_trip(caps: &RunCaps, u: &RunUsage, bound: Bound) -> Option<CapTrip> {
    if let Some(tb) = &caps.token_budget {
        let input_known = u.input_tokens_unknown_steps == 0;
        let output_known = u.output_tokens_unknown_steps == 0;
        // (the cap's name, its kind, its limit, the count, whether every
        // count the cap reads is known)
        let token_caps = [
            (
                "total_tokens",
                CapKind::TotalTokens,
                tb.total_tokens,
                u.total_tokens(),
                input_known && output_known,
            ),
            (
                "input_tokens",
                CapKind::InputTokens,
                tb.input_tokens,
                u.input_tokens,
                input_known,
            ),
            (
                "output_tokens",
                CapKind::OutputTokens,
                tb.output_tokens,
                u.output_tokens,
                output_known,
            ),
        ];
        for (cap, kind, limit, count, known) in token_caps {
            let Some(limit) = limit.map(u64::from) else {
                continue;
            };
            let kind = if !bound.holds(count, limit) {
                kind
            } else if !known {
                CapKind::TokensNotReported
            } else {
                continue;
            };
            return Some(CapTrip {
                kind,
                limit: limit as f64,
                current: count as f64,
                tokens: Some((limit, count)),
                cap,
            });
        }
    }
    if let Some(limit) = caps.cost_budget_usd {
        let cap_nanos = cost_to_nanos(limit);
        let known = u.cost_unknown_steps == 0;
        let through = match cap_nanos {
            Some(c) => known && bound.holds(u.cost_nanos, c),
            None => false,
        };
        if !through {
            return Some(CapTrip {
                // A broken cap is named as such even when a cost is also
                // unknown: no step could pass it.
                kind: if cap_nanos.is_some() && !known {
                    CapKind::CostNotReported
                } else {
                    CapKind::CostUsd
                },
                limit,
                current: u.cost_usd(),
                tokens: None,
                cap: "cost_usd",
            });
        }
    }
    None
}

impl CapTrip {
    /// A token trip's cap and count, exact.
    fn token_counts(&self) -> (u64, u64) {
        self.tokens
            .unwrap_or((self.limit as u64, self.current as u64))
    }

    /// A dollar cap that no step could pass: not a cost a step may report.
    fn cap_is_invalid(&self) -> bool {
        self.kind == CapKind::CostUsd && cost_to_nanos(self.limit).is_none()
    }

    /// The trip event to record against `node_id`: `TokenBudgetExceeded` or
    /// `CostBudgetExceeded`, with a `reason` when the count alone does not
    /// show why. A NaN or infinite cap is written as `limit_usd: 0.0` with
    /// `reason: "cost_cap_invalid"`: JSON has no such number, and
    /// serde_json would write `null`, which no build reads back.
    pub fn event(&self, node_id: &str) -> EventKind {
        match self.kind {
            CapKind::TotalTokens
            | CapKind::InputTokens
            | CapKind::OutputTokens
            | CapKind::TokensNotReported => EventKind::TokenBudgetExceeded {
                node_id: node_id.to_string(),
                kind: self.cap.to_string(),
                limit: self.token_counts().0,
                current: self.token_counts().1,
                reason: (self.kind == CapKind::TokensNotReported)
                    .then(|| "tokens_not_reported".to_string()),
            },
            CapKind::CostUsd | CapKind::CostNotReported => EventKind::CostBudgetExceeded {
                node_id: node_id.to_string(),
                limit_usd: if self.limit.is_finite() {
                    self.limit
                } else {
                    0.0
                },
                current_usd: self.current,
                reason: if self.kind == CapKind::CostNotReported {
                    Some("cost_not_reported".to_string())
                } else if self.cap_is_invalid() {
                    Some("cost_cap_invalid".to_string())
                } else {
                    None
                },
            },
        }
    }

    /// One line for a refusal or a failed run.
    pub fn describe(&self) -> String {
        match self.kind {
            CapKind::TotalTokens | CapKind::InputTokens | CapKind::OutputTokens => format!(
                "token budget reached: {} {} of {}",
                self.cap,
                self.token_counts().1,
                self.token_counts().0
            ),
            CapKind::TokensNotReported => format!(
                "token budget cannot be checked: a step did not report the tokens this cap counts ({} cap {})",
                self.cap,
                self.token_counts().0
            ),
            CapKind::CostUsd if self.cap_is_invalid() => format!(
                "cost budget ${} is not a cap a run can be held to: it must be between $0 and ${}",
                self.limit, MAX_REPORTED_COST_USD
            ),
            CapKind::CostUsd => format!(
                "cost budget reached: ${} of ${}",
                self.current, self.limit
            ),
            CapKind::CostNotReported => format!(
                "cost budget cannot be checked: a step reported no usable cost (${} counted, cap ${})",
                self.current, self.limit
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::tests::{completion, fold};
    use serde_json::{json, Value};

    fn tokens(total: Option<u32>, input: Option<u32>, output: Option<u32>) -> RunCaps {
        RunCaps {
            token_budget: Some(TokenBudgetIr {
                total_tokens: total,
                input_tokens: input,
                output_tokens: output,
            }),
            cost_budget_usd: None,
        }
    }

    fn dollars(c: f64) -> RunCaps {
        RunCaps {
            token_budget: None,
            cost_budget_usd: Some(c),
        }
    }

    fn json_of(trip: &CapTrip, node: &str) -> Value {
        serde_json::to_value(trip.event(node)).unwrap()
    }

    fn kind_limit_current(trip: &CapTrip) -> (CapKind, f64, f64) {
        (trip.kind, trip.limit, trip.current)
    }

    #[test]
    fn a_cap_reached_exactly_refuses_the_next_step_and_does_not_fail_the_run() {
        let u = fold(&[completion("m", Some(1500), Some(500), Some(0.75), None)]);
        let cases = [
            (tokens(Some(2000), None, None), CapKind::TotalTokens, 2000.0),
            (tokens(None, Some(1500), None), CapKind::InputTokens, 1500.0),
            (tokens(None, None, Some(500)), CapKind::OutputTokens, 500.0),
            (dollars(0.75), CapKind::CostUsd, 0.75),
        ];
        for (caps, kind, at) in cases {
            let trip = may_fire(&caps, &u).expect_err("the next step is refused at the cap");
            assert_eq!(kind_limit_current(&trip), (kind, at, at));
            assert_eq!(
                past_cap(&caps, &u),
                None,
                "{kind:?}: at the cap is not past it"
            );
        }
        assert_eq!(
            json_of(
                &may_fire(&tokens(Some(2000), None, None), &u).unwrap_err(),
                "m"
            ),
            json!({"type":"token_budget_exceeded","node_id":"m","kind":"total_tokens","limit":2000,"current":2000})
        );

        // One under every cap: the step fires.
        let under = RunCaps {
            token_budget: Some(TokenBudgetIr {
                total_tokens: Some(2001),
                input_tokens: Some(1501),
                output_tokens: Some(501),
            }),
            cost_budget_usd: Some(0.750_000_001),
        };
        assert_eq!(may_fire(&under, &u), Ok(()));
        assert_eq!(past_cap(&under, &u), None);
    }

    #[test]
    fn a_cap_passed_by_one_token_fails_the_run() {
        let u = fold(&[completion("__model_2__", Some(1600), Some(401), None, None)]);
        let caps = tokens(Some(2000), None, None);
        let trip = past_cap(&caps, &u).expect("one token past the cap fails the run");
        assert_eq!(
            kind_limit_current(&trip),
            (CapKind::TotalTokens, 2000.0, 2001.0)
        );
        assert_eq!(
            json_of(&trip, "__model_2__"),
            json!({"type":"token_budget_exceeded","node_id":"__model_2__","kind":"total_tokens","limit":2000,"current":2001})
        );
        assert!(may_fire(&caps, &u).is_err());

        let input = past_cap(&tokens(None, Some(1599), None), &u).expect("input past");
        assert_eq!(
            json_of(&input, "__model_2__"),
            json!({"type":"token_budget_exceeded","node_id":"__model_2__","kind":"input_tokens","limit":1599,"current":1600})
        );
        let output = past_cap(&tokens(None, None, Some(400)), &u).expect("output past");
        assert_eq!(
            json_of(&output, "__model_2__"),
            json!({"type":"token_budget_exceeded","node_id":"__model_2__","kind":"output_tokens","limit":400,"current":401})
        );

        // One under: nothing trips.
        let under = fold(&[completion("m", Some(1600), Some(399), None, None)]);
        assert_eq!(may_fire(&caps, &under), Ok(()));
        assert_eq!(past_cap(&caps, &under), None);
    }

    #[test]
    fn point_one_plus_point_two_reaches_a_point_three_cap_exactly() {
        let u = fold(&[
            completion("a", Some(1), Some(1), Some(0.1), None),
            completion("b", Some(1), Some(1), Some(0.2), None),
        ]);
        assert_eq!(u.cost_nanos, 300_000_000);
        // Why the sum is in nano-dollars: in f64 it is past the cap.
        let f64_sum: f64 = [0.1, 0.2].iter().sum();
        assert_eq!(f64_sum, 0.300_000_000_000_000_04);

        let caps = dollars(0.3);
        assert_eq!(
            past_cap(&caps, &u),
            None,
            "exactly at the cap is not past it"
        );
        let trip = may_fire(&caps, &u).expect_err("at the cap the next step is refused");
        assert_eq!(kind_limit_current(&trip), (CapKind::CostUsd, 0.3, 0.3));
        assert_eq!(
            json_of(&trip, "b"),
            json!({"type":"cost_budget_exceeded","node_id":"b","limit_usd":0.3,"current_usd":0.3})
        );
    }

    #[test]
    fn cost_unknown_under_a_dollar_cap_refuses_and_fails() {
        let u = fold(&[
            completion("a", Some(10), Some(5), Some(0.41), None),
            completion("__model_2__", Some(10), Some(5), None, None),
        ]);
        let caps = dollars(1.0);
        let trip = may_fire(&caps, &u).expect_err("an unknown cost refuses");
        assert_eq!(
            kind_limit_current(&trip),
            (CapKind::CostNotReported, 1.0, 0.41)
        );
        assert_eq!(
            json_of(&trip, "__model_2__"),
            json!({"type":"cost_budget_exceeded","node_id":"__model_2__","limit_usd":1.0,"current_usd":0.41,"reason":"cost_not_reported"})
        );
        let failed = past_cap(&caps, &u).expect("an unknown cost fails the run");
        assert_eq!(failed.kind, CapKind::CostNotReported);
    }

    #[test]
    fn cost_unknown_without_a_dollar_cap_is_fine() {
        let u = fold(&[
            completion("a", Some(10), Some(5), Some(0.41), None),
            completion("__model_2__", Some(10), Some(5), None, None),
        ]);
        assert_eq!(u.cost_unknown_steps, 1);
        for caps in [
            RunCaps::default(),
            tokens(Some(1000), Some(1000), Some(1000)),
        ] {
            assert_eq!(may_fire(&caps, &u), Ok(()));
            assert_eq!(past_cap(&caps, &u), None);
        }
    }

    #[test]
    fn tokens_unknown_under_a_token_cap_refuses_and_fails() {
        let u = fold(&[completion("step", None, None, Some(0.2), None)]);
        assert_eq!(u.tokens_unknown_steps, 1);
        let caps = tokens(Some(2000), None, None);
        let trip = may_fire(&caps, &u).expect_err("unknown tokens refuse");
        assert_eq!(
            kind_limit_current(&trip),
            (CapKind::TokensNotReported, 2000.0, 0.0)
        );
        assert_eq!(
            json_of(&trip, "step"),
            json!({"type":"token_budget_exceeded","node_id":"step","kind":"total_tokens","limit":2000,"current":0,"reason":"tokens_not_reported"})
        );
        let failed = past_cap(&caps, &u).expect("unknown tokens fail the run");
        assert_eq!(failed.kind, CapKind::TokensNotReported);

        // The event names the token cap that is set.
        let output = may_fire(&tokens(None, None, Some(50)), &u).unwrap_err();
        assert_eq!(
            json_of(&output, "step"),
            json!({"type":"token_budget_exceeded","node_id":"step","kind":"output_tokens","limit":50,"current":0,"reason":"tokens_not_reported"})
        );

        // Without a token cap, unknown tokens are fine.
        assert_eq!(may_fire(&dollars(1.0), &u), Ok(()));
        assert_eq!(past_cap(&dollars(1.0), &u), None);
    }

    #[test]
    fn a_missing_count_is_unknown_for_every_cap_that_reads_it() {
        let input_only = fold(&[completion("step", Some(500), None, Some(0.2), None)]);
        let output_only = fold(&[completion("step", None, Some(40), Some(0.2), None)]);
        let missing = |u: &RunUsage| {
            (
                u.input_tokens_unknown_steps,
                u.output_tokens_unknown_steps,
                u.tokens_unknown_steps,
            )
        };
        assert_eq!(missing(&input_only), (0, 1, 0));
        assert_eq!(missing(&output_only), (1, 0, 0));
        assert_eq!(
            serde_json::to_value(&input_only).unwrap(),
            json!({"input_tokens":500,"output_tokens":0,"billable_steps":1,"cost_unknown_steps":0,"tokens_unknown_steps":0,"input_tokens_unknown_steps":0,"output_tokens_unknown_steps":1})
        );

        // (usage, caps, the event's `kind` and `current` when the cap reads
        // the missing count; None when it does not)
        let cases = [
            (&input_only, tokens(None, Some(1000), None), None),
            (
                &input_only,
                tokens(None, None, Some(1000)),
                Some(("output_tokens", 0)),
            ),
            (
                &input_only,
                tokens(Some(1000), None, None),
                Some(("total_tokens", 500)),
            ),
            (&output_only, tokens(None, None, Some(1000)), None),
            (
                &output_only,
                tokens(None, Some(1000), None),
                Some(("input_tokens", 0)),
            ),
            (
                &output_only,
                tokens(Some(1000), None, None),
                Some(("total_tokens", 40)),
            ),
        ];
        for (u, caps, unknown) in cases {
            let Some((cap, current)) = unknown else {
                assert_eq!(may_fire(&caps, u), Ok(()), "{caps:?}");
                assert_eq!(past_cap(&caps, u), None, "{caps:?}");
                continue;
            };
            let expected = json!({"type":"token_budget_exceeded","node_id":"step","kind":cap,"limit":1000,"current":current,"reason":"tokens_not_reported"});
            let refused = may_fire(&caps, u).expect_err("a missing count refuses");
            assert_eq!(
                kind_limit_current(&refused),
                (CapKind::TokensNotReported, 1000.0, current as f64),
                "{cap}"
            );
            assert_eq!(json_of(&refused, "step"), expected);
            let failed = past_cap(&caps, u).expect("a missing count fails the run");
            assert_eq!(json_of(&failed, "step"), expected);
        }

        // A count already past its cap trips as that cap, with no reason:
        // what it left out can only add to it.
        let past = fold(&[completion("a", Some(10), None, None, Some("k"))]);
        let caps = tokens(Some(5), None, None);
        let trip = past_cap(&caps, &past).expect("10 tokens are past 5");
        assert_eq!(
            json_of(&trip, "a"),
            json!({"type":"token_budget_exceeded","node_id":"a","kind":"total_tokens","limit":5,"current":10})
        );
        assert_eq!(
            may_fire(&caps, &past).unwrap_err().kind,
            CapKind::TotalTokens
        );
        // Exactly at the cap with a count missing: the next step is refused
        // at the cap, and the run fails because the cap cannot be checked.
        let caps = tokens(Some(500), None, None);
        assert_eq!(
            may_fire(&caps, &input_only).unwrap_err().kind,
            CapKind::TotalTokens
        );
        assert_eq!(
            past_cap(&caps, &input_only).map(|t| t.kind),
            Some(CapKind::TokensNotReported)
        );
    }

    #[test]
    fn a_nan_or_negative_stored_ceiling_refuses_every_step() {
        // Registration refuses these; a ceiling stored before it did, or
        // built in memory, must not read as no limit.
        let fresh = Default::default();
        for c in [f64::NAN, -1.0, -0.000_000_001, f64::INFINITY, 20_000.0] {
            let caps = dollars(c);
            let trip = may_fire(&caps, &fresh).expect_err("a broken cap refuses");
            assert_eq!(trip.kind, CapKind::CostUsd, "{c}");
            assert!(
                past_cap(&caps, &fresh).is_some(),
                "{c}: a broken cap fails the run"
            );
            // The event never carries a non-finite number: serde_json would
            // write it as null, and the log would no longer read.
            let text = serde_json::to_string(&trip.event("n")).unwrap();
            let back: EventKind = serde_json::from_str(&text).expect("the event reads back");
            assert!(
                matches!(&back, EventKind::CostBudgetExceeded { reason: Some(r), .. } if r == "cost_cap_invalid"),
                "{c}: {text}"
            );
        }
        assert_eq!(
            json_of(&may_fire(&dollars(-1.0), &fresh).unwrap_err(), "n"),
            json!({"type":"cost_budget_exceeded","node_id":"n","limit_usd":-1.0,"current_usd":0.0,"reason":"cost_cap_invalid"})
        );
        assert_eq!(
            json_of(&may_fire(&dollars(f64::NAN), &fresh).unwrap_err(), "n"),
            json!({"type":"cost_budget_exceeded","node_id":"n","limit_usd":0.0,"current_usd":0.0,"reason":"cost_cap_invalid"})
        );
    }

    #[test]
    fn a_zero_dollar_cap_refuses_the_first_spending_step() {
        let fresh = Default::default();
        let trip = may_fire(&dollars(0.0), &fresh).expect_err("nothing may spend");
        assert_eq!(kind_limit_current(&trip), (CapKind::CostUsd, 0.0, 0.0));
        assert_eq!(
            past_cap(&dollars(0.0), &fresh),
            None,
            "nothing spent is not past a zero cap"
        );
        // A zero token cap is the same.
        let trip = may_fire(&tokens(Some(0), None, None), &fresh).unwrap_err();
        assert_eq!(kind_limit_current(&trip), (CapKind::TotalTokens, 0.0, 0.0));
        // One nano-dollar is past it.
        let u = fold(&[completion("m", Some(0), Some(0), Some(1e-9), None)]);
        assert_eq!(
            past_cap(&dollars(0.0), &u).map(|t| t.kind),
            Some(CapKind::CostUsd)
        );
    }

    #[test]
    fn trips_describe_themselves() {
        let at = fold(&[completion("a", Some(1500), Some(500), Some(0.3), None)]);
        assert_eq!(
            may_fire(&tokens(Some(2000), None, None), &at)
                .unwrap_err()
                .describe(),
            "token budget reached: total_tokens 2000 of 2000"
        );
        assert_eq!(
            may_fire(&dollars(0.3), &at).unwrap_err().describe(),
            "cost budget reached: $0.3 of $0.3"
        );
        let unknown = fold(&[
            completion("a", Some(10), Some(5), Some(0.41), None),
            completion("b", None, None, None, Some("k")),
        ]);
        // `b` reported nothing and named no model: not billable, so nothing
        // is unknown yet.
        assert_eq!(may_fire(&dollars(1.0), &unknown), Ok(()));
        let unknown = fold(&[
            completion("a", None, None, Some(0.41), None),
            completion("b", Some(10), Some(5), None, None),
        ]);
        assert_eq!(
            may_fire(&dollars(1.0), &unknown).unwrap_err().describe(),
            "cost budget cannot be checked: a step reported no usable cost ($0.41 counted, cap $1)"
        );
        assert_eq!(
            may_fire(&tokens(None, Some(100), None), &unknown)
                .unwrap_err()
                .describe(),
            "token budget cannot be checked: a step did not report the tokens this cap counts (input_tokens cap 100)"
        );
        assert_eq!(
            may_fire(&dollars(-1.0), &Default::default())
                .unwrap_err()
                .describe(),
            "cost budget $-1 is not a cap a run can be held to: it must be between $0 and $10000"
        );
    }

    /// A token count above 2^53 does not fit an f64 exactly. The trip event
    /// and the refusal text carry the count the cap compared, not a rounded
    /// one.
    #[test]
    fn a_trip_keeps_a_large_token_count_exact() {
        let big = 9_007_199_254_740_993_u64;
        let u = fold(&[completion("m", Some(big), Some(0), None, None)]);
        let trip = past_cap(&tokens(Some(1), None, None), &u).expect("past the cap");
        assert_eq!(
            json_of(&trip, "m"),
            json!({"type":"token_budget_exceeded","node_id":"m","kind":"total_tokens","limit":1,"current":big})
        );
        assert_eq!(
            trip.describe(),
            "token budget reached: total_tokens 9007199254740993 of 1"
        );
    }
}

//! Pure, total evaluator for `Condition`-node branch expressions.
//!
//! # Expression grammar
//!
//! Only three forms are authored in practice (grounding §1c):
//!
//! ```text
//! expr    := path                        # truthiness
//!          | path "==" literal           # equality
//!          | path "!=" literal           # inequality
//! path    := "state" ("." ident)+        # always rooted at `state`, dotted segments
//! literal := '"' … '"'                   # double-quoted JSON string
//!          | "true" | "false" | "null"   # JSON keywords (bare, lowercase)
//!          | number                       # JSON number
//! ```
//!
//! # Truthiness rule
//!
//! For the bare-path form a value is **falsy** iff it is:
//! - absent (path does not resolve),
//! - JSON `null`,
//! - `false`,
//! - the number `0` (or `0.0`),
//! - the empty string `""`,
//! - an empty array `[]`, or
//! - an empty object `{}`.
//!
//! Everything else is truthy. This mirrors JS/Python truthiness and matches the
//! budget code that sets `__cost_exceeded__` as a bool.
//!
//! # Total / never-panics contract
//!
//! A malformed or unsupported expression always returns `false` — no panic.
//! Unrecognised forms emit a `tracing::warn!` to aid debugging.
//!
//! # Path resolution
//!
//! The path after `state.` is split on `.` and each segment descends into the
//! JSON object (or an array by zero-based numeric index when the segment is all
//! digits). A missing or non-traversable segment resolves to absent (treated as
//! `null` for comparisons and as falsy for truthiness).

use serde_json::Value;

/// Evaluate a Condition-node branch expression against committed workflow state.
///
/// Pure + total: never panics; a malformed or unsupported expression returns `false`.
///
/// # Truthiness rule (bare-path form)
///
/// Falsy: absent path, `null`, `false`, `0`/`0.0`, `""`, `[]`, `{}`.
/// Everything else is truthy.
///
/// # Literal parsing (comparison form)
///
/// The RHS must be a valid JSON literal: a double-quoted string (`"x"`),
/// `true`, `false`, `null`, or a JSON number. Bare words (e.g. `done`) are NOT
/// valid literals; an unparseable RHS causes the whole comparison to return
/// `false` (fail-closed). Comparison uses typed `serde_json::Value` equality:
/// `"5" != 5`.
pub fn eval_condition(expr: &str, state: &Value) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return false;
    }

    match find_first_op(expr) {
        Some((op_pos, op)) => eval_comparison(expr, state, op_pos, op),
        None => eval_truthiness(expr, state),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Scan left-to-right for the first `!=` or `==` operator.
///
/// Returns `(byte_position_of_operator, operator_str)`.
fn find_first_op(expr: &str) -> Option<(usize, &'static str)> {
    let b = expr.as_bytes();
    let len = b.len();
    if len < 2 {
        return None;
    }
    for i in 0..len - 1 {
        match (b[i], b[i + 1]) {
            (b'!', b'=') => return Some((i, "!=")),
            (b'=', b'=') => return Some((i, "==")),
            _ => {}
        }
    }
    None
}

/// Evaluate a comparison expression: `lhs OP rhs`.
fn eval_comparison(expr: &str, state: &Value, op_pos: usize, op: &str) -> bool {
    let lhs = expr[..op_pos].trim();
    // op_pos + 2 is safe: find_first_op only returns positions where i+1 < len,
    // so i+2 <= len is guaranteed.
    let rhs = expr[op_pos + 2..].trim();

    let path = match lhs.strip_prefix("state.") {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::warn!(
                "eval_condition: lhs `{}` does not start with `state.<path>`; returning false",
                lhs
            );
            return false;
        }
    };

    // Fail closed: an unparseable RHS literal (e.g. bare word `done`) is an
    // invalid expression — return false rather than silently coercing to a string.
    let rhs_val = match parse_literal(rhs) {
        Some(v) => v,
        None => {
            tracing::warn!(
                "eval_condition: RHS `{}` is not a valid literal \
                 (must be a quoted string, true, false, null, or a number); returning false",
                rhs
            );
            return false;
        }
    };

    let resolved = resolve_path(state, path);
    let equal = values_equal(resolved, &rhs_val);

    match op {
        "==" => equal,
        "!=" => !equal,
        _ => false, // unreachable guard; keeps the function total
    }
}

/// Evaluate a truthiness expression: the whole expr is a `state.<path>`.
fn eval_truthiness(expr: &str, state: &Value) -> bool {
    let path = match expr.strip_prefix("state.") {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::warn!(
                "eval_condition: expr `{}` does not start with `state.<path>`; returning false",
                expr
            );
            return false;
        }
    };
    is_truthy(resolve_path(state, path))
}

/// Walk a dotted path (e.g. `"a.b.c"`) through a JSON value.
///
/// Returns `None` for any missing or non-traversable segment.
/// Array indexing by zero-based numeric segment is supported: `"arr.0"` reads index 0.
fn resolve_path<'a>(root: &'a Value, dotted_path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in dotted_path.split('.') {
        if segment.is_empty() {
            return None;
        }
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// JSON truthiness: falsy = absent | null | false | 0 | "" | [] | {}.
/// Everything else is truthy.
fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Parse an RHS literal string into a `serde_json::Value`.
///
/// Valid forms: double-quoted JSON string (`"x"`), `true`, `false`, `null`,
/// or a JSON number. Returns `None` for any other input (e.g. bare words),
/// which the caller must treat as an invalid expression (fail-closed: return
/// `false`).
fn parse_literal(s: &str) -> Option<Value> {
    serde_json::from_str::<Value>(s).ok()
}

/// Compare a resolved path value to an RHS literal.
///
/// A missing path (`None`) is treated as `null`: it equals `Value::Null` and
/// nothing else.
fn values_equal(resolved: Option<&Value>, rhs: &Value) -> bool {
    match resolved {
        None => rhs == &Value::Null,
        Some(v) => v == rhs,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- string equality ----

    #[test]
    fn string_eq_match() {
        assert!(eval_condition(
            r#"state.last_model_finish_reason == "tool_calls""#,
            &json!({"last_model_finish_reason": "tool_calls"}),
        ));
    }

    #[test]
    fn string_eq_mismatch() {
        assert!(!eval_condition(
            r#"state.last_model_finish_reason == "tool_calls""#,
            &json!({"last_model_finish_reason": "stop"}),
        ));
    }

    #[test]
    fn string_eq_missing_key() {
        assert!(!eval_condition(
            r#"state.last_model_finish_reason == "tool_calls""#,
            &json!({}),
        ));
    }

    // ---- string inequality ----

    #[test]
    fn string_neq_true() {
        assert!(eval_condition(r#"state.x != "a""#, &json!({"x": "b"}),));
    }

    #[test]
    fn string_neq_false() {
        assert!(!eval_condition(r#"state.x != "a""#, &json!({"x": "a"}),));
    }

    // ---- truthiness ----

    #[test]
    fn truthiness_bool_true() {
        assert!(eval_condition(
            "state.__cost_exceeded__",
            &json!({"__cost_exceeded__": true}),
        ));
    }

    #[test]
    fn truthiness_bool_false() {
        assert!(!eval_condition(
            "state.__cost_exceeded__",
            &json!({"__cost_exceeded__": false}),
        ));
    }

    #[test]
    fn truthiness_absent_key() {
        assert!(!eval_condition("state.__cost_exceeded__", &json!({})));
    }

    #[test]
    fn truthiness_non_empty_string() {
        assert!(eval_condition(
            "state.__cost_exceeded__",
            &json!({"__cost_exceeded__": "yes"}),
        ));
    }

    #[test]
    fn truthiness_zero_is_falsy() {
        assert!(!eval_condition("state.count", &json!({"count": 0})));
    }

    #[test]
    fn truthiness_empty_string_is_falsy() {
        assert!(!eval_condition("state.s", &json!({"s": ""})));
    }

    #[test]
    fn truthiness_empty_array_is_falsy() {
        assert!(!eval_condition("state.arr", &json!({"arr": []})));
    }

    #[test]
    fn truthiness_empty_object_is_falsy() {
        assert!(!eval_condition("state.obj", &json!({"obj": {}})));
    }

    // ---- nested paths ----

    #[test]
    fn nested_bool_eq_true() {
        assert!(eval_condition(
            "state.__critic_0_verdict__.passed == true",
            &json!({"__critic_0_verdict__": {"passed": true}}),
        ));
    }

    #[test]
    fn nested_bool_eq_false_value() {
        assert!(!eval_condition(
            "state.__critic_0_verdict__.passed == true",
            &json!({"__critic_0_verdict__": {"passed": false}}),
        ));
    }

    #[test]
    fn nested_missing_parent() {
        assert!(!eval_condition(
            "state.__critic_0_verdict__.passed == true",
            &json!({}),
        ));
    }

    // ---- null literal ----

    #[test]
    fn null_eq_missing_path() {
        // A missing path is treated as null.
        assert!(eval_condition("state.missing_key == null", &json!({})));
    }

    #[test]
    fn null_eq_explicit_null() {
        assert!(eval_condition("state.x == null", &json!({"x": null}),));
    }

    #[test]
    fn null_eq_non_null_is_false() {
        assert!(!eval_condition(
            "state.x == null",
            &json!({"x": "something"}),
        ));
    }

    // ---- number literal ----

    #[test]
    fn number_eq_true() {
        assert!(eval_condition("state.count == 5", &json!({"count": 5})));
    }

    #[test]
    fn number_eq_string_is_false() {
        // Typed equality: "5" (string) != 5 (number).
        assert!(!eval_condition("state.count == 5", &json!({"count": "5"}),));
    }

    // ---- bool literal ----

    #[test]
    fn bool_false_literal_match() {
        assert!(eval_condition(
            "state.flag == false",
            &json!({"flag": false}),
        ));
    }

    // ---- malformed inputs — must return false, never panic ----

    #[test]
    fn malformed_garbage() {
        assert!(!eval_condition("garbage", &json!({})));
    }

    #[test]
    fn malformed_lhs_not_state() {
        assert!(!eval_condition(r#"x == "y""#, &json!({"x": "y"})));
    }

    #[test]
    fn malformed_empty_expr() {
        assert!(!eval_condition("", &json!({})));
    }

    #[test]
    fn malformed_just_state_dot() {
        assert!(!eval_condition("state.", &json!({})));
    }

    #[test]
    fn malformed_whitespace_only() {
        assert!(!eval_condition("   ", &json!({})));
    }

    // ---- fail-closed: invalid (unquoted) RHS literal ----

    #[test]
    fn unquoted_rhs_bareword_fail_closed() {
        // `done` is a bare word, not a valid JSON literal.
        // Even though state.status == "done" would be true, the malformed
        // expression must return false (fail-closed contract).
        assert!(!eval_condition(
            "state.status == done",
            &json!({"status": "done"}),
        ));
    }

    #[test]
    fn quoted_rhs_string_still_works() {
        // The quoted form must continue to work correctly.
        assert!(eval_condition(
            r#"state.status == "done""#,
            &json!({"status": "done"}),
        ));
    }
}

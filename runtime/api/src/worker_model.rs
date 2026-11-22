//! A `model` node with `executor: worker`: a model step its run's own worker
//! runs.
//!
//! The claim hands the step to the run's worker with the request the engine
//! built and what the run's caps leave ([`BudgetLeft`]), after the same gate
//! as every item (`routes::gate_claimed_item`). A step whose request holds no
//! prompt to send is failed there instead, as the in-process model executor
//! fails it ([`missing_prompt`]), so the worker never sends a prompt of its
//! own making.

use jamjet_core::node::ModelExecutor;
use jamjet_core::workflow::ExecutionId;
use jamjet_core::NodeKind;
use jamjet_state::backend::StateBackend;
use jamjet_state::usage::cost_to_nanos;
use jamjet_state::{fold_run_usage, RunCaps, RunUsage};
use serde::Serialize;
use serde_json::Value;

/// Whether `kind` is a model step its run's own worker runs.
pub fn is_worker_model(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Model {
            executor: ModelExecutor::Worker,
            ..
        }
    )
}

/// What a run's caps leave, handed to its worker with a model step, beside
/// the payload. Each field is what that cap leaves, or `None` (null) when the
/// run has no such cap.
///
/// A cap the engine cannot check leaves nothing: a step did not report a
/// count the cap reads, or the dollar cap is not one a step could pass. A run
/// past a cap has nothing left of it, never a negative amount.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BudgetLeft {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// USD, from the nano-dollar sums the caps compare.
    pub cost_usd: Option<f64>,
}

/// What `caps` leave after `usage`, read the way
/// `jamjet_state::caps::may_fire` reads them: a total cap reads both token
/// counts, an input or output cap its own, and a dollar cap the nano-dollar
/// sum.
pub fn budget_left(caps: &RunCaps, usage: &RunUsage) -> BudgetLeft {
    let input_known = usage.input_tokens_unknown_steps == 0;
    let output_known = usage.output_tokens_unknown_steps == 0;
    let tokens_left = |cap: Option<u32>, count: u64, known: bool| {
        cap.map(|cap| {
            if known {
                u64::from(cap).saturating_sub(count)
            } else {
                0
            }
        })
    };
    let tokens = caps.token_budget.as_ref();
    BudgetLeft {
        total_tokens: tokens_left(
            tokens.and_then(|t| t.total_tokens),
            usage.total_tokens(),
            input_known && output_known,
        ),
        input_tokens: tokens_left(
            tokens.and_then(|t| t.input_tokens),
            usage.input_tokens,
            input_known,
        ),
        output_tokens: tokens_left(
            tokens.and_then(|t| t.output_tokens),
            usage.output_tokens,
            output_known,
        ),
        cost_usd: caps.cost_budget_usd.map(|cap| match cost_to_nanos(cap) {
            Some(cap) if usage.cost_unknown_steps == 0 => {
                cap.saturating_sub(usage.cost_nanos) as f64 / 1e9
            }
            _ => 0.0,
        }),
    }
}

/// [`budget_left`] for the run of `execution_id`, its usage folded from its
/// log, every segment. A run with no cap reads nothing. `Err` carries why the
/// usage could not be counted.
pub async fn budget_left_of_run(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    caps: &RunCaps,
) -> Result<BudgetLeft, String> {
    if caps.token_budget.is_none() && caps.cost_budget_usd.is_none() {
        return Ok(BudgetLeft::default());
    }
    match fold_run_usage(backend, execution_id).await {
        Ok(Ok(usage)) => Ok(budget_left(caps, &usage)),
        Ok(Err(unreadable)) => Err(format!("the run's usage cannot be counted: {unreadable}")),
        Err(e) => Err(format!("the run's usage cannot be read: {e}")),
    }
}

/// Why the model step of `payload` cannot be sent to a model: the request
/// the engine built holds no prompt. `None` when it holds one.
///
/// The in-process model executor fails the step with these words before it
/// calls the model (`jamjet_worker::ModelNodeExecutor`), not retryable: the
/// scheduler could not render the node's prompt (`prompt_error`), or the node
/// has no prompt and the conversation it would read is empty once a system
/// message repeating the node's system prompt is dropped. The claim fails a
/// worker model step the same way.
pub fn missing_prompt(payload: &Value) -> Option<String> {
    if let Some(reason) = payload.get("prompt_error").and_then(Value::as_str) {
        return Some(format!("ModelNodeExecutor: {reason}"));
    }
    let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
    if !prompt.is_empty() {
        return None;
    }
    let system_prompt = payload.get("system_prompt").and_then(Value::as_str);
    let conversation = payload
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let sent = conversation.iter().any(|message| {
        let role = message.get("role").and_then(Value::as_str);
        let content = message.get("content").and_then(Value::as_str).unwrap_or("");
        !(role == Some("system") && Some(content) == system_prompt)
    });
    (!sent).then(|| "ModelNodeExecutor: no prompt or messages provided".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_ir::workflow::TokenBudgetIr;
    use jamjet_state::Event;
    use jamjet_state::EventKind;
    use serde_json::json;

    fn caps(
        total: Option<u32>,
        input: Option<u32>,
        output: Option<u32>,
        cost: Option<f64>,
    ) -> RunCaps {
        RunCaps {
            token_budget: Some(TokenBudgetIr {
                total_tokens: total,
                input_tokens: input,
                output_tokens: output,
            }),
            cost_budget_usd: cost,
        }
    }

    /// The usage of one billable step per entry: `(input, output, cost)`.
    fn usage(steps: &[(Option<u64>, Option<u64>, Option<f64>)]) -> RunUsage {
        let mut u = RunUsage::default();
        for (i, (input, output, cost)) in steps.iter().enumerate() {
            let e = Event::new(
                ExecutionId::new(),
                i as i64 + 1,
                EventKind::NodeCompleted {
                    node_id: format!("step-{i}"),
                    output: json!({}),
                    state_patch: json!({}),
                    duration_ms: 1,
                    gen_ai_system: None,
                    gen_ai_model: Some("m".into()),
                    input_tokens: *input,
                    output_tokens: *output,
                    finish_reason: None,
                    cost_usd: *cost,
                    provenance: None,
                    idempotency_key: Some(format!("key-{i}")),
                },
            );
            u.apply(&e).unwrap();
        }
        u
    }

    fn left(
        total: Option<u64>,
        input: Option<u64>,
        output: Option<u64>,
        cost: Option<f64>,
    ) -> BudgetLeft {
        BudgetLeft {
            total_tokens: total,
            input_tokens: input,
            output_tokens: output,
            cost_usd: cost,
        }
    }

    #[test]
    fn each_cap_leaves_its_limit_less_what_the_run_counted() {
        let u = usage(&[
            (Some(300), Some(50), Some(0.25)),
            (Some(100), Some(50), Some(0.33)),
        ]);
        assert_eq!(
            budget_left(&caps(Some(1000), Some(900), Some(400), Some(1.0)), &u),
            left(Some(500), Some(500), Some(300), Some(0.42))
        );
        assert_eq!(
            serde_json::to_value(budget_left(&caps(Some(1000), None, None, None), &u)).unwrap(),
            json!({ "total_tokens": 500, "input_tokens": null, "output_tokens": null, "cost_usd": null })
        );
    }

    #[test]
    fn a_run_without_caps_has_every_field_null() {
        let u = usage(&[(Some(10), Some(10), Some(0.1))]);
        assert_eq!(budget_left(&RunCaps::default(), &u), BudgetLeft::default());
        assert_eq!(
            budget_left(&caps(None, None, None, None), &u),
            BudgetLeft::default()
        );
    }

    /// Integer nano-dollars: $0.10 and $0.20 leave nothing of $0.30, and
    /// exactly $0.20 of $0.50. $0.10 leaves exactly $0.20 of $0.30 and $0.58
    /// exactly $0.42 of $1.00, where subtracting in `f64` gives
    /// 0.19999999999999998 and 0.42000000000000004.
    #[test]
    fn cents_are_counted_exactly() {
        let u = usage(&[(Some(1), Some(1), Some(0.1)), (Some(1), Some(1), Some(0.2))]);
        assert_eq!(
            budget_left(&caps(None, None, None, Some(0.3)), &u).cost_usd,
            Some(0.0)
        );
        assert_eq!(
            budget_left(&caps(None, None, None, Some(0.5)), &u).cost_usd,
            Some(0.2)
        );
        let dime = usage(&[(Some(1), Some(1), Some(0.1))]);
        assert_eq!(
            budget_left(&caps(None, None, None, Some(0.3)), &dime).cost_usd,
            Some(0.2)
        );
        let spent = usage(&[(Some(1), Some(1), Some(0.58))]);
        assert_eq!(
            budget_left(&caps(None, None, None, Some(1.0)), &spent).cost_usd,
            Some(0.42)
        );
    }

    #[test]
    fn a_run_past_its_cap_has_nothing_left_of_it() {
        let u = usage(&[(Some(1500), Some(700), Some(2.5))]);
        assert_eq!(
            budget_left(&caps(Some(2000), Some(1000), Some(500), Some(1.0)), &u),
            left(Some(0), Some(0), Some(0), Some(0.0))
        );
    }

    /// A count the cap reads that a step did not report: nothing is left of
    /// that cap. A cap that reads only the other count still leaves what it
    /// leaves.
    #[test]
    fn a_count_the_cap_cannot_read_leaves_nothing() {
        let no_output = usage(&[(Some(100), None, Some(0.1))]);
        assert_eq!(
            budget_left(&caps(Some(1000), Some(1000), Some(1000), None), &no_output),
            left(Some(0), Some(900), Some(0), None)
        );
        let no_input = usage(&[(None, Some(100), Some(0.1))]);
        assert_eq!(
            budget_left(&caps(Some(1000), Some(1000), Some(1000), None), &no_input),
            left(Some(0), Some(0), Some(900), None)
        );
        let no_cost = usage(&[(Some(1), Some(1), Some(0.1)), (Some(1), Some(1), None)]);
        assert_eq!(
            budget_left(&caps(None, None, None, Some(1.0)), &no_cost).cost_usd,
            Some(0.0)
        );
    }

    /// A dollar cap no step could pass (not a number, negative, above the
    /// largest cost a step may report) leaves nothing, not null.
    #[test]
    fn a_dollar_cap_no_step_could_pass_leaves_nothing() {
        let u = usage(&[]);
        for cap in [f64::NAN, -1.0, f64::INFINITY, 10_000.5] {
            assert_eq!(
                budget_left(&caps(None, None, None, Some(cap)), &u).cost_usd,
                Some(0.0),
                "{cap}"
            );
        }
    }

    const SYSTEM: &str = "You plan events.";

    #[test]
    fn a_rendered_prompt_is_a_request_to_send() {
        for payload in [
            json!({ "prompt": "Find a venue in Lisbon" }),
            json!({ "prompt": "Find a venue", "system_prompt": SYSTEM }),
            json!({ "messages": [ { "role": "user", "content": "hi" } ] }),
            json!({ "messages": [ { "content": "no role reads as the user" } ] }),
            json!({ "system_prompt": SYSTEM, "messages": [
                { "role": "system", "content": "another system message" }
            ] }),
            json!({ "prompt": "", "messages": [ { "role": "assistant", "content": "" } ] }),
        ] {
            assert_eq!(missing_prompt(&payload), None, "{payload}");
        }
    }

    #[test]
    fn a_prompt_the_scheduler_could_not_render_is_missing() {
        let payload = json!({
            "prompt_error": "prompt template: `{{ state.city }}`: the run's state has no `city`",
            "messages": [ { "role": "user", "content": "a conversation to fall back on" } ]
        });
        assert_eq!(
            missing_prompt(&payload).as_deref(),
            Some(
                "ModelNodeExecutor: prompt template: `{{ state.city }}`: the run's state has no \
                 `city`"
            )
        );
    }

    #[test]
    fn no_prompt_and_no_conversation_is_missing() {
        let none = Some("ModelNodeExecutor: no prompt or messages provided");
        for payload in [
            json!({}),
            json!({ "prompt": "" }),
            json!({ "prompt": 7 }),
            json!({ "messages": [] }),
            json!({ "messages": "not a list" }),
            json!({ "system_prompt": SYSTEM, "messages": [
                { "role": "system", "content": SYSTEM }
            ] }),
        ] {
            assert_eq!(missing_prompt(&payload).as_deref(), none, "{payload}");
        }
    }

    /// The executor reads `prompt_error` only as text; another shape is
    /// passed over, and the prompt decides.
    #[test]
    fn a_prompt_error_that_is_not_text_is_passed_over() {
        assert_eq!(
            missing_prompt(&json!({ "prompt_error": 1, "prompt": "Find a venue" })),
            None
        );
    }

    #[test]
    fn only_a_model_node_its_worker_runs_is_a_worker_model_step() {
        let model = |executor: Option<&str>| {
            let mut kind =
                json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "" });
            if let Some(e) = executor {
                kind["executor"] = json!(e);
            }
            serde_json::from_value::<NodeKind>(kind).unwrap()
        };
        assert!(is_worker_model(&model(Some("worker"))));
        assert!(!is_worker_model(&model(Some("engine"))));
        assert!(!is_worker_model(&model(None)));
        let python: NodeKind = serde_json::from_value(
            json!({ "type": "python_fn", "module": "m", "function": "f", "output_schema": "" }),
        )
        .unwrap();
        assert!(!is_worker_model(&python));
    }
}

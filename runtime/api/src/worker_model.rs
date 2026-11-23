//! A `model` node with `executor: worker`: a model step its run's own worker
//! runs.
//!
//! The claim hands the step to the run's worker with the request the engine
//! built and what the run's caps leave ([`BudgetLeft`]), after the same gate
//! as every item (`routes::gate_claimed_item`). A step whose request holds no
//! prompt to send is failed there instead, as the in-process model executor
//! fails it (`jamjet_worker::executors::model_node::missing_prompt`), so the
//! worker never sends a prompt of its own making.
//!
//! The completion takes the model's answer ([`ModelResult`]), never an
//! output or a state patch of the worker's own. The engine checks it against
//! the registered node ([`ModelResult::malformed`]) and builds the step's
//! output and state from it with the in-process executor's function
//! (`model_outcome`), so the two executors record the same step.

use jamjet_core::node::ModelExecutor;
use jamjet_core::workflow::ExecutionId;
use jamjet_core::NodeKind;
use jamjet_models::adapter::ToolCall;
use jamjet_models::ModelResponse;
use jamjet_state::backend::StateBackend;
use jamjet_state::usage::cost_to_nanos;
use jamjet_state::{fold_run_usage, RunCaps, RunUsage};
use serde::{Deserialize, Serialize};
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

/// The most tool calls one answer may carry: as many as a tool dispatch
/// node reads (`jamjet_policy::dispatch::MAX_PENDING_CALLS`).
pub const MAX_TOOL_CALLS: usize = 256;

/// The longest model id an answer may report, in characters.
pub const MAX_MODEL_CHARS: usize = 256;

/// A model step's answer, as its run's worker reports it (`model_result` on
/// `POST /work-items/:id/complete`): what the model said, which model said
/// it, and what the call used.
///
/// The worker is trusted for the answer and its usage. The engine checks
/// the answer against the registered node ([`Self::malformed`]), holds the
/// model to the allow lists, and builds the step's output and state from it.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelResult {
    pub content: String,
    pub finish_reason: String,
    /// The model that answered, as the worker reports it.
    pub model: String,
    #[serde(default)]
    pub tool_calls: Vec<ReportedCall>,
    /// Required, with `output_tokens`: the caps count them ([`Self::tokens`]).
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// USD; held to the rule every reported cost is held to.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Optional; when sent, it must be `"USD"`.
    #[serde(default)]
    pub cost_currency: Option<String>,
}

/// One tool call the model asked for.
#[derive(Debug, Clone, Deserialize)]
pub struct ReportedCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ModelResult {
    /// Both token counts, or why the answer cannot be recorded without
    /// them: they are what the run's caps count.
    pub fn tokens(&self) -> Result<(u64, u64), String> {
        match (self.input_tokens, self.output_tokens) {
            (Some(input), Some(output)) => Ok((input, output)),
            _ => Err(TOKENS_REQUIRED.to_string()),
        }
    }

    /// Why this is not an answer the step of `kind` could record, or `None`.
    /// `kind` is the registered node's, never the item's payload: the tools
    /// a call may name are the ones the node offers its model.
    ///
    /// The model id is recorded and checked against allow lists, so it is
    /// not empty, at most [`MAX_MODEL_CHARS`] characters and holds no
    /// control or invisible format character. At most [`MAX_TOOL_CALLS`]
    /// calls. A call has a non-empty id no other call has (its result is
    /// matched back by it) and names a tool the node offers. Calls finish
    /// with `tool_calls`, and `tool_calls` has calls.
    ///
    /// The reasons name a call by its place in the list, never by what the
    /// worker sent.
    pub fn malformed(&self, kind: &NodeKind) -> Option<String> {
        if self.model.is_empty() {
            return Some("model must not be empty".to_string());
        }
        if self.model.chars().count() > MAX_MODEL_CHARS {
            return Some(format!("model is longer than {MAX_MODEL_CHARS} characters"));
        }
        if self
            .model
            .chars()
            .any(jamjet_ir::is_forbidden_identifier_char)
        {
            return Some("model holds a control or invisible format character".to_string());
        }
        if self.tool_calls.len() > MAX_TOOL_CALLS {
            return Some(format!("more than {MAX_TOOL_CALLS} tool calls"));
        }
        match (
            self.finish_reason == "tool_calls",
            self.tool_calls.is_empty(),
        ) {
            (true, true) => {
                return Some("finish_reason is tool_calls and there are no tool calls".to_string())
            }
            (false, false) => {
                return Some("an answer with tool calls must finish with tool_calls".to_string())
            }
            _ => {}
        }
        let offered = offered_tools(kind);
        let mut seen: std::collections::HashMap<&str, usize> = Default::default();
        for (i, call) in self.tool_calls.iter().enumerate() {
            if call.id.is_empty() {
                return Some(format!("tool call {i} has an empty id"));
            }
            if let Some(first) = seen.insert(call.id.as_str(), i) {
                return Some(format!("tool calls {first} and {i} have the same id"));
            }
            if !offered.contains(&call.name.as_str()) {
                return Some(format!(
                    "tool call {i} names a tool the node does not offer"
                ));
            }
        }
        None
    }

    /// The answer as the engine's model adapter returns one, with its
    /// [`Self::tokens`]: what `model_outcome` and the step's event read.
    pub fn response(&self, (input_tokens, output_tokens): (u64, u64)) -> ModelResponse {
        ModelResponse {
            content: self.content.clone(),
            model: self.model.clone(),
            finish_reason: self.finish_reason.clone(),
            input_tokens,
            output_tokens,
            structured: None,
            tool_calls: self
                .tool_calls
                .iter()
                .map(|c| ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect(),
            cost_usd: self.cost_usd,
        }
    }
}

/// The words for an answer that leaves out a token count, parsed or not.
pub const TOKENS_REQUIRED: &str =
    "model_result must report input_tokens and output_tokens, which the run's caps count";

/// Usage in an answer that does not parse which no cap can count: a token
/// count left out or of the wrong type, or a cost of the wrong type. Under a
/// cap that reads it the step may have paid with nothing to count, so it
/// fails as one that left the usage out does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadableUsage {
    /// `input_tokens` or `output_tokens` that is not a whole number from 0
    /// to `u64::MAX` (negative, fractional, a string, past the range).
    Tokens(&'static str),
    /// `input_tokens` or `output_tokens` left out or null, or an answer that
    /// is not an object at all. The answer is not recorded, so the caps
    /// count nothing of the step, as for a parsed answer that leaves the
    /// count out ([`ModelResult::tokens`]).
    TokensMissing(&'static str),
    /// `cost_usd` that is not a number, or `cost_currency` that is not text.
    Cost(&'static str),
}

impl UnreadableUsage {
    /// The field, as the answer and the log name it. Never the value sent.
    pub fn field(self) -> &'static str {
        match self {
            Self::Tokens(field) | Self::TokensMissing(field) | Self::Cost(field) => field,
        }
    }
}

/// The first usage field of `raw`, a `model_result` that did not parse,
/// that no cap can count, or `None`: a token count of the wrong type, then
/// one left out or null (the answer reports no counts the caps could use),
/// then a cost of the wrong type. A cost left out or null is not read here:
/// the cost rule decides that once the answer parses.
pub fn unreadable_usage(raw: &Value) -> Option<UnreadableUsage> {
    const COUNTS: [&str; 2] = ["input_tokens", "output_tokens"];
    let Some(answer) = raw.as_object() else {
        return Some(UnreadableUsage::TokensMissing(COUNTS[0]));
    };
    let sent = |field: &str| answer.get(field).filter(|v| !v.is_null());
    for field in COUNTS {
        if sent(field).is_some_and(|v| v.as_u64().is_none()) {
            return Some(UnreadableUsage::Tokens(field));
        }
    }
    for field in COUNTS {
        if sent(field).is_none() {
            return Some(UnreadableUsage::TokensMissing(field));
        }
    }
    if sent("cost_usd").is_some_and(|v| !v.is_number()) {
        return Some(UnreadableUsage::Cost("cost_usd"));
    }
    if sent("cost_currency").is_some_and(|v| !v.is_string()) {
        return Some(UnreadableUsage::Cost("cost_currency"));
    }
    None
}

/// The names of the tools a model node offers its model: each schema's
/// `function.name` (the OpenAI shape the SDK compiles), or a flat schema's
/// `name`. A schema with neither, or an empty one, offers nothing. Empty for
/// any other kind.
pub fn offered_tools(kind: &NodeKind) -> Vec<&str> {
    let NodeKind::Model { tools, .. } = kind else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|schema| {
            schema
                .get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| schema.get("name"))
                .and_then(Value::as_str)
        })
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token count or a cost of the wrong type is named by its field, and
    /// so is a token count left out or null (an answer that is not an object
    /// has none); a cost left out or null, or one of the right type, is not.
    #[test]
    fn usage_no_cap_can_count_is_named_by_its_field() {
        // An answer that reports both counts, with `field` set to `value`.
        let with = |field: &str, value: Value| {
            let mut answer = json!({
                "content": "", "finish_reason": "stop", "model": "m",
                "input_tokens": 1, "output_tokens": 2
            });
            answer[field] = value;
            unreadable_usage(&answer)
        };
        for value in [
            json!(-5),
            json!("812"),
            json!(812.5),
            json!(1e20),
            json!([1]),
        ] {
            assert_eq!(
                with("input_tokens", value.clone()),
                Some(UnreadableUsage::Tokens("input_tokens")),
                "{value}"
            );
            assert_eq!(
                with("output_tokens", value.clone()),
                Some(UnreadableUsage::Tokens("output_tokens")),
                "{value}"
            );
        }
        assert_eq!(
            with("cost_usd", json!("0.0031")),
            Some(UnreadableUsage::Cost("cost_usd"))
        );
        assert_eq!(
            with("cost_currency", json!(7)),
            Some(UnreadableUsage::Cost("cost_currency"))
        );
        for (field, value) in [
            ("input_tokens", json!(0)),
            ("output_tokens", json!(u64::MAX)),
            ("cost_usd", json!(-1.0)),
            ("cost_usd", Value::Null),
            ("cost_currency", json!("EUR")),
            ("content", json!(7)),
        ] {
            assert_eq!(with(field, value.clone()), None, "{field}: {value}");
        }
        // Counts left out or null are named, the first one missing, and
        // before a cost that does not parse.
        for field in ["input_tokens", "output_tokens"] {
            assert_eq!(
                with(field, Value::Null),
                Some(UnreadableUsage::TokensMissing(field)),
                "{field}: null"
            );
            let mut answer = json!({ "content": 7, "input_tokens": 1, "output_tokens": 2 });
            answer.as_object_mut().unwrap().remove(field);
            assert_eq!(
                unreadable_usage(&answer),
                Some(UnreadableUsage::TokensMissing(field)),
                "{field}: left out"
            );
        }
        assert_eq!(
            unreadable_usage(&json!({ "cost_usd": "0.0031" })),
            Some(UnreadableUsage::TokensMissing("input_tokens"))
        );
        assert_eq!(
            unreadable_usage(&json!({ "input_tokens": "1", "cost_usd": "0.1" })),
            Some(UnreadableUsage::Tokens("input_tokens"))
        );
        // An answer that is not an object reports no counts.
        for raw in [json!([{ "input_tokens": 5, "output_tokens": 5 }]), json!(3)] {
            assert_eq!(
                unreadable_usage(&raw),
                Some(UnreadableUsage::TokensMissing("input_tokens")),
                "{raw}"
            );
        }
        assert_eq!(
            UnreadableUsage::TokensMissing("output_tokens").field(),
            "output_tokens"
        );
    }
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

    /// A model node offering `tools`, by name, in the OpenAI shape the SDK
    /// compiles.
    fn offering(tools: &[&str]) -> NodeKind {
        serde_json::from_value(json!({
            "type": "model", "model_ref": "m", "prompt_ref": "p", "output_schema": "",
            "executor": "worker",
            "tools": tools.iter().map(|t| json!({
                "type": "function", "function": { "name": t, "parameters": {} }
            })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    fn answer(model_result: Value) -> ModelResult {
        serde_json::from_value(model_result).unwrap()
    }

    fn good() -> Value {
        json!({
            "content": "", "finish_reason": "tool_calls", "model": "vertex_ai/gemini-2.5-pro",
            "tool_calls": [ { "id": "c1", "name": "search", "arguments": { "q": "venues" } } ],
            "input_tokens": 812, "output_tokens": 95, "cost_usd": 0.0031
        })
    }

    fn with(key: &str, value: Value) -> Value {
        let mut a = good();
        a[key] = value;
        a
    }

    fn call(id: &str, name: &str) -> Value {
        json!({ "id": id, "name": name, "arguments": {} })
    }

    #[test]
    fn a_well_formed_answer_is_taken() {
        assert_eq!(answer(good()).malformed(&offering(&["search"])), None);
        let text = json!({
            "content": "Book the hall.", "finish_reason": "stop", "model": "m",
            "input_tokens": 1, "output_tokens": 1
        });
        assert_eq!(answer(text).malformed(&offering(&[])), None);
    }

    #[test]
    fn both_token_counts_are_reported() {
        assert_eq!(answer(good()).tokens(), Ok((812, 95)));
        for (what, keys) in [
            ("input", &["input_tokens"][..]),
            ("output", &["output_tokens"][..]),
            ("both", &["input_tokens", "output_tokens"][..]),
        ] {
            let mut a = good();
            for k in keys {
                a.as_object_mut().unwrap().remove(*k);
            }
            assert_eq!(
                answer(a).tokens(),
                Err(
                    "model_result must report input_tokens and output_tokens, which the \
                     run's caps count"
                        .to_string()
                ),
                "{what}"
            );
        }
    }

    /// The tools a call may name are the node's: each schema's
    /// `function.name`, or a flat schema's `name`. A schema with neither, or
    /// an empty one, offers nothing.
    #[test]
    fn a_call_names_a_tool_the_node_offers() {
        let kind: NodeKind = serde_json::from_value(json!({
            "type": "model", "model_ref": "m", "prompt_ref": "p", "output_schema": "",
            "tools": [
                { "type": "function", "function": { "name": "search" } },
                { "name": "book", "input_schema": {} },
                { "type": "function", "function": { "description": "no name" } },
                { "type": "function", "function": { "name": "" } },
                { "name": 7 }
            ]
        }))
        .unwrap();
        assert_eq!(offered_tools(&kind), vec!["search", "book"]);
        let calls = |names: &[&str]| {
            with(
                "tool_calls",
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| call(&format!("c{i}"), n))
                    .collect::<Vec<_>>()
                    .into(),
            )
        };
        assert_eq!(answer(calls(&["search", "book"])).malformed(&kind), None);
        for names in [
            &["delete_records"][..],
            &["search", "delete_records"],
            &[""],
        ] {
            assert_eq!(
                answer(calls(names)).malformed(&kind),
                Some(format!(
                    "tool call {} names a tool the node does not offer",
                    names.len() - 1
                )),
                "{names:?}"
            );
        }
        assert_eq!(
            answer(good()).malformed(&offering(&[])),
            Some("tool call 0 names a tool the node does not offer".to_string())
        );
        let python: NodeKind = serde_json::from_value(
            json!({ "type": "python_fn", "module": "m", "function": "f", "output_schema": "" }),
        )
        .unwrap();
        assert!(offered_tools(&python).is_empty());
    }

    #[test]
    fn every_call_has_an_id_of_its_own() {
        let kind = offering(&["search"]);
        assert_eq!(
            answer(with(
                "tool_calls",
                json!([call("c1", "search"), call("c1", "search")])
            ))
            .malformed(&kind),
            Some("tool calls 0 and 1 have the same id".to_string())
        );
        assert_eq!(
            answer(with(
                "tool_calls",
                json!([
                    call("a", "search"),
                    call("b", "search"),
                    call("a", "search")
                ])
            ))
            .malformed(&kind),
            Some("tool calls 0 and 2 have the same id".to_string())
        );
        assert_eq!(
            answer(with(
                "tool_calls",
                json!([call("c1", "search"), call("", "search")])
            ))
            .malformed(&kind),
            Some("tool call 1 has an empty id".to_string())
        );
    }

    #[test]
    fn the_finish_reason_and_the_calls_agree() {
        let kind = offering(&["search"]);
        assert_eq!(
            answer(with("tool_calls", json!([]))).malformed(&kind),
            Some("finish_reason is tool_calls and there are no tool calls".to_string())
        );
        let mut none = good();
        none.as_object_mut().unwrap().remove("tool_calls");
        assert_eq!(
            answer(none).malformed(&kind),
            Some("finish_reason is tool_calls and there are no tool calls".to_string())
        );
        for reason in ["stop", "length", "tool_use", ""] {
            assert_eq!(
                answer(with("finish_reason", json!(reason))).malformed(&kind),
                Some("an answer with tool calls must finish with tool_calls".to_string()),
                "{reason}"
            );
        }
    }

    #[test]
    fn at_most_256_calls() {
        let kind = offering(&["search"]);
        let calls = |n: usize| -> Value {
            (0..n)
                .map(|i| call(&format!("c{i}"), "search"))
                .collect::<Vec<_>>()
                .into()
        };
        assert_eq!(
            answer(with("tool_calls", calls(256))).malformed(&kind),
            None
        );
        assert_eq!(
            answer(with("tool_calls", calls(257))).malformed(&kind),
            Some("more than 256 tool calls".to_string())
        );
    }

    /// The model id is recorded and checked against allow lists: not empty,
    /// at most 256 characters (not bytes), no control or invisible format
    /// character.
    #[test]
    fn the_model_id_is_bounded_and_visible() {
        let kind = offering(&["search"]);
        for model in [
            "m".repeat(256),
            "é".repeat(256),
            "openai/gpt-4o".to_string(),
        ] {
            assert_eq!(
                answer(with("model", json!(model))).malformed(&kind),
                None,
                "{model}"
            );
        }
        for (model, why) in [
            (String::new(), "model must not be empty"),
            ("m".repeat(257), "model is longer than 256 characters"),
            ("é".repeat(257), "model is longer than 256 characters"),
            (
                "vertex_ai/gemini\nopenai/gpt-4o".to_string(),
                "model holds a control or invisible format character",
            ),
            (
                "vertex_ai/gemini\u{202E}".to_string(),
                "model holds a control or invisible format character",
            ),
            (
                "vertex_ai/\u{200B}gemini".to_string(),
                "model holds a control or invisible format character",
            ),
        ] {
            assert_eq!(
                answer(with("model", json!(model))).malformed(&kind),
                Some(why.to_string()),
                "{model:?}"
            );
        }
    }

    /// The answer as the engine's model adapter returns one: what
    /// `model_outcome` and the step's event read.
    #[test]
    fn the_response_carries_the_answer() {
        let a = answer(good());
        let response = a.response(a.tokens().unwrap());
        assert_eq!(response.content, "");
        assert_eq!(response.model, "vertex_ai/gemini-2.5-pro");
        assert_eq!(response.finish_reason, "tool_calls");
        assert_eq!((response.input_tokens, response.output_tokens), (812, 95));
        assert_eq!(response.cost_usd, Some(0.0031));
        assert_eq!(response.structured, None);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            (
                response.tool_calls[0].id.as_str(),
                response.tool_calls[0].name.as_str(),
                &response.tool_calls[0].arguments
            ),
            ("c1", "search", &json!({ "q": "venues" }))
        );
    }
}

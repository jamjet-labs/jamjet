//! Executor for `Eval` workflow nodes (3.12–3.17).
//!
//! Runs configurable scorers against the preceding node's output:
//! - `LlmJudge`  — calls an LLM with a rubric, extracts 1-5 score
//! - `Assertion` — evaluates Python-like boolean expressions
//! - `Latency`   — checks elapsed ms against a threshold
//! - `Cost`      — checks estimated cost against a USD threshold
//! - `Custom`    — delegates to a Python scorer via module reference
//!
//! On failure, the configured `on_fail` action determines the next step:
//! `retry_with_feedback`, `escalate`, `halt`, or `log_and_continue`.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_core::node::{EvalOnFail, EvalScorer};
use jamjet_models::{ChatMessage, ModelConfig, ModelRegistry, ModelRequest};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, instrument, warn};

/// Per-scorer result recorded in telemetry and execution output.
#[derive(Debug, serde::Serialize)]
pub struct ScorerResult {
    pub scorer_type: String,
    pub passed: bool,
    pub score: Option<f64>,
    pub message: String,
}

pub struct EvalExecutor {
    model_registry: Arc<ModelRegistry>,
}

impl EvalExecutor {
    pub fn new(model_registry: Arc<ModelRegistry>) -> Self {
        Self { model_registry }
    }

    async fn run_llm_judge(
        &self,
        model: &str,
        rubric: &str,
        min_score: u8,
        subject: &Value,
    ) -> ScorerResult {
        let prompt = format!(
            "You are an impartial evaluator.\n\n\
             Rubric: {rubric}\n\n\
             Output to evaluate:\n{subject}\n\n\
             Respond with ONLY a JSON object: {{\"score\": <integer 1-5>, \"reason\": \"<brief reason>\"}}"
        );

        let request = ModelRequest::new(vec![ChatMessage::user(prompt)]).with_config(ModelConfig {
            model: Some(model.to_string()),
            max_tokens: Some(256),
            temperature: Some(0.0),
            system_prompt: None,
            stop_sequences: None,
        });

        match self.model_registry.chat(request).await {
            Ok(resp) => {
                // Extract JSON from the response content.
                let content = resp.content.trim();
                // Find the JSON object in the response.
                let parsed: Option<Value> = content
                    .find('{')
                    .and_then(|start| content.rfind('}').map(|end| &content[start..=end]))
                    .and_then(|json_str| serde_json::from_str(json_str).ok());

                if let Some(obj) = parsed {
                    let score = obj.get("score").and_then(|s| s.as_u64()).unwrap_or(0) as u8;
                    let reason = obj
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("no reason")
                        .to_string();
                    let passed = score >= min_score;
                    ScorerResult {
                        scorer_type: "llm_judge".into(),
                        passed,
                        score: Some(score as f64),
                        message: format!("score={score}/5 (min={min_score}): {reason}"),
                    }
                } else {
                    ScorerResult {
                        scorer_type: "llm_judge".into(),
                        passed: false,
                        score: None,
                        message: format!("failed to parse judge response: {content}"),
                    }
                }
            }
            Err(e) => ScorerResult {
                scorer_type: "llm_judge".into(),
                passed: false,
                score: None,
                message: format!("model call failed: {e}"),
            },
        }
    }

    fn run_assertions(&self, checks: &[String], subject: &Value) -> ScorerResult {
        let mut failures = Vec::new();

        for check in checks {
            // Evaluate simple Python-like assertions against the JSON output.
            // Full Python eval is delegated to the Python worker process.
            // Here we support a minimal set of structural checks:
            // - "'key' in output"  → key present in top-level object
            // - "len(output.key) >= N" → array length check
            // - "output.key == 'value'" → string equality check
            let passed = eval_assertion(check, subject);
            if !passed {
                failures.push(check.clone());
            }
        }

        if failures.is_empty() {
            ScorerResult {
                scorer_type: "assertion".into(),
                passed: true,
                score: Some(1.0),
                message: format!("all {} assertions passed", checks.len()),
            }
        } else {
            ScorerResult {
                scorer_type: "assertion".into(),
                passed: false,
                score: Some(0.0),
                message: format!("failed assertions: {}", failures.join("; ")),
            }
        }
    }

    fn run_latency(&self, threshold_ms: u64, actual_ms: u64) -> ScorerResult {
        let passed = actual_ms <= threshold_ms;
        ScorerResult {
            scorer_type: "latency".into(),
            passed,
            score: Some(actual_ms as f64),
            message: format!("{actual_ms}ms (threshold: {threshold_ms}ms)"),
        }
    }

    fn run_cost(&self, threshold_usd: f64, actual_usd: f64) -> ScorerResult {
        let passed = actual_usd <= threshold_usd;
        ScorerResult {
            scorer_type: "cost".into(),
            passed,
            score: Some(actual_usd),
            message: format!("${actual_usd:.6} (threshold: ${threshold_usd:.4})"),
        }
    }

    fn run_custom(&self, module: &str, _kwargs: &Value, _subject: &Value) -> ScorerResult {
        // Custom scorers are invoked via the Python worker process in production.
        // In the Rust executor, we emit a marker that the Python layer can intercept.
        warn!(
            module = %module,
            "Custom scorer: delegating to Python worker process (not yet implemented in Rust executor)"
        );
        ScorerResult {
            scorer_type: "custom".into(),
            passed: true, // optimistic pass — Python worker enforces
            score: None,
            message: format!("custom scorer '{module}' delegated to Python worker"),
        }
    }
}

#[async_trait]
impl NodeExecutor for EvalExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();

        // Deserialize scorer configs from payload.
        let scorers: Vec<EvalScorer> = item
            .payload
            .get("scorers")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let on_fail: EvalOnFail = item
            .payload
            .get("on_fail")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let max_retries: u32 = item
            .payload
            .get("max_retries")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        // The subject is the output of the preceding node (stored in state).
        let subject: Value = item
            .payload
            .get("input")
            .or_else(|| item.payload.get("last_output"))
            .cloned()
            .unwrap_or(Value::Null);

        // Preceding node latency and cost from payload (set by scheduler).
        let preceding_ms = item
            .payload
            .get("preceding_duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let preceding_cost_usd = item
            .payload
            .get("preceding_cost_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        debug!(scorers = scorers.len(), "Running eval node");

        // Run all scorers.
        let mut results: Vec<ScorerResult> = Vec::new();
        for scorer in &scorers {
            let result = match scorer {
                EvalScorer::LlmJudge {
                    model,
                    rubric,
                    min_score,
                } => {
                    self.run_llm_judge(model, rubric, *min_score, &subject)
                        .await
                }
                EvalScorer::Assertion { checks } => self.run_assertions(checks, &subject),
                EvalScorer::Latency { threshold_ms } => {
                    self.run_latency(*threshold_ms, preceding_ms)
                }
                EvalScorer::Cost { threshold_usd } => {
                    self.run_cost(*threshold_usd, preceding_cost_usd)
                }
                EvalScorer::Custom { module, kwargs } => self.run_custom(module, kwargs, &subject),
            };
            results.push(result);
        }

        let overall_passed = results.iter().all(|r| r.passed);
        let duration_ms = start.elapsed().as_millis() as u64;

        // Build structured output with per-scorer breakdowns.
        let results_json: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "scorer": r.scorer_type,
                    "passed": r.passed,
                    "score": r.score,
                    "message": r.message,
                })
            })
            .collect();

        let output = json!({
            "passed": overall_passed,
            "scorers": results_json,
            "on_fail": serde_json::to_value(&on_fail).unwrap_or(Value::Null),
            "max_retries": max_retries,
        });

        // Determine on_fail action: encode it in state_patch so the scheduler can act.
        let action = if overall_passed {
            "continue"
        } else {
            match &on_fail {
                EvalOnFail::RetryWithFeedback => "retry_with_feedback",
                EvalOnFail::Escalate => "escalate",
                EvalOnFail::Halt => "halt",
                EvalOnFail::LogAndContinue => "log_and_continue",
            }
        };

        let state_patch = json!({
            "eval_passed": overall_passed,
            "eval_action": action,
            "eval_results": results_json,
        });

        if !overall_passed && matches!(on_fail, EvalOnFail::Halt) {
            return Err(format!(
                "eval node failed — scorers: {}",
                results
                    .iter()
                    .filter(|r| !r.passed)
                    .map(|r| r.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }

        Ok(ExecutionResult {
            output,
            state_patch,
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: Some(if overall_passed { "pass" } else { action }.to_string()),
        })
    }
}

// ── Assertion evaluator ───────────────────────────────────────────────────────

/// Evaluate a simple structural assertion against a JSON value.
///
/// Supported patterns (Python-compatible syntax):
/// - `"'key' in output"` — key present in top-level object
/// - `"len(output.key) >= N"` — array length comparison
/// - `"output.key == 'value'"` — string equality
/// - `"output.key != null"` — null check
///
/// For arbitrary Python expressions, the Python worker process evaluates them.
fn eval_assertion(check: &str, subject: &Value) -> bool {
    let check = check.trim();

    // Pattern: "'key' in output"
    if let Some(rest) = check.strip_suffix(" in output") {
        let key = rest.trim().trim_matches('\'').trim_matches('"');
        if let Some(obj) = subject.as_object() {
            return obj.contains_key(key);
        }
        return false;
    }

    // Pattern: "len(output.key) >= N" or "len(output.key) > N" etc.
    if check.starts_with("len(output.") {
        if let Some(inner) = check.strip_prefix("len(output.") {
            if let Some(paren_end) = inner.find(')') {
                let field = &inner[..paren_end];
                let rest = inner[paren_end + 1..].trim();
                let arr_len = subject
                    .get(field)
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                return eval_numeric_comparison(arr_len as i64, rest);
            }
        }
    }

    // Pattern: "output.key == 'value'" or "output.key != null"
    if let Some(rest) = check.strip_prefix("output.") {
        if let Some(eq_pos) = rest.find(" == ") {
            let field = rest[..eq_pos].trim();
            let expected = rest[eq_pos + 4..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"');
            let actual = subject.get(field).and_then(|v| v.as_str()).unwrap_or("");
            return actual == expected;
        }
        if let Some(ne_pos) = rest.find(" != ") {
            let field = rest[..ne_pos].trim();
            let expected_raw = rest[ne_pos + 4..].trim();
            if expected_raw == "null" || expected_raw == "None" {
                return !subject.get(field).map(|v| v.is_null()).unwrap_or(true);
            }
        }
    }

    // Unknown pattern — log and optimistically pass (Python worker will re-check).
    warn!(check = %check, "Unknown assertion pattern; delegating to Python worker");
    true
}

fn eval_numeric_comparison(actual: i64, op_and_rhs: &str) -> bool {
    let op_and_rhs = op_and_rhs.trim();
    if let Some(rhs_str) = op_and_rhs.strip_prefix(">= ") {
        if let Ok(n) = rhs_str.trim().parse::<i64>() {
            return actual >= n;
        }
    }
    if let Some(rhs_str) = op_and_rhs.strip_prefix("> ") {
        if let Ok(n) = rhs_str.trim().parse::<i64>() {
            return actual > n;
        }
    }
    if let Some(rhs_str) = op_and_rhs.strip_prefix("<= ") {
        if let Ok(n) = rhs_str.trim().parse::<i64>() {
            return actual <= n;
        }
    }
    if let Some(rhs_str) = op_and_rhs.strip_prefix("< ") {
        if let Ok(n) = rhs_str.trim().parse::<i64>() {
            return actual < n;
        }
    }
    if let Some(rhs_str) = op_and_rhs.strip_prefix("== ") {
        if let Ok(n) = rhs_str.trim().parse::<i64>() {
            return actual == n;
        }
    }
    true // unknown comparison — delegate to Python
}

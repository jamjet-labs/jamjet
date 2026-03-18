//! Executor for `Coordinator` workflow nodes.
//!
//! Runs the three-phase coordinator pipeline: discovery → scoring → decision,
//! delegating each phase to the Python strategy bridge via REST.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_scheduler::strategy_bridge::{
    DecideRequest, DiscoverRequest, ScoreRequest, StrategyBridge,
};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use tracing::{debug, instrument, warn};

pub struct CoordinatorExecutor {
    bridge: StrategyBridge,
}

impl CoordinatorExecutor {
    pub fn new(strategy_bridge_url: String) -> Self {
        Self {
            bridge: StrategyBridge::new(strategy_bridge_url),
        }
    }
}

#[async_trait]
impl NodeExecutor for CoordinatorExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();
        let p = &item.payload;

        // Extract config from payload
        let task = p
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let required_skills: Vec<String> = p
            .get("required_skills")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let preferred_skills: Vec<String> = p
            .get("preferred_skills")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let trust_domain = p
            .get("trust_domain")
            .and_then(|v| v.as_str())
            .map(String::from);
        let strategy_name = p
            .get("strategy")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let output_key = p
            .get("output_key")
            .and_then(|v| v.as_str())
            .unwrap_or("result")
            .to_string();
        let threshold = p
            .get("tiebreaker")
            .and_then(|t| t.get("threshold"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.1);
        let tiebreaker_model = p
            .get("tiebreaker")
            .and_then(|t| t.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-6")
            .to_string();
        let weights = p.get("weights").cloned().unwrap_or(json!({}));
        let context = p.get("input").cloned().unwrap_or(json!({}));

        debug!(task = %task, strategy = %strategy_name, "Coordinator: starting discovery");

        // Phase 1: Discovery
        let discover_resp = self
            .bridge
            .discover(DiscoverRequest {
                task: task.clone(),
                required_skills: required_skills.clone(),
                preferred_skills: preferred_skills.clone(),
                trust_domain: trust_domain.clone(),
                strategy_name: strategy_name.clone(),
                context: context.clone(),
            })
            .await
            .map_err(|e| format!("Discovery failed: {e}"))?;

        let discovery_event = json!({
            "type": "coordinator_discovery",
            "node_id": item.node_id,
            "query_skills": required_skills,
            "query_trust_domain": trust_domain,
            "candidates": discover_resp.candidates.iter()
                .map(|c| json!({"uri": &c.uri, "skills": &c.skills}))
                .collect::<Vec<_>>(),
            "filtered_out": discover_resp.filtered_out,
        });

        // Handle zero candidates
        if discover_resp.candidates.is_empty() {
            let decision_event = json!({
                "type": "coordinator_decision",
                "node_id": item.node_id,
                "selected": null,
                "method": "no_candidates",
                "confidence": 0.0,
                "rejected": [],
            });
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(ExecutionResult {
                output: json!(null),
                state_patch: json!({
                    "coordinator_events": [discovery_event, decision_event]
                }),
                duration_ms,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            });
        }

        // Handle single candidate — skip scoring
        if discover_resp.candidates.len() == 1 {
            let selected = discover_resp.candidates[0].uri.clone();
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(ExecutionResult {
                output: json!({ output_key: selected.clone() }),
                state_patch: json!({
                    "coordinator_events": [
                        discovery_event,
                        {
                            "type": "coordinator_scoring",
                            "node_id": &item.node_id,
                            "rankings": [{"uri": &selected, "composite": 1.0}],
                            "spread": 1.0,
                            "weights": &weights
                        },
                        {
                            "type": "coordinator_decision",
                            "node_id": &item.node_id,
                            "selected": &selected,
                            "method": "single_candidate",
                            "confidence": 1.0,
                            "rejected": []
                        },
                    ]
                }),
                duration_ms,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            });
        }

        // Phase 2: Scoring
        let score_resp = self
            .bridge
            .score(ScoreRequest {
                task: task.clone(),
                candidates: discover_resp.candidates,
                weights: weights.clone(),
                context: context.clone(),
            })
            .await
            .map_err(|e| format!("Scoring failed: {e}"))?;

        // Guard: empty rankings means scoring produced nothing
        if score_resp.rankings.is_empty() {
            let decision_event = json!({
                "type": "coordinator_decision",
                "node_id": &item.node_id,
                "selected": null,
                "method": "no_candidates",
                "confidence": 0.0,
                "rejected": [],
            });
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(ExecutionResult {
                output: json!(null),
                state_patch: json!({
                    "coordinator_events": [
                        json!({
                            "type": "coordinator_scoring",
                            "node_id": &item.node_id,
                            "rankings": [],
                            "spread": score_resp.spread,
                            "weights": &weights,
                        }),
                        decision_event,
                    ]
                }),
                duration_ms,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            });
        }

        let scoring_event = json!({
            "type": "coordinator_scoring",
            "node_id": item.node_id,
            "rankings": score_resp.rankings,
            "spread": score_resp.spread,
            "weights": weights,
        });

        // Phase 3: Decision
        let used_tiebreaker_model: Option<String>;
        let decide_result: Value = if score_resp.spread > threshold {
            // Fast path: structured scoring wins
            let top = &score_resp.rankings[0];
            let rejected: Vec<Value> = score_resp.rankings[1..]
                .iter()
                .map(|r| json!({"uri": &r.uri, "reason": "lower score"}))
                .collect();
            used_tiebreaker_model = None;
            json!({
                "type": "coordinator_decision",
                "node_id": item.node_id,
                "selected": top.uri,
                "method": "structured",
                "confidence": top.composite,
                "rejected": rejected,
            })
        } else {
            // LLM tiebreaker
            let max_candidates = p
                .get("tiebreaker")
                .and_then(|t| t.get("max_candidates"))
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as usize;
            let top_n: Vec<_> = score_resp.rankings.into_iter().take(max_candidates).collect();

            match self
                .bridge
                .decide(DecideRequest {
                    task: task.clone(),
                    top_candidates: top_n.clone(),
                    threshold,
                    tiebreaker_model: tiebreaker_model.clone(),
                    context: context.clone(),
                })
                .await
            {
                Ok(r) => {
                    let is_llm = r.method == "llm_tiebreaker";
                    used_tiebreaker_model = if is_llm {
                        Some(tiebreaker_model.clone())
                    } else {
                        None
                    };
                    json!({
                        "type": "coordinator_decision",
                        "node_id": item.node_id,
                        "selected": r.selected_uri,
                        "method": r.method,
                        "reasoning": r.reasoning,
                        "confidence": r.confidence,
                        "rejected": r.rejected,
                        "tiebreaker_tokens": r.tiebreaker_tokens,
                        "tiebreaker_cost": r.tiebreaker_cost,
                    })
                }
                Err(e) => {
                    warn!("LLM tiebreaker failed, falling back to top scorer: {e}");
                    used_tiebreaker_model = None;
                    if let Some(first) = top_n.first() {
                        json!({
                            "type": "coordinator_decision",
                            "node_id": item.node_id,
                            "selected": first.uri,
                            "method": "tiebreaker_failed",
                            "reasoning": format!("Tiebreaker error: {e}"),
                            "confidence": first.composite,
                            "rejected": [],
                        })
                    } else {
                        json!({
                            "type": "coordinator_decision",
                            "node_id": item.node_id,
                            "selected": null,
                            "method": "tiebreaker_failed",
                            "reasoning": format!("Tiebreaker error: {e}"),
                            "confidence": 0.0,
                            "rejected": [],
                        })
                    }
                }
            }
        };

        let selected_uri = decide_result
            .get("selected")
            .and_then(|v| v.as_str())
            .map(String::from);
        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(ExecutionResult {
            output: match &selected_uri {
                Some(uri) => json!({ output_key: uri }),
                None => json!(null),
            },
            state_patch: json!({
                "coordinator_events": [discovery_event, scoring_event, decide_result]
            }),
            duration_ms,
            gen_ai_system: None,
            gen_ai_model: used_tiebreaker_model,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

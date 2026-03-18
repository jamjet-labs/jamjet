use serde::{Deserialize, Serialize};

/// Budget constraints for a coordinator routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorBudget {
    /// Maximum allowable cost in USD for the delegated call. None = unlimited.
    pub max_cost_usd: Option<f64>,
    /// Maximum allowable latency in milliseconds. None = unlimited.
    pub max_latency_ms: Option<u64>,
}

/// Configuration for the LLM tiebreaker step when scores are too close to call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TiebreakerConfig {
    /// Model identifier to invoke for tiebreaking (e.g. "claude-3-5-haiku-20241022").
    pub model: String,
    /// Composite score difference below which two candidates are considered tied.
    pub threshold: f64,
    /// Maximum number of candidates forwarded to the tiebreaker LLM.
    #[serde(default = "default_max_candidates")]
    pub max_candidates: usize,
}

fn default_max_candidates() -> usize {
    3
}

/// Per-dimension scores for a single agent candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScores {
    /// How well the agent's declared capabilities match the task requirements.
    pub capability_fit: f64,
    /// How well the agent's cost profile fits within the budget.
    pub cost_fit: f64,
    /// How well the agent's latency profile fits within the deadline.
    pub latency_fit: f64,
    /// Whether the agent's trust level is compatible with the caller's policy.
    pub trust_compatibility: f64,
    /// Agent's historical success rate and quality for similar tasks.
    pub historical_performance: f64,
}

impl DimensionScores {
    /// Compute the weighted composite score across all dimensions.
    ///
    /// Uses weight-normalised averaging so that zero-weight dimensions are
    /// ignored rather than dragging the score toward zero.
    pub fn composite(&self, weights: &DimensionWeights) -> f64 {
        let total_weight = weights.capability_fit
            + weights.cost_fit
            + weights.latency_fit
            + weights.trust_compatibility
            + weights.historical_performance;

        if total_weight == 0.0 {
            return 0.0;
        }

        let weighted_sum = self.capability_fit * weights.capability_fit
            + self.cost_fit * weights.cost_fit
            + self.latency_fit * weights.latency_fit
            + self.trust_compatibility * weights.trust_compatibility
            + self.historical_performance * weights.historical_performance;

        weighted_sum / total_weight
    }
}

/// Weights applied to each scoring dimension when computing a composite score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionWeights {
    pub capability_fit: f64,
    pub cost_fit: f64,
    pub latency_fit: f64,
    pub trust_compatibility: f64,
    pub historical_performance: f64,
}

impl Default for DimensionWeights {
    fn default() -> Self {
        Self {
            capability_fit: 1.0,
            cost_fit: 1.0,
            latency_fit: 1.0,
            trust_compatibility: 1.0,
            historical_performance: 1.0,
        }
    }
}

/// Scoring result for a single candidate agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringResult {
    /// URI identifying the candidate agent.
    pub agent_uri: String,
    /// Raw per-dimension scores.
    pub scores: DimensionScores,
    /// Pre-computed composite score.
    pub composite: f64,
}

/// How the coordinator arrived at its final routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMethod {
    /// Only one candidate was available; chosen without further scoring.
    SingleCandidate,
    /// Winner determined by structured multi-dimensional scoring.
    Structured,
    /// Structured scoring produced a tie; an LLM tiebreaker broke it.
    LlmTiebreaker,
    /// No candidates were available; routing failed.
    NoCandidates,
    /// The highest-scoring candidate did not meet the minimum threshold.
    BelowThreshold,
    /// The LLM tiebreaker was invoked but failed (e.g. timeout, error).
    TiebreakerFailed,
}

/// A candidate agent that was considered but not selected, with a reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedAgent {
    /// URI of the rejected agent.
    pub agent_uri: String,
    /// Human-readable reason for rejection.
    pub reason: String,
}

/// Token usage recorded for an LLM tiebreaker call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of input (prompt) tokens consumed.
    pub input: u32,
    /// Number of output (completion) tokens generated.
    pub output: u32,
}

/// The final routing decision produced by the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorDecision {
    /// URI of the selected agent, or None if routing failed.
    pub selected: Option<String>,
    /// How the decision was made.
    pub method: DecisionMethod,
    /// Optional natural-language explanation of the decision (from tiebreaker or scoring).
    pub reasoning: Option<String>,
    /// Confidence in the decision on a [0.0, 1.0] scale.
    pub confidence: f64,
    /// Agents that were considered but rejected, with reasons.
    pub rejected: Vec<RejectedAgent>,
    /// Token usage for the tiebreaker LLM call, if one was made.
    pub tiebreaker_tokens: Option<TokenUsage>,
    /// Estimated cost in USD for the tiebreaker LLM call, if one was made.
    pub tiebreaker_cost: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_equal_weights() {
        let scores = DimensionScores {
            capability_fit: 0.8,
            cost_fit: 0.6,
            latency_fit: 0.7,
            trust_compatibility: 1.0,
            historical_performance: 0.9,
        };
        let weights = DimensionWeights::default();
        let c = scores.composite(&weights);
        // (0.8 + 0.6 + 0.7 + 1.0 + 0.9) / 5 = 4.0 / 5 = 0.8
        assert!((c - 0.8).abs() < 1e-9, "composite={c}");
    }

    #[test]
    fn composite_zero_weight_dimension_ignored() {
        let scores = DimensionScores {
            capability_fit: 1.0,
            cost_fit: 0.0, // irrelevant when weight is 0
            latency_fit: 1.0,
            trust_compatibility: 1.0,
            historical_performance: 1.0,
        };
        let weights = DimensionWeights {
            capability_fit: 1.0,
            cost_fit: 0.0,
            latency_fit: 1.0,
            trust_compatibility: 1.0,
            historical_performance: 1.0,
        };
        let c = scores.composite(&weights);
        // 4.0 / 4.0 = 1.0 (cost_fit dimension dropped)
        assert!((c - 1.0).abs() < 1e-9, "composite={c}");
    }

    #[test]
    fn composite_all_zero_weights_returns_zero() {
        let scores = DimensionScores {
            capability_fit: 0.9,
            cost_fit: 0.9,
            latency_fit: 0.9,
            trust_compatibility: 0.9,
            historical_performance: 0.9,
        };
        let weights = DimensionWeights {
            capability_fit: 0.0,
            cost_fit: 0.0,
            latency_fit: 0.0,
            trust_compatibility: 0.0,
            historical_performance: 0.0,
        };
        assert_eq!(scores.composite(&weights), 0.0);
    }

    #[test]
    fn tiebreaker_config_default_max_candidates() {
        let json = r#"{"model":"claude-3-5-haiku-20241022","threshold":0.05}"#;
        let cfg: TiebreakerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_candidates, 3);
    }

    #[test]
    fn decision_method_snake_case_roundtrip() {
        let method = DecisionMethod::LlmTiebreaker;
        let json = serde_json::to_string(&method).unwrap();
        assert_eq!(json, r#""llm_tiebreaker""#);
        let back: DecisionMethod = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DecisionMethod::LlmTiebreaker));
    }
}

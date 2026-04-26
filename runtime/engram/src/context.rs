//! Context assembly — token-budgeted context building for LLM prompts.
//!
//! The context assembler retrieves facts via hybrid search, ranks them by
//! tier priority, fills a token budget greedily, and formats the output
//! for injection into system prompts, messages, or raw JSON.

use crate::embedding::EmbeddingProvider;
use crate::fact::{Fact, MemoryTier};
use crate::graph::GraphStore;
use crate::retrieve::{HybridRetriever, RetrievalConfig};
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError};
use crate::vector::VectorStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// TokenEstimator
// ---------------------------------------------------------------------------

/// Pluggable token estimation. Implementations convert text to an approximate
/// token count without requiring a full tokenizer dependency.
pub trait TokenEstimator: Send + Sync {
    /// Estimate the number of tokens in `text`.
    fn estimate(&self, text: &str) -> usize;
}

/// Character-based token estimator (~4 chars per token).
/// Accurate to within ~10% for English text across GPT/Claude models.
pub struct CharTokenEstimator;

impl TokenEstimator for CharTokenEstimator {
    fn estimate(&self, text: &str) -> usize {
        // ~4 characters per token is a widely-used heuristic
        text.len().div_ceil(4)
    }
}

// ---------------------------------------------------------------------------
// OutputFormat
// ---------------------------------------------------------------------------

/// Format for the assembled context block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// XML-tagged block for system prompt injection.
    #[default]
    SystemPrompt,
    /// Human-readable Markdown summary.
    Markdown,
    /// Structured JSON with all metadata.
    Raw,
}

// ---------------------------------------------------------------------------
// ContextConfig
// ---------------------------------------------------------------------------

/// Configuration for context assembly.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Maximum number of tokens to include. Default: 2000.
    pub token_budget: usize,
    /// Output format. Default: SystemPrompt.
    pub format: OutputFormat,
    /// Maximum number of candidate facts to retrieve before budget filtering.
    /// Default: 100.
    pub max_candidates: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            token_budget: 2000,
            format: OutputFormat::SystemPrompt,
            max_candidates: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// ContextBlock
// ---------------------------------------------------------------------------

/// The assembled context block returned by `ContextBuilder::build()`.
#[derive(Debug, Clone, Serialize)]
pub struct ContextBlock {
    /// Formatted text ready for prompt injection.
    pub text: String,
    /// Estimated token count of the `text` field.
    pub token_count: usize,
    /// Number of facts included in the output.
    pub facts_included: usize,
    /// Number of facts that were retrieved but omitted due to budget.
    pub facts_omitted: usize,
    /// Breakdown of included facts by tier.
    pub tier_breakdown: HashMap<String, usize>,
}

// ---------------------------------------------------------------------------
// Tier priority
// ---------------------------------------------------------------------------

/// Returns the priority order for `MemoryTier`. Lower value = higher priority.
fn tier_priority(tier: &MemoryTier) -> u8 {
    match tier {
        MemoryTier::Working => 0,
        MemoryTier::Conversation => 1,
        MemoryTier::Knowledge => 2,
    }
}

/// Sort facts by tier priority (Working first, then Conversation, then Knowledge).
/// Within the same tier, preserve the original order (which is by retrieval score).
pub fn sort_by_tier_priority(facts: &mut [Fact]) {
    facts.sort_by_key(|f| tier_priority(&f.tier));
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

/// Format a fact as a bullet point, optionally prefixed with its date.
fn format_fact_line(fact: &Fact) -> String {
    // Prefer valid_from when it has a meaningful timestamp (not today).
    // This is more reliable than event_date metadata which may contain
    // relative words like "today" or "last week" that weren't resolved.
    if fact.valid_from.date_naive() != chrono::Utc::now().date_naive() {
        return format!("- [{}] {}", fact.valid_from.format("%Y-%m-%d"), fact.text);
    }
    // Fall back to event_date metadata only if it looks like an ISO date (starts with digit)
    if let Some(event_date) = fact.metadata.get("event_date").and_then(|v| v.as_str()) {
        if event_date.starts_with(|c: char| c.is_ascii_digit()) {
            return format!("- [{event_date}] {}", fact.text);
        }
    }
    format!("- {}", fact.text)
}

/// Format facts as an XML-tagged block for system prompt injection.
pub fn format_system_prompt(facts: &[Fact]) -> String {
    let mut working = Vec::new();
    let mut conversation = Vec::new();
    let mut knowledge = Vec::new();

    for fact in facts {
        let line = format_fact_line(fact);
        match fact.tier {
            MemoryTier::Working => working.push(line),
            MemoryTier::Conversation => conversation.push(line),
            MemoryTier::Knowledge => knowledge.push(line),
        }
    }

    let mut out = String::from("<memory>\n");

    if !working.is_empty() {
        out.push_str("<working>\n");
        for line in &working {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("</working>\n");
    }
    if !conversation.is_empty() {
        out.push_str("<conversation>\n");
        for line in &conversation {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("</conversation>\n");
    }
    if !knowledge.is_empty() {
        out.push_str("<knowledge>\n");
        for line in &knowledge {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("</knowledge>\n");
    }

    out.push_str("</memory>");
    out
}

/// Format facts as human-readable Markdown.
pub fn format_markdown(facts: &[Fact]) -> String {
    let mut out = String::from("## Memory Context\n\n");

    let mut current_tier: Option<&MemoryTier> = None;
    for fact in facts {
        if current_tier != Some(&fact.tier) {
            let label = match fact.tier {
                MemoryTier::Working => "Working Memory",
                MemoryTier::Conversation => "Conversation",
                MemoryTier::Knowledge => "Knowledge",
            };
            out.push_str(&format!("### {label}\n\n"));
            current_tier = Some(&fact.tier);
        }
        out.push_str(&format_fact_line(fact));
        out.push('\n');
    }

    out
}

/// Format facts as raw JSON (array of objects with text, tier, category, confidence).
pub fn format_raw(facts: &[Fact]) -> String {
    let entries: Vec<serde_json::Value> = facts
        .iter()
        .map(|f| {
            serde_json::json!({
                "id": f.id.to_string(),
                "text": f.text,
                "tier": f.tier,
                "category": f.category,
                "confidence": f.confidence,
            })
        })
        .collect();
    serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// ContextBuilder
// ---------------------------------------------------------------------------

/// Token-budgeted context assembler.
///
/// Retrieves facts via hybrid search, ranks by tier priority, fills
/// a token budget greedily, and returns a `ContextBlock` ready for
/// LLM prompt injection.
pub struct ContextBuilder {
    fact_store: Arc<dyn FactStore>,
    vector_store: Arc<dyn VectorStore>,
    graph_store: Arc<dyn GraphStore>,
    embedding: Arc<dyn EmbeddingProvider>,
    estimator: Box<dyn TokenEstimator>,
    config: ContextConfig,
}

impl ContextBuilder {
    pub fn new(
        fact_store: Arc<dyn FactStore>,
        vector_store: Arc<dyn VectorStore>,
        graph_store: Arc<dyn GraphStore>,
        embedding: Arc<dyn EmbeddingProvider>,
        config: ContextConfig,
    ) -> Self {
        Self {
            fact_store,
            vector_store,
            graph_store,
            embedding,
            estimator: Box::new(CharTokenEstimator),
            config,
        }
    }

    /// Override the default token estimator.
    pub fn with_estimator(mut self, estimator: Box<dyn TokenEstimator>) -> Self {
        self.estimator = estimator;
        self
    }

    /// Build a context block for the given query and scope.
    pub async fn build(&self, query: &str, scope: &Scope) -> Result<ContextBlock, MemoryError> {
        // 1. Retrieve candidates via hybrid search
        let retriever = HybridRetriever::new(
            self.fact_store.clone(),
            self.vector_store.clone(),
            self.graph_store.clone(),
            self.embedding.clone(),
            RetrievalConfig::default(),
        );

        let scored = retriever
            .search(query, scope, self.config.max_candidates)
            .await?;

        // 2. Extract facts and sort by tier priority
        let mut facts: Vec<Fact> = scored.into_iter().map(|sf| sf.fact).collect();
        sort_by_tier_priority(&mut facts);

        // 3. Greedy budget filling
        let mut included: Vec<Fact> = Vec::new();
        let mut token_count: usize = 0;
        let overhead = self.format_overhead();
        let budget_for_facts = self.config.token_budget.saturating_sub(overhead);

        for fact in &facts {
            let fact_tokens = self.estimator.estimate(&fact.text) + 2; // "- " prefix + newline
            if token_count + fact_tokens > budget_for_facts {
                continue; // Skip this fact, try smaller ones
            }
            token_count += fact_tokens;
            included.push(fact.clone());
        }

        let facts_omitted = facts.len() - included.len();

        // 4. Build tier breakdown
        let mut tier_breakdown: HashMap<String, usize> = HashMap::new();
        for fact in &included {
            let tier_name = match fact.tier {
                MemoryTier::Working => "working",
                MemoryTier::Conversation => "conversation",
                MemoryTier::Knowledge => "knowledge",
            };
            *tier_breakdown.entry(tier_name.to_string()).or_insert(0) += 1;
        }

        // 5. Format output
        let text = match self.config.format {
            OutputFormat::SystemPrompt => format_system_prompt(&included),
            OutputFormat::Markdown => format_markdown(&included),
            OutputFormat::Raw => format_raw(&included),
        };

        let total_tokens = self.estimator.estimate(&text);

        Ok(ContextBlock {
            text,
            token_count: total_tokens,
            facts_included: included.len(),
            facts_omitted,
            tier_breakdown,
        })
    }

    /// Estimate the token overhead of the format wrapper (tags, headers, etc.).
    fn format_overhead(&self) -> usize {
        match self.config.format {
            // <memory>\n</memory> + tier tags ≈ 60 chars ≈ 15 tokens
            OutputFormat::SystemPrompt => 15,
            // ## Memory Context\n\n### Tier\n\n ≈ 50 chars ≈ 13 tokens
            OutputFormat::Markdown => 13,
            // JSON brackets and formatting ≈ 20 chars ≈ 5 tokens
            OutputFormat::Raw => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_estimator_basic() {
        let est = CharTokenEstimator;
        // "hello world" = 11 chars → (11+3)/4 = 3 tokens
        assert_eq!(est.estimate("hello world"), 3);
        assert_eq!(est.estimate(""), 0);
        // 100 chars → (100+3)/4 = 25 tokens
        let hundred = "a".repeat(100);
        assert_eq!(est.estimate(&hundred), 25);
    }

    #[test]
    fn context_config_defaults() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.token_budget, 2000);
        assert_eq!(cfg.format, OutputFormat::SystemPrompt);
        assert_eq!(cfg.max_candidates, 100);
    }

    #[test]
    fn tier_priority_ordering() {
        assert!(tier_priority(&MemoryTier::Working) < tier_priority(&MemoryTier::Conversation));
        assert!(tier_priority(&MemoryTier::Conversation) < tier_priority(&MemoryTier::Knowledge));
    }

    #[test]
    fn sort_by_tier_groups_correctly() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let mut facts = vec![
            Fact::new("knowledge fact", scope.clone()).with_tier(MemoryTier::Knowledge),
            Fact::new("working fact", scope.clone()).with_tier(MemoryTier::Working),
            Fact::new("convo fact", scope).with_tier(MemoryTier::Conversation),
        ];
        sort_by_tier_priority(&mut facts);
        assert_eq!(facts[0].tier, MemoryTier::Working);
        assert_eq!(facts[1].tier, MemoryTier::Conversation);
        assert_eq!(facts[2].tier, MemoryTier::Knowledge);
    }

    #[test]
    fn format_system_prompt_groups_by_tier() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let facts = vec![
            Fact::new("ephemeral note", scope.clone()).with_tier(MemoryTier::Working),
            Fact::new("user likes pizza", scope.clone()).with_tier(MemoryTier::Conversation),
            Fact::new("company policy", scope).with_tier(MemoryTier::Knowledge),
        ];
        let output = format_system_prompt(&facts);
        assert!(output.starts_with("<memory>"));
        assert!(output.ends_with("</memory>"));
        assert!(output.contains("<working>"));
        assert!(output.contains("- ephemeral note"));
        assert!(output.contains("<conversation>"));
        assert!(output.contains("- user likes pizza"));
        assert!(output.contains("<knowledge>"));
        assert!(output.contains("- company policy"));
    }

    #[test]
    fn format_system_prompt_omits_empty_tiers() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let facts = vec![Fact::new("just a convo fact", scope).with_tier(MemoryTier::Conversation)];
        let output = format_system_prompt(&facts);
        assert!(!output.contains("<working>"));
        assert!(output.contains("<conversation>"));
        assert!(!output.contains("<knowledge>"));
    }

    #[test]
    fn format_markdown_produces_sections() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let facts = vec![
            Fact::new("user prefers dark mode", scope.clone()).with_tier(MemoryTier::Knowledge),
            Fact::new("user asked about Rust", scope).with_tier(MemoryTier::Knowledge),
        ];
        let output = format_markdown(&facts);
        assert!(output.contains("## Memory Context"));
        assert!(output.contains("### Knowledge"));
        assert!(output.contains("- user prefers dark mode"));
    }

    #[test]
    fn format_raw_produces_valid_json() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let facts = vec![Fact::new("test fact", scope).with_tier(MemoryTier::Conversation)];
        let output = format_raw(&facts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["text"], "test fact");
        assert_eq!(parsed[0]["tier"], "conversation");
    }

    #[test]
    fn format_system_prompt_includes_date_prefix() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let mut fact = Fact::new("User went to dentist", scope).with_tier(MemoryTier::Conversation);
        fact.valid_from = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let output = format_system_prompt(&[fact]);
        assert!(
            output.contains("[2024-03-15] User went to dentist"),
            "got: {output}"
        );
    }

    #[test]
    fn format_fact_line_uses_event_date_metadata() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let mut fact = Fact::new("User ran a 5K", scope).with_tier(MemoryTier::Conversation);
        fact.metadata
            .insert("event_date".to_string(), serde_json::json!("2024-06-01"));
        let line = format_fact_line(&fact);
        assert_eq!(line, "- [2024-06-01] User ran a 5K");
    }

    #[test]
    fn format_fact_line_no_date_for_today() {
        use crate::scope::Scope;
        let scope = Scope::org("test");
        let fact = Fact::new("User likes pizza", scope).with_tier(MemoryTier::Conversation);
        let line = format_fact_line(&fact);
        assert_eq!(line, "- User likes pizza");
    }
}

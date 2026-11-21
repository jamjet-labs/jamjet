//! JamJet Model Adapters
//!
//! Provides a unified `ModelAdapter` trait with concrete implementations for:
//! - Anthropic Claude (via Messages API)
//! - OpenAI (via Chat Completions API)
//! - Google Gemini (via Generative Language API)
//! - Ollama (local inference via /api/chat)
//!
//! Each adapter records OTel GenAI span attributes and returns token counts
//! in the response for telemetry and cost attribution.

pub mod adapter;
pub mod anthropic;
pub mod google;
pub mod http;
pub mod ollama;
pub mod openai;
pub mod registry;
pub mod sidecar;

pub use adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
pub use registry::ModelRegistry;
pub use sidecar::SidecarModelAdapter;

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// The native adapters call a provider API that prices nothing, so each
    /// reports no cost, even when the answer carries a field that looks like
    /// one. A run under a dollar cap then fails closed instead of counting a
    /// made-up price.
    #[test]
    fn native_adapters_report_no_cost() {
        let anthropic = crate::anthropic::AnthropicAdapter::new("test-key")
            .parse_response(json!({
                "model": "claude-sonnet-4-6",
                "content": [{ "type": "text", "text": "hi" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 10, "output_tokens": 5, "cost": 0.01 }
            }))
            .expect("anthropic answer");
        let openai = crate::openai::OpenAiAdapter::new("test-key")
            .parse_response(json!({
                "model": "gpt-4o",
                "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "cost": 0.01 }
            }))
            .expect("openai answer");
        let google = crate::google::GoogleAdapter::new("test-key")
            .parse_response(json!({
                "candidates": [{ "content": { "parts": [{ "text": "hi" }] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5 },
                "modelVersion": "gemini-2.0-flash",
                "cost_usd": 0.01
            }))
            .expect("google answer");
        let ollama = crate::ollama::OllamaAdapter::new("http://127.0.0.1:11434")
            .parse_response(json!({
                "model": "llama3.2:3b",
                "message": { "content": "hi" },
                "prompt_eval_count": 10,
                "eval_count": 5,
                "done_reason": "stop",
                "cost_usd": 0.01
            }))
            .expect("ollama answer");
        let costs = [
            ("anthropic", anthropic.input_tokens, anthropic.cost_usd),
            ("openai", openai.input_tokens, openai.cost_usd),
            ("google", google.input_tokens, google.cost_usd),
            ("ollama", ollama.input_tokens, ollama.cost_usd),
        ];
        assert_eq!(
            costs,
            [
                ("anthropic", 10, None),
                ("openai", 10, None),
                ("google", 10, None),
                ("ollama", 10, None),
            ]
        );
    }
}

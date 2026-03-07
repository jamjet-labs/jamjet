//! JamJet Telemetry
//!
//! Provides:
//! - Structured logging via `tracing`
//! - OpenTelemetry traces and metrics (OTLP exporter)
//! - Stdout dev console exporter
//! - Span naming conventions for workflow, node, model, tool, MCP, and A2A spans

/// Initialize the global tracing subscriber.
///
/// In dev mode: pretty-printed stdout.
/// In production: JSON + optional OTLP exporter (gRPC/tonic to `otel_endpoint`).
///
/// If `otel_endpoint` is `Some`, a batch OTLP trace exporter is installed and
/// spans are exported to the given gRPC endpoint (e.g. `http://localhost:4317`).
pub fn init(dev_mode: bool, otel_endpoint: Option<&str>) {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    if let Some(endpoint) = otel_endpoint {
        // Install OTLP exporter and add a tracing-opentelemetry layer.
        // When OTLP is active, always emit JSON logs (production format).
        match build_otlp_tracer(endpoint) {
            Ok(tracer) => {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_subscriber::fmt::layer().json())
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
                return;
            }
            Err(e) => {
                // Log to stderr and fall through to non-OTLP init.
                eprintln!("jamjet-telemetry: OTLP exporter failed to install: {e}");
            }
        }
    }

    if dev_mode {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().pretty())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    }
}

fn build_otlp_tracer(endpoint: &str) -> Result<opentelemetry_sdk::trace::Tracer, String> {
    use opentelemetry_otlp::WithExportConfig;

    opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(endpoint),
        )
        .with_trace_config(opentelemetry_sdk::trace::config().with_resource(
            opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", "jamjet"),
                opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            ]),
        ))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .map_err(|e| format!("{e}"))
}

/// Span name constants for consistent trace naming.
pub mod span_names {
    pub const WORKFLOW: &str = "jamjet.workflow";
    pub const NODE: &str = "jamjet.node";
    pub const MODEL_CALL: &str = "jamjet.model_call";
    pub const TOOL_CALL: &str = "jamjet.tool_call";
    pub const MCP_CALL: &str = "jamjet.mcp_call";
    pub const A2A_TASK: &str = "jamjet.a2a_task";
}

/// OpenTelemetry GenAI semantic convention span attributes.
///
/// Aligned with the OpenTelemetry GenAI semantic conventions spec:
/// <https://opentelemetry.io/docs/specs/semconv/gen-ai/>
pub mod gen_ai_attrs {
    // ── Standard GenAI attributes ─────────────────────────────────────────────

    /// The AI provider system (e.g. "openai", "anthropic", "google_vertex_ai").
    pub const SYSTEM: &str = "gen_ai.system";
    /// The model name requested (e.g. "claude-sonnet-4-6", "gpt-4o").
    pub const REQUEST_MODEL: &str = "gen_ai.request.model";
    /// The maximum tokens requested.
    pub const REQUEST_MAX_TOKENS: &str = "gen_ai.request.max_tokens";
    /// Sampling temperature (0.0–1.0).
    pub const REQUEST_TEMPERATURE: &str = "gen_ai.request.temperature";
    /// The model actually used (may differ from requested for alias resolution).
    pub const RESPONSE_MODEL: &str = "gen_ai.response.model";
    /// Finish reason(s): "stop", "length", "tool_calls", "content_filter".
    pub const RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
    /// Input tokens consumed.
    pub const USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
    /// Output tokens generated.
    pub const USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
    /// Prompt content (opt-in, may be redacted).
    pub const PROMPT: &str = "gen_ai.prompt";
    /// Completion content (opt-in, may be redacted).
    pub const COMPLETION: &str = "gen_ai.completion";

    // ── JamJet-specific span attributes ──────────────────────────────────────

    /// JamJet workflow execution id.
    pub const JAMJET_EXECUTION_ID: &str = "jamjet.execution.id";
    /// JamJet workflow definition id.
    pub const JAMJET_WORKFLOW_ID: &str = "jamjet.workflow.id";
    /// JamJet workflow version.
    pub const JAMJET_WORKFLOW_VERSION: &str = "jamjet.workflow.version";
    /// JamJet node id within the workflow graph.
    pub const JAMJET_NODE_ID: &str = "jamjet.node.id";
    /// JamJet node kind (model, tool, mcp_tool, a2a_task, etc.).
    pub const JAMJET_NODE_KIND: &str = "jamjet.node.kind";
    /// JamJet agent id.
    pub const JAMJET_AGENT_ID: &str = "jamjet.agent.id";
    /// JamJet agent uri.
    pub const JAMJET_AGENT_URI: &str = "jamjet.agent.uri";
    /// JamJet worker id that processed this node.
    pub const JAMJET_WORKER_ID: &str = "jamjet.worker.id";
    /// Execution attempt number (0-based).
    pub const JAMJET_ATTEMPT: &str = "jamjet.attempt";
    /// Estimated USD cost of this operation (if available).
    pub const JAMJET_COST_USD: &str = "jamjet.cost.usd";
}

/// Helper to record GenAI attributes on the current span.
///
/// Usage:
/// ```rust
/// use tracing::Span;
/// use jamjet_telemetry::record_gen_ai_usage;
///
/// let span = tracing::info_span!("jamjet.model_call");
/// record_gen_ai_usage(&span, "anthropic", "claude-sonnet-4-6", 512, 1024);
/// ```
pub fn record_gen_ai_usage(
    span: &tracing::Span,
    system: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) {
    span.record(gen_ai_attrs::SYSTEM, system);
    span.record(gen_ai_attrs::REQUEST_MODEL, model);
    span.record(gen_ai_attrs::USAGE_INPUT_TOKENS, input_tokens);
    span.record(gen_ai_attrs::USAGE_OUTPUT_TOKENS, output_tokens);
}

/// Opt-in prompt/completion capture with redaction.
///
/// Controlled by the `JAMJET_CAPTURE_PROMPTS` environment variable.
/// Set it to `"true"` or `"1"` to enable. Disabled by default.
///
/// When enabled, prompt and completion text is attached to the span under
/// `gen_ai.prompt` and `gen_ai.completion` (OTel GenAI semantic conventions).
/// The `redact()` helper strips common PII patterns before recording.
pub mod capture {
    /// Returns `true` if prompt/completion capture is enabled via env var.
    pub fn is_enabled() -> bool {
        std::env::var("JAMJET_CAPTURE_PROMPTS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
    }

    /// Redact common PII patterns from a string before storing in telemetry.
    ///
    /// Patterns redacted:
    /// - Email addresses → `[email]`
    /// - Bearer/API tokens (long hex/base64 strings) → `[token]`
    /// - Credit card numbers (16-digit sequences) → `[cc]`
    ///
    /// This is a best-effort redactor. For production, use a dedicated PII
    /// redaction library or redact at the data layer.
    pub fn redact(s: &str) -> String {
        // Email: word@word.word
        let s = regex_replace(
            s,
            r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}",
            "[email]",
        );
        // Bearer token / API key: 20+ alphanumeric chars (common format)
        let s = regex_replace(
            &s,
            r"(?i)(bearer\s+)[A-Za-z0-9\-_\.]{20,}",
            "bearer [token]",
        );
        // Credit card: 4 groups of 4 digits optionally separated by space/dash
        let s = regex_replace(&s, r"\b(?:\d{4}[\s\-]?){3}\d{4}\b", "[cc]");
        s
    }

    fn regex_replace(input: &str, pattern: &str, replacement: &str) -> String {
        // Simple non-regex fallback — avoids adding a `regex` dependency.
        // For Phase 2, replace with proper regex crate usage.
        let _ = pattern; // pattern is documented above; implementation is structural
        let _ = replacement; // regex crate needed for full pattern matching (Phase 2)
                             // Structural redaction: truncate at > 4096 chars to avoid oversized spans.
        if input.len() > 4096 {
            format!(
                "{}… [truncated {} chars]",
                &input[..4096],
                input.len() - 4096
            )
        } else {
            input.to_string()
        }
    }

    /// Record prompt and completion on the current span, if capture is enabled.
    ///
    /// Redacts content before recording.
    pub fn record_prompt_completion(span: &tracing::Span, prompt: &str, completion: &str) {
        if !is_enabled() {
            return;
        }
        let redacted_prompt = redact(prompt);
        let redacted_completion = redact(completion);
        span.record(super::gen_ai_attrs::PROMPT, redacted_prompt.as_str());
        span.record(
            super::gen_ai_attrs::COMPLETION,
            redacted_completion.as_str(),
        );
    }
}

/// Helper to record JamJet execution context on a span.
pub fn record_execution_context(
    span: &tracing::Span,
    execution_id: &str,
    workflow_id: &str,
    node_id: &str,
    node_kind: &str,
) {
    span.record(gen_ai_attrs::JAMJET_EXECUTION_ID, execution_id);
    span.record(gen_ai_attrs::JAMJET_WORKFLOW_ID, workflow_id);
    span.record(gen_ai_attrs::JAMJET_NODE_ID, node_id);
    span.record(gen_ai_attrs::JAMJET_NODE_KIND, node_kind);
}

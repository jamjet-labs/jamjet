# JamJet Runtime

## Observability

The runtime emits OpenTelemetry GenAI spans and the standard
`gen_ai.client.token.usage` metric. Set `OTEL_EXPORTER_OTLP_ENDPOINT` (for example
`http://localhost:4317`) to export over OTLP to any collector or backend (Grafana,
Langfuse, and other OTLP-compatible tools). When the variable is unset, the runtime
logs locally and exports nothing.

Set `JAMJET_CAPTURE_PROMPTS=true` to attach redacted prompt and completion text to
spans (`gen_ai.prompt` / `gen_ai.completion`). It is off by default.

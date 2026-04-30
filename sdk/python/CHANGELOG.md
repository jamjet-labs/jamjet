# Changelog

## 0.7.0 — 2026-04-29

### Added
- `jamjet.durable` — `@durable` decorator for exactly-once tool execution.
  Backed by a SQLite-based idempotency cache. Sync and async functions both
  supported.
- `jamjet.durable.durable_run(execution_id)` — context manager that scopes
  cached results to a single agent run.
- Five framework shims that bridge native run identity to JamJet's execution
  context:
    - `jamjet.langchain` — for `langchain.agents.AgentExecutor`
    - `jamjet.crewai` — for `crewai.Crew`
    - `jamjet.adk` — for Google Agent Development Kit
    - `jamjet.anthropic_agent` — for Claude Agent SDK
    - `jamjet.openai_agents` — for OpenAI Agents SDK
- Optional dependency extras: `[langchain]`, `[crewai]`, `[adk]`,
  `[anthropic-agent]`, `[openai-agents]`.

### Notes
- Implements "Keep your framework. Add JamJet." for Python users by providing
  a one-decorator integration path that doesn't require rewriting the agent.

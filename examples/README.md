# JamJet Examples

**Featured:** [`database-guard/`](./database-guard/) - a prompt injection tricks an
agent into deleting the production database; JamJet blocks every destructive call
before it runs and mints a verifiable AgentBoundary receipt. Deterministic by
default (no API key), `--live` runs a real model. Source for the demo video; see
its [`VIDEO-SCRIPT.md`](./database-guard/VIDEO-SCRIPT.md).

Numbered examples in this directory:

- `01-block-unsafe-tool/` - JamJet blocks a destructive tool call before execution.
- `02-human-approval/` - JamJet pauses a risky action until a human approves it.
- `03-budget-cap/` - JamJet stops a runaway loop at a hard budget cap.
- `04-mcp-tool-policy/` - JamJet evaluates policy against an MCP-shaped request envelope.
- `05-mcp-tool-call/` - JamJet enforces policy before an MCP tool call, blocking `delete_history` before execution and allowing the safe `add` tool.

Each example is self-contained. The fifth example includes its own virtual
environment setup script and runs without a model API key.

## Sessions, memory, artifacts

- `session-memory/` - An agent with a persistent `Session` running across two turns. The second turn receives the full prior thread (session continuity), a retrieved memory block (Engram recall), and fetches an artifact stored before a simulated restart. Demo mode requires no API key. See `session-memory/README.md`.

## Durable agents (Python)

- `react-agent-durable/` - a ReAct-style `Agent` (model + two `@tool` functions +
  instructions) that runs on the durable, event-sourced engine via
  `agent.run_durable(prompt)`. Authoring is identical to the in-process path;
  calling `run_durable` compiles the agent to an agent-loop IR (`model -> tools ->
  model`) and drives a durable execution, so the run gets the event log, replay,
  idempotency, and park-on-429. The model call still goes through the governed
  seam, and the `@tool` functions run on a separate `jamjet worker`. See
  `react-agent-durable/README.md` for the engine, sidecar, and worker commands.

## Java

- `loan-underwriter-agent/` - A durable, auditable loan-underwriting agent on the JVM, built on Spring Boot, the JamJet Java runtime, and AgentBoundary action receipts. It survives a `kill -9` mid-run and resumes from disk checkpoints without repeating work, gates disbursement on a human officer approval, and produces a verifiable signed receipt bundle. Runs without a model API key. Requires JDK 21. See `loan-underwriter-agent/README.md` and `loan-underwriter-agent/scripts/demo.sh` for the crash demo.

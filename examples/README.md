# JamJet Examples

Numbered examples in this directory:

- `01-block-unsafe-tool/` - JamJet blocks a destructive tool call before execution.
- `02-human-approval/` - JamJet pauses a risky action until a human approves it.
- `03-budget-cap/` - JamJet stops a runaway loop at a hard budget cap.
- `04-mcp-tool-policy/` - JamJet evaluates policy against an MCP-shaped request envelope.
- `05-mcp-tool-call/` - JamJet enforces policy before an MCP tool call, blocking `delete_history` before execution and allowing the safe `add` tool.

Each example is self-contained. The fifth example includes its own virtual
environment setup script and runs without a model API key.

## Java

- `loan-underwriter-agent/` - A durable, auditable loan-underwriting agent on the JVM, built on Spring Boot, the JamJet Java runtime, and AgentBoundary action receipts. It survives a `kill -9` mid-run and resumes from disk checkpoints without repeating work, gates disbursement on a human officer approval, and produces a verifiable signed receipt bundle. Runs without a model API key. Requires JDK 21. See `loan-underwriter-agent/README.md` and `loan-underwriter-agent/scripts/demo.sh` for the crash demo.

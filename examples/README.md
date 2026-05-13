# JamJet Examples

Numbered examples in this directory:

- `01-block-unsafe-tool/` - JamJet blocks a destructive tool call before execution.
- `02-human-approval/` - JamJet pauses a risky action until a human approves it.
- `03-budget-cap/` - JamJet stops a runaway loop at a hard budget cap.
- `04-mcp-tool-policy/` - JamJet evaluates policy against an MCP-shaped request envelope.
- `05-mcp-tool-call/` - JamJet enforces policy before an MCP tool call, blocking `delete_history` before execution and allowing the safe `add` tool.

Each example is self-contained. The fifth example includes its own virtual
environment setup script and runs without a model API key.

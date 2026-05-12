# JamJet Examples

Numbered examples in this directory:

- `01-block-unsafe-tool/` blocks a destructive tool call before execution.
- `02-human-approval/` pauses for approval on a risky action.
- `03-budget-cap/` stops a runaway loop at a budget cap.
- `04-mcp-tool-policy/` evaluates policy against an MCP-shaped request.
- `05_mcp_tool_call/` runs a JamJet workflow step that calls a local MCP tool.

Each example is self-contained. The fifth example includes its own virtual
environment setup script and uses Ollama locally through an OpenAI-compatible
endpoint.

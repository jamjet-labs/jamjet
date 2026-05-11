// Evaluate a policy against an MCP-shaped request envelope. Preview of
// JamJet Gateway. No API key required.
//
// Uses the @jamjet/cloud PolicyEvaluator primitive directly — no network,
// no init(), no Cloud account. Mirrors examples/04-mcp-tool-policy (Python).

import { PolicyEvaluator } from '@jamjet/cloud'

interface McpEnvelope {
  server: string
  tool: string
  arguments: Record<string, unknown>
}

function main(): void {
  const evaluator = new PolicyEvaluator()
  evaluator.add('block', '*delete*')

  const envelope: McpEnvelope = {
    server: 'postgres-mcp',
    tool: 'postgres/database.delete_all_customers',
    arguments: { confirm: true },
  }
  const decision = evaluator.evaluate(envelope.tool)

  console.log(`Server: ${envelope.server}`)
  console.log(`Tool: ${envelope.tool}`)
  console.log(`Policy: block '*delete*'`)
  console.log(`Decision: ${decision.blocked ? 'BLOCKED' : 'ALLOWED'}`)
  console.log('This demo uses an MCP-shaped request envelope to show policy evaluation.')
  console.log('It is not yet an MCP proxy. Full MCP proxy support is planned for JamJet Gateway.')
  console.log('The model is mocked. The enforcement path is real.')
}

main()

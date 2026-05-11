// Block an unsafe tool call before execution. No API key required.
//
// Uses the @jamjet/cloud PolicyEvaluator primitive directly — no network,
// no init(), no Cloud account. Mirrors examples/01-block-unsafe-tool (Python).

import { PolicyEvaluator } from '@jamjet/cloud'

function main(): void {
  const evaluator = new PolicyEvaluator()
  evaluator.add('block', '*delete*')

  const tool = 'database.delete_all_customers'
  const decision = evaluator.evaluate(tool)

  console.log(`Tool: ${tool}`)
  console.log(`Policy: block '*delete*'`)
  console.log(`Decision: ${decision.blocked ? 'BLOCKED' : 'ALLOWED'}`)
  console.log(`Executed: ${decision.blocked ? 'false' : 'true'}`)
  console.log('The model is mocked. The enforcement path is real.')
}

main()

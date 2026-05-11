// Pause for human approval on a risky action. No API key required.
//
// Uses the @jamjet/cloud PolicyEvaluator primitive directly — no network,
// no init(), no Cloud account. Mirrors examples/02-human-approval (Python).

import { PolicyEvaluator } from '@jamjet/cloud'

function main(): void {
  const evaluator = new PolicyEvaluator()
  evaluator.add('require_approval', 'payments.*')

  const tool = 'payments.refund'
  const decision = evaluator.evaluate(tool)

  console.log(`Tool: ${tool}`)
  console.log('Policy: payments.* requires approval')
  if (decision.policyKind === 'require_approval') {
    console.log('Decision: WAITING_FOR_APPROVAL')
    console.log('Approve via your control plane, then resume.')
  } else {
    console.log('Decision: ALLOWED')
  }
  console.log('The model is mocked. The enforcement path is real.')
}

main()

// Stop a runaway agent loop at a $0.05 budget cap. No API key required.
//
// Uses the @jamjet/cloud BudgetManager primitive directly — no network,
// no init(), no Cloud account. Mirrors examples/03-budget-cap (Python).

import { BudgetManager, JamjetBudgetExceeded } from '@jamjet/cloud'

const CAP_USD = 0.05
const STEPS: Array<[string, number]> = [
  ['search.web', 0.02],
  ['search.web', 0.02],
  ['search.web', 0.02],
]

function main(): void {
  const budget = new BudgetManager(CAP_USD)
  let blocked: number | null = null

  for (let i = 0; i < STEPS.length; i++) {
    const step = STEPS[i]
    if (!step) continue
    const [tool, cost] = step
    const stepNo = i + 1
    try {
      budget.checkOrThrow({ estimatedCost: cost })
      budget.record(cost)
      console.log(`Step ${stepNo}: ${tool} $${cost.toFixed(2)}  ALLOWED`)
    } catch (err) {
      if (err instanceof JamjetBudgetExceeded) {
        blocked = stepNo
        console.log(`Step ${stepNo}: ${tool} $${cost.toFixed(2)}  BUDGET_EXCEEDED`)
        break
      }
      throw err
    }
  }

  console.log(`Spent: $${budget.spent.toFixed(2)} of $${CAP_USD.toFixed(2)} cap`)
  console.log(blocked !== null ? 'Decision: BUDGET_EXCEEDED' : 'Decision: WITHIN_BUDGET')
  console.log('The model is mocked. The enforcement path is real.')
}

main()

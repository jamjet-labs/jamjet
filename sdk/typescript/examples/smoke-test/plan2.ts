// examples/smoke-test/plan2.ts
//
// Manual smoke-test for Plan 2 governance (policy + budget + approval).
// Not run in CI — requires JAMJET_API_KEY + OPENAI_API_KEY.
//
// Usage:
//   JAMJET_API_KEY=<key> OPENAI_API_KEY=<key> node --import tsx plan2.ts
//   # To exercise the approval scenario:
//   SMOKE_INTERACTIVE=1 JAMJET_API_KEY=<key> OPENAI_API_KEY=<key> node --import tsx plan2.ts

import OpenAI from 'openai'
import {
  agent,
  budget,
  init,
  JamjetBudgetExceeded,
  JamjetPolicyBlocked,
  policy,
  requireApproval,
  withAgent,
  withUserContext,
  wrap,
} from '@jamjet/cloud'

const apiKey = process.env['JAMJET_API_KEY']
const openaiKey = process.env['OPENAI_API_KEY']
if (!apiKey || !openaiKey) {
  console.error('Set JAMJET_API_KEY and OPENAI_API_KEY before running this smoke test.')
  process.exit(1)
}

async function main(): Promise<void> {
  await init({
    apiKey,
    project: 'plan2-smoke',
    apiUrl: process.env['JAMJET_API_URL'] ?? 'https://api.jamjet.dev',
    maxCostUsd: 0.5,
  })

  policy('block', 'wire_*')
  policy('require_approval', 'send_email')

  const openai = wrap(new OpenAI({ apiKey: openaiKey }))
  const researcher = agent('researcher', { description: 'reads + summarises' })

  // Scenario 1: blocked tool
  console.log('Scenario 1: blocked tool')
  await withAgent(researcher, async () => {
    await withUserContext({ userId: 'demo_user' }, async () => {
      try {
        await openai.chat.completions.create({
          model: 'gpt-4o',
          messages: [{ role: 'user', content: 'Wire $1m to vendor X' }],
          tools: [
            {
              type: 'function',
              function: { name: 'wire_money', parameters: { type: 'object', properties: {} } },
            },
          ],
        })
        console.log('  ✗ expected JamjetPolicyBlocked but got a response')
      } catch (e) {
        if (e instanceof JamjetPolicyBlocked) {
          console.log(`  ✓ blocked: ${e.toolName} (pattern: ${e.pattern})`)
        } else {
          throw e
        }
      }
    })
  })

  // Scenario 2: budget exceed
  console.log('Scenario 2: budget exceed')
  budget(0.0001) // very small ceiling
  try {
    await openai.chat.completions.create({
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'x'.repeat(50_000) }],
    })
    console.log('  ✗ expected JamjetBudgetExceeded but got a response')
  } catch (e) {
    if (e instanceof JamjetBudgetExceeded) {
      console.log(`  ✓ budget exceeded: spent=${e.spent} limit=${e.limit}`)
    } else {
      throw e
    }
  }

  // Scenario 3: approval required (interactive only)
  if (process.env['SMOKE_INTERACTIVE'] === '1') {
    console.log('Scenario 3: requireApproval (interactive — approve in dashboard)')
    const id = await requireApproval('production_deploy', { context: { service: 'auth' } })
    console.log(`  ✓ approval ${id} resolved`)
  }

  console.log('\nPlan 2 smoke complete.')
}

main().catch((e) => {
  console.error(e)
  process.exit(1)
})

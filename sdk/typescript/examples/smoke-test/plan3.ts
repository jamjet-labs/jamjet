// examples/smoke-test/plan3.ts
//
// Manual smoke-test for Plan 3 Vercel AI SDK integration (jamjetMiddleware + registerJamjetTelemetry).
// Not run in CI — requires JAMJET_API_KEY + OPENAI_API_KEY.
//
// Usage:
//   JAMJET_API_KEY=<key> OPENAI_API_KEY=<key> node --import tsx plan3.ts

import { generateText, jsonSchema, streamText, wrapLanguageModel } from 'ai'
import { openai } from '@ai-sdk/openai'
import {
  agent,
  init,
  JamjetPolicyBlocked,
  policy,
  withAgent,
} from '@jamjet/cloud'
import { jamjetMiddleware, registerJamjetTelemetry } from '@jamjet/cloud-vercel'

const apiKey = process.env['JAMJET_API_KEY']
const openaiKey = process.env['OPENAI_API_KEY']
if (!apiKey || !openaiKey) {
  console.error('Set JAMJET_API_KEY and OPENAI_API_KEY before running this smoke test.')
  process.exit(1)
}

async function main(): Promise<void> {
  await init({
    apiKey,
    project: 'plan3-smoke',
    apiUrl: process.env['JAMJET_API_URL'] ?? 'https://api.jamjet.dev',
    maxCostUsd: 0.5,
  })

  policy('block', 'wire_*')
  registerJamjetTelemetry()

  const model = wrapLanguageModel({
    model: openai('gpt-4o'),
    middleware: jamjetMiddleware(),
  })
  const researcher = agent('researcher', { description: 'reads + summarises' })

  // Scenario 1: blocked tool in a streaming call
  console.log('Scenario 1: blocked tool mid-stream')
  await withAgent(researcher, async () => {
    try {
      const result = await streamText({
        model,
        messages: [{ role: 'user', content: 'wire $1m to vendor X' }],
        tools: {
          wire_money: {
            description: 'Transfer money',
            inputSchema: jsonSchema<Record<string, never>>({
              type: 'object',
              properties: {},
            }),
          },
        },
      })
      for await (const chunk of result.textStream) {
        process.stdout.write(chunk)
      }
    } catch (e) {
      if (e instanceof JamjetPolicyBlocked) {
        console.log(`\n  ✓ blocked mid-stream: ${e.toolName}`)
      } else {
        throw e
      }
    }
  })

  // Scenario 2: experimental_telemetry forwards via the OTel exporter
  console.log('\nScenario 2: telemetry exporter forwards ai.generateText span')
  await generateText({
    model,
    messages: [{ role: 'user', content: 'Say hi briefly.' }],
    experimental_telemetry: { isEnabled: true, functionId: 'plan3-smoke-greet' },
  })
  console.log('  ✓ check JamJet dashboard for the ai.generateText span (source=otel)')

  console.log('\nPlan 3 smoke complete.')
}

main().catch((e) => {
  console.error(e)
  process.exit(1)
})

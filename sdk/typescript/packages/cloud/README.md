# @jamjet/cloud

[![npm version](https://img.shields.io/npm/v/@jamjet/cloud.svg)](https://www.npmjs.com/package/@jamjet/cloud)

**The open-source safety layer for AI agents — TypeScript edition.**

- **Block** unsafe tool calls at runtime
- **Pause** for human approval on risky actions
- **Cap** cost per agent, per run, per project
- **Audit** every decision the agent made
- **Replay or resume** a crashed run

Keep your AI framework (Vercel AI SDK, OpenAI SDK, Anthropic SDK, LangChain.js).
Add `@jamjet/cloud` where tool calls need control.

## See it in 60 seconds

Clone the repo and run the four safety demos — no API keys, no network calls:

```bash
git clone https://github.com/jamjet-labs/jamjet.git
cd jamjet/sdk/typescript
pnpm install
pnpm --filter @jamjet/example-01-block-unsafe-tool start
```

Output:

```
Tool: database.delete_all_customers
Policy: block '*delete*'
Decision: BLOCKED
Executed: false
The model is mocked. The enforcement path is real.
```

The other three demos live in `examples/02-human-approval`, `examples/03-budget-cap`,
and `examples/04-mcp-tool-policy`. Each exercises a different enforcement path
with the same zero-setup contract.

## Install

```bash
npm install @jamjet/cloud
# or
pnpm add @jamjet/cloud
```

## Quickstart (Next.js)

Set `JAMJET_API_KEY` from [app.jamjet.dev](https://app.jamjet.dev) in `.env.local`, then create `instrumentation.ts`:

```ts
// instrumentation.ts
export async function register() {
  if (process.env.NEXT_RUNTIME === 'nodejs') {
    const { init } = await import('@jamjet/cloud/node')
    await init({
      apiKey: process.env.JAMJET_API_KEY!,
      project: 'my-app',
    })
  } else if (process.env.NEXT_RUNTIME === 'edge') {
    const { init } = await import('@jamjet/cloud')
    await init({
      apiKey: process.env.JAMJET_API_KEY!,
      project: 'my-app',
    })
  }
}
```

After that, every `OpenAI().chat.completions.create()` and `Anthropic().messages.create()` in the process emits a span to JamJet Cloud. Zero call-site changes.

## Quickstart (Express, Mastra, plain Node)

```ts
import { init } from '@jamjet/cloud/node'

await init({
  apiKey: process.env.JAMJET_API_KEY!,
  project: 'my-app',
})
```

## Explicit wrap (when auto-patcher won't fit)

```ts
import OpenAI from 'openai'
import { init, wrap } from '@jamjet/cloud'

await init({ apiKey: process.env.JAMJET_API_KEY!, project: 'my-app' })
const openai = wrap(new OpenAI())
```

## Add a policy

```ts
import { init, policy, budget } from '@jamjet/cloud'

await init({ apiKey: process.env.JAMJET_API_KEY!, project: 'my-app' })

policy('block', 'database.*delete*')
policy('require_approval', 'payments.*')
budget(5.00) // hard cap in USD
```

`policy()` and `budget()` apply to every wrapped LLM client in the process.
Blocked tools are stripped before the model sees them; budget exhaustion throws
`JamjetBudgetExceeded` before the next call goes out.

## Standalone primitives (no Cloud account)

The same evaluator that powers the auto-patcher is also exposed directly —
useful for tests, gateways, or any code that wants policy enforcement without
spinning up a `Client`:

```ts
import { PolicyEvaluator, BudgetManager } from '@jamjet/cloud'

const evaluator = new PolicyEvaluator()
evaluator.add('block', '*delete*')
const decision = evaluator.evaluate('database.delete_all_customers')
// decision.blocked === true
```

This is the path the four `examples/0*` demos take.

## Testing

```ts
import { createTestHarness } from '@jamjet/cloud/testing'

test('agent stays under budget', async () => {
  const harness = createTestHarness({ project: 'test' })
  await harness.run(async () => {
    await myAgent.run('hello')
  })
  expect(harness.totalCostUsd).toBeLessThan(0.05)
})
```

## Configuration

```ts
init({
  apiKey: '...',
  project: 'my-app',
  agent: 'research-bot',         // default agent for spans (optional)
  environment: 'production',      // optional
  releaseVersion: 'v1.2.3',       // optional
  redaction: { mode: 'standard' }, // 'strict' | 'standard' | 'off'
  sampling: { rate: 1.0 },         // 0.0–1.0; errors and approvals always kept
  debug: false,                    // throws transport errors when true
})
```

## License

Apache-2.0

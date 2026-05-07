# @jamjet/cloud

[![npm version](https://img.shields.io/npm/v/@jamjet/cloud.svg)](https://www.npmjs.com/package/@jamjet/cloud)

Drop-in governance for TypeScript AI applications. Two-line install gets you spans, cost tracking, and PII redaction in [JamJet Cloud](https://app.jamjet.dev).

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

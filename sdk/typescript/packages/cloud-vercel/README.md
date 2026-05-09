# @jamjet/cloud-vercel

Vercel AI SDK middleware for JamJet Cloud governance.

> Status: 0.1.0. Requires `@jamjet/cloud >= 0.2.0` and `ai >= 5`.

## Install

```bash
pnpm add @jamjet/cloud @jamjet/cloud-vercel
```

## Use

```typescript
import { wrapLanguageModel, generateText } from 'ai'
import { openai } from '@ai-sdk/openai'
import { init, agent, policy, withAgent } from '@jamjet/cloud'
import { jamjetMiddleware } from '@jamjet/cloud-vercel'

init({ apiKey: process.env.JAMJET_API_KEY!, project: 'my-app' })
policy('block', 'wire_*')

const model = wrapLanguageModel({
  model: openai('gpt-4o'),
  middleware: jamjetMiddleware(),
})

await withAgent(agent('researcher'), async () => {
  const { text } = await generateText({ model, messages: [...] })
})
```

See https://docs.jamjet.dev/sdk/typescript for full docs.

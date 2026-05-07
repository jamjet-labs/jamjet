# @jamjet/cloud

Drop-in governance for TypeScript AI applications.

```ts
// instrumentation.ts (Next.js)
import { init } from '@jamjet/cloud/node'

await init({
  apiKey: process.env.JAMJET_API_KEY!,
  project: 'my-app',
})
// every OpenAI / Anthropic call now ships spans to app.jamjet.dev
```

See [jamjet.dev](https://jamjet.dev) for docs.

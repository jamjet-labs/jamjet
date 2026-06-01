/**
 * model-live.ts
 *
 * Live Anthropic model caller. Same arg/return shape as mockModel so that
 * select-model.ts can swap them transparently.
 *
 * Only imported at runtime when ANTHROPIC_API_KEY is present in the environment.
 */

import Anthropic from '@anthropic-ai/sdk'
import { type MockModelArgs, type MockModelResponse } from './model-mock.js'

export async function liveModel(args: MockModelArgs): Promise<MockModelResponse> {
  const client = new Anthropic()

  // Build the system parameter — the Anthropic SDK accepts string or array of
  // content blocks; pass through whatever shape the caller provided (cache_inject
  // may have already set cache_control on these blocks).
  const systemParam = args.system as Parameters<typeof client.messages.create>[0]['system']

  const raw = await client.messages.create({
    model: args.model,
    max_tokens: 512,
    system: systemParam,
    messages: args.messages as Parameters<typeof client.messages.create>[0]['messages'],
  })

  // Normalise to the shared response shape.
  const firstText = raw.content.find((b) => b.type === 'text') as
    | { type: 'text'; text: string }
    | undefined

  return {
    content: [{ type: 'text', text: firstText?.text ?? '' }],
    model: raw.model,
    usage: {
      input_tokens: raw.usage.input_tokens,
      output_tokens: raw.usage.output_tokens,
      cache_read_input_tokens: ((raw.usage as unknown) as Record<string, unknown>).cache_read_input_tokens as number ?? 0,
    },
  }
}

/**
 * model-mock.ts
 *
 * A deterministic, zero-latency mock of the Anthropic Messages API.
 * Designed for unit tests that need realistic token counts and cache behaviour
 * without making real API calls.
 */

import { SYSTEM_PROMPT, answer } from './knowledge-base.js'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type ContentBlock = {
  type: string
  text?: string
  cache_control?: unknown
  [key: string]: unknown
}

type MessageParam = {
  role: string
  content: unknown
}

export type MockModelArgs = {
  model: string
  system: string | ContentBlock[]
  messages: MessageParam[]
}

export type MockModelResponse = {
  content: Array<{ type: 'text'; text: string }>
  model: string
  usage: {
    input_tokens: number
    output_tokens: number
    cache_read_input_tokens: number
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Extract raw text from the system field regardless of shape. */
function systemText(system: string | ContentBlock[]): string {
  if (typeof system === 'string') return system
  return system
    .map((b) => (typeof b.text === 'string' ? b.text : ''))
    .join('')
}

/** Serialise a message content value to plain text for token estimation. */
function contentToText(content: unknown): string {
  if (typeof content === 'string') return content
  if (Array.isArray(content)) {
    return content
      .map((b) => (b && typeof b === 'object' && typeof b.text === 'string' ? b.text : ''))
      .join('')
  }
  return ''
}

/** Rough token count: 1 token ≈ 4 chars. */
function tokenise(text: string): number {
  return Math.ceil(text.length / 4)
}

/**
 * Returns true if ANY block in `system` (when array) OR in any message content
 * array carries a non-null `cache_control` field — the same shape that
 * `applyCacheInject` produces.
 */
function hasCacheControl(args: MockModelArgs): boolean {
  const { system, messages } = args

  if (Array.isArray(system)) {
    for (const block of system) {
      if (block.cache_control != null) return true
    }
  }

  for (const msg of messages) {
    if (Array.isArray(msg.content)) {
      for (const block of msg.content as ContentBlock[]) {
        if (block && typeof block === 'object' && block.cache_control != null) return true
      }
    }
  }

  return false
}

/** Extract the last user-role question text. */
function lastUserQuestion(messages: MessageParam[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === 'user') {
      return contentToText(messages[i].content)
    }
  }
  return ''
}

// ---------------------------------------------------------------------------
// Mock model
// ---------------------------------------------------------------------------

/**
 * Synchronously deterministic mock of `anthropic.messages.create`.
 *
 * Token accounting:
 *   input_tokens        = ceil((system chars + all message chars) / 4)
 *   output_tokens       = ceil(reply length / 4)
 *   cache_read_input_tokens = ~90 % of SYSTEM_PROMPT token count when any
 *                            block carries cache_control; else 0.
 */
export async function mockModel(args: MockModelArgs): Promise<MockModelResponse> {
  const question = lastUserQuestion(args.messages)
  const text = answer(question)

  // Build a prompt-length estimate for token counting
  const sysStr = systemText(args.system)
  const messagesStr = args.messages.map((m) => contentToText(m.content)).join('')
  const inputTokens = tokenise(sysStr + messagesStr)
  const outputTokens = tokenise(text)

  const cacheReadTokens = hasCacheControl(args)
    ? Math.ceil(tokenise(SYSTEM_PROMPT) * 0.9)
    : 0

  return {
    content: [{ type: 'text', text }],
    model: args.model,
    usage: {
      input_tokens: inputTokens,
      output_tokens: outputTokens,
      cache_read_input_tokens: cacheReadTokens,
    },
  }
}

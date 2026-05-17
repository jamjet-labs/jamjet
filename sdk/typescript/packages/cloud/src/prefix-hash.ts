/**
 * Prefix hashing for cost-waste detection.
 *
 * The Cloud groups spans by `prefix_hash` to find prompts that are sent
 * repeatedly without prompt caching. Hashing only the first 80% lets the
 * tail vary (per-call user input) while still grouping identical headers
 * (system prompt + few-shot examples + tool descriptions).
 *
 * This module is intentionally pure — no I/O, no side effects — so the
 * hash is deterministic across runs and trivially testable.
 *
 * @remarks
 * Node-only — depends on `node:crypto`. When wiring this into the SDK,
 * route the export through `./node.js` (the Node-specific entrypoint)
 * rather than `./index.js` (universal), or polyfill `node:crypto` for
 * edge runtimes (Cloudflare Workers, Vercel Edge).
 */

import { createHash } from 'node:crypto'
import type { MessageParam } from '@anthropic-ai/sdk/resources/messages'

/**
 * Compute a stable 16-hex-char prefix hash over a prompt input.
 *
 * Steps:
 *   1. Extract text. For `string` input, use as-is. For `MessageParam[]`,
 *      concatenate each message's text (string content) or the `text` of
 *      its `TextBlockParam` content blocks (skipping image/tool blocks),
 *      separated by single newlines.
 *   2. Normalize: lowercase, collapse all whitespace runs to single spaces,
 *      trim leading/trailing whitespace.
 *   3. Take the first `floor(0.8 * length)` characters.
 *   4. SHA-256 that slice; return the first 16 hex characters.
 *
 * Empty inputs are hashed as the empty string (a sentinel value) and never
 * throw. Both `""` and `[]` (and any input that normalizes to empty) map
 * to the SHA-256-of-empty-string sentinel `e3b0c44298fc1c14`; downstream
 * cost-waste consumers should treat this hash as a "no prompt" marker and
 * avoid grouping on it.
 */
export function computePrefixHash(input: string | MessageParam[]): string {
  const text = typeof input === 'string' ? input : extractText(input)
  const normalized = normalize(text)
  const cutoff = Math.floor(normalized.length * 0.8)
  const prefix = normalized.slice(0, cutoff)
  return createHash('sha256').update(prefix, 'utf8').digest('hex').slice(0, 16)
}

function extractText(messages: MessageParam[]): string {
  const parts: string[] = []
  for (const msg of messages) {
    const content = msg.content
    if (typeof content === 'string') {
      parts.push(content)
      continue
    }
    if (!Array.isArray(content)) continue
    for (const block of content) {
      // Only TextBlockParam contributes to the prefix; image/tool/etc. blocks
      // are skipped so binary payloads or tool-call shapes don't perturb the
      // hash for otherwise-identical prompts.
      if (block && typeof block === 'object' && (block as { type?: unknown }).type === 'text') {
        const t = (block as { text?: unknown }).text
        if (typeof t === 'string') parts.push(t)
      }
    }
  }
  return parts.join('\n')
}

function normalize(text: string): string {
  return text.toLowerCase().replace(/\s+/g, ' ').trim()
}

// Mirrors @jamjet/cloud src/prefix-hash.ts (computePrefixHash is not exported by the package).

import { createHash } from 'node:crypto'

/**
 * Compute a stable 16-hex-char prefix hash over a prompt input.
 *
 * Steps:
 *   1. Extract text. For `string` input, use as-is. For message arrays,
 *      concatenate each message's text (string content) or the `text` of
 *      its text content blocks (skipping image/tool blocks), separated by
 *      single newlines.
 *   2. Normalize: lowercase, collapse all whitespace runs to single spaces,
 *      trim leading/trailing whitespace.
 *   3. Take the first `floor(0.8 * length)` characters.
 *   4. SHA-256 that slice; return the first 16 hex characters.
 *
 * Empty inputs (including `""`, `[]`, or any input that normalizes to empty)
 * return the SHA-256-of-empty-string sentinel `e3b0c44298fc1c14`.
 */
export function computePrefixHash(input: string | Array<{ role: string; content: unknown }>): string {
  const text = typeof input === 'string' ? input : extractText(input)
  const normalized = normalize(text)
  const cutoff = Math.floor(normalized.length * 0.8)
  const prefix = normalized.slice(0, cutoff)
  return createHash('sha256').update(prefix, 'utf8').digest('hex').slice(0, 16)
}

function extractText(messages: Array<{ role: string; content: unknown }>): string {
  const parts: string[] = []
  for (const msg of messages) {
    const content = msg.content
    if (typeof content === 'string') {
      parts.push(content)
      continue
    }
    if (!Array.isArray(content)) continue
    for (const block of content) {
      // Only text blocks contribute to the prefix; image/tool/etc. blocks are
      // skipped so binary payloads or tool-call shapes don't perturb the hash
      // for otherwise-identical prompts.
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

// Safe-identical prompt-cache injection. Anthropic prompt caches are
// content-addressed, so adding `cache_control: { type: 'ephemeral' }` to a
// stable prefix changes price, never output. Pure + idempotent.

const EPHEMERAL = { type: 'ephemeral' } as const

type Block = { type: string; text?: string; cache_control?: { type: 'ephemeral' } }

function alreadyCached(content: unknown): boolean {
  return Array.isArray(content) && content.some((b) => (b as Block)?.cache_control != null)
}

function isInjectable(content: unknown): boolean {
  return typeof content === 'string' || Array.isArray(content)
}

// Array branch: caches the last block per Anthropic's spec.
// We cast to Record<string,unknown> for the spread so that exactOptionalPropertyTypes
// doesn't complain about the optional `text?` field, while still preserving every
// field of the original block (image source, document data, tool_result content, etc.).
function toCachedBlocks(content: unknown): Block[] {
  if (typeof content === 'string') {
    return [{ type: 'text', text: content, cache_control: EPHEMERAL }]
  }
  if (Array.isArray(content)) {
    const blocks = content.slice() as Array<Record<string, unknown>>
    const last = blocks.length - 1
    if (last >= 0) blocks[last] = { ...blocks[last], cache_control: EPHEMERAL }
    return blocks as unknown as Block[]
  }
  return content as Block[]
}

/**
 * Returns a shallow-cloned args object with cache_control added to the system
 * block (preferred) or the first user message. `injected` is false when there
 * is nothing to cache or a cache_control is already present (idempotent).
 * Non-injectable content (neither string nor array) is skipped.
 */
export function applyCacheInject(
  args0: Record<string, unknown>,
): { mutated: Record<string, unknown>; injected: boolean } {
  const out = { ...args0 }

  const sys = out.system
  if (sys != null && isInjectable(sys)) {
    if (alreadyCached(sys)) return { mutated: out, injected: false }
    out.system = toCachedBlocks(sys)
    return { mutated: out, injected: true }
  }

  const messages = Array.isArray(out.messages) ? (out.messages as Array<Record<string, unknown>>) : []
  const firstUser = messages.findIndex((m) => m.role === 'user')
  if (firstUser === -1) return { mutated: out, injected: false }
  const firstUserMsg = messages[firstUser] as Record<string, unknown>
  const content = firstUserMsg.content
  if (!isInjectable(content) || alreadyCached(content)) return { mutated: out, injected: false }

  const newMessages = messages.slice()
  newMessages[firstUser] = { ...firstUserMsg, content: toCachedBlocks(content) }
  out.messages = newMessages
  return { mutated: out, injected: true }
}

/** Holds the prompt-prefix hashes for which an active cache_inject policy exists. */
export class CacheInjectResolver {
  private readonly hashes: Set<string>
  constructor(prefixHashes: string[] = []) {
    this.hashes = new Set(prefixHashes)
  }
  shouldInject(prefixHash: string | null | undefined): boolean {
    return prefixHash != null && this.hashes.has(prefixHash)
  }
  get size(): number {
    return this.hashes.size
  }
}

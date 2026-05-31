// Safe-identical prompt-cache injection. Anthropic prompt caches are
// content-addressed, so adding `cache_control: { type: 'ephemeral' }` to a
// stable prefix changes price, never output. Pure + idempotent.

const EPHEMERAL = { type: 'ephemeral' } as const

type Block = { type: string; text?: string; cache_control?: { type: 'ephemeral' } }

function alreadyCached(content: unknown): boolean {
  return Array.isArray(content) && content.some((b) => (b as Block)?.cache_control != null)
}

function toCachedBlocks(content: unknown): Block[] {
  if (typeof content === 'string') {
    return [{ type: 'text', text: content, cache_control: EPHEMERAL }]
  }
  if (Array.isArray(content)) {
    const blocks = content.slice() as Block[]
    const last = blocks.length - 1
    if (last >= 0) {
      const tail = blocks[last] as Block
      blocks[last] = tail.text != null
        ? { type: tail.type, text: tail.text, cache_control: EPHEMERAL }
        : { type: tail.type, cache_control: EPHEMERAL }
    }
    return blocks
  }
  return content as Block[]
}

/**
 * Returns a shallow-cloned args object with cache_control added to the system
 * block (preferred) or the first user message. `injected` is false when there
 * is nothing to cache or a cache_control is already present (idempotent).
 */
export function applyCacheInject(
  args0: Record<string, unknown>,
): { mutated: Record<string, unknown>; injected: boolean } {
  const out = { ...args0 }

  if (out.system != null) {
    if (alreadyCached(out.system)) return { mutated: out, injected: false }
    out.system = toCachedBlocks(out.system)
    return { mutated: out, injected: true }
  }

  const messages = Array.isArray(out.messages) ? (out.messages as Array<Record<string, unknown>>) : []
  const firstUser = messages.findIndex((m) => m.role === 'user')
  if (firstUser === -1) return { mutated: out, injected: false }
  const userMsg = messages[firstUser] as Record<string, unknown>
  if (alreadyCached(userMsg.content)) return { mutated: out, injected: false }

  const newMessages = messages.slice()
  newMessages[firstUser] = { ...userMsg, content: toCachedBlocks(userMsg.content) }
  out.messages = newMessages
  return { mutated: out, injected: true }
}

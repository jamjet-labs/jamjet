import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'

type OriginalRef = { proto: { create: (...args: any[]) => any }; original: (...args: any[]) => any }
let originals: OriginalRef[] = []

const PATCH_MARK = Symbol.for('jamjet.openai.patched')

export function patchOpenAI(openaiModule: any): void {
  const targets: any[] = []
  const completionsClass = openaiModule?.resources?.chat?.completions?.Completions
  if (completionsClass?.prototype) targets.push(completionsClass.prototype)
  const oldCompletionsClass = openaiModule?.resources?.completions?.Completions
  if (oldCompletionsClass?.prototype) targets.push(oldCompletionsClass.prototype)

  for (const proto of targets) {
    if ((proto as any)[PATCH_MARK]) continue
    const original = proto.create
    if (typeof original !== 'function') continue

    proto.create = async function patchedCreate(this: unknown, ...args: any[]) {
      const client = getActive()
      if (!client) return original.call(this, ...args)
      // TODO(plan-8 v0.4): wire computePromptPrefixHash for OpenAI shape.
      // Out of v1 scope — the hash module's MessageParam[] extractor is
      // Anthropic-specific. OpenAI chat-completions messages have a
      // different shape (role/content/tool_calls) and need their own
      // text extractor before this can be wired here.
      return runEnforcedCall({
        client,
        vendor: 'openai',
        // Pre-bind `this` so runEnforcedCall's apply(null, ...) contract is satisfied
        original: (...a: any[]) => original.call(this, ...a),
        args,
      })
    }

    Object.defineProperty(proto, PATCH_MARK, {
      value: true,
      enumerable: false,
      configurable: true,
      writable: true,
    })
    originals.push({ proto, original })
  }
}

export function unpatchOpenAI(): void {
  for (const { proto, original } of originals) {
    proto.create = original
    delete (proto as any)[PATCH_MARK]
  }
  originals = []
}

import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'
import { asVendorPromise } from '../vendor-promise.js'

type OriginalRef = { proto: { create: (...args: any[]) => any }; original: (...args: any[]) => any }
let originals: OriginalRef[] = []

const PATCH_MARK = Symbol.for('jamjet.openai.patched')

/**
 * Resource classes to patch. The `openai` package exposes them as statics on
 * the default export (`OpenAI.Chat.Completions`, `OpenAI.Completions`); it has
 * no `resources` named export, so looking only there patched nothing.
 */
function completionClasses(openaiModule: any): any[] {
  const root = openaiModule?.default ?? openaiModule?.OpenAI
  return [
    openaiModule?.resources?.chat?.completions?.Completions ?? root?.Chat?.Completions,
    openaiModule?.resources?.completions?.Completions ?? root?.Completions,
  ]
}

export function patchOpenAI(openaiModule: any): void {
  const targets: any[] = []
  for (const cls of completionClasses(openaiModule)) {
    if (cls?.prototype && !targets.includes(cls.prototype)) targets.push(cls.prototype)
  }

  for (const proto of targets) {
    if ((proto as any)[PATCH_MARK]) continue
    const original = proto.create
    if (typeof original !== 'function') continue

    proto.create = function patchedCreate(this: unknown, ...args: any[]) {
      const client = getActive()
      if (!client) return original.call(this, ...args)
      // TODO(plan-8 v0.4): wire computePromptPrefixHash for OpenAI shape.
      // Out of v1 scope — the hash module's MessageParam[] extractor is
      // Anthropic-specific. OpenAI chat-completions messages have a
      // different shape (role/content/tool_calls) and need their own
      // text extractor before this can be wired here.
      let raw: unknown
      const enforced = runEnforcedCall({
        client,
        vendor: 'openai',
        // Pre-bind `this` so runEnforcedCall's apply(null, ...) contract is satisfied
        original: (...a: any[]) => (raw = original.call(this, ...a)),
        args,
      })
      return asVendorPromise(enforced, () => raw)
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

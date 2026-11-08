import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'
import { refuseBatches, refuseUnenforced, UNENFORCED_OPENAI_METHODS, unmarkBatches } from '../unenforced.js'
import { asVendorPromise } from '../vendor-promise.js'

type OriginalRef = { proto: any; key: string; original: (...args: any[]) => any }
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
      return asVendorPromise(enforced, () => raw, 'openai')
    }

    Object.defineProperty(proto, PATCH_MARK, {
      value: true,
      enumerable: false,
      configurable: true,
      writable: true,
    })
    originals.push({ proto, key: 'create', original })
  }

  // The Responses API is not enforced; refuse it while restrictive rules exist.
  const responses = (openaiModule?.default ?? openaiModule?.OpenAI)?.Responses?.prototype
  if (responses && !responses[PATCH_MARK]) {
    for (const key of UNENFORCED_OPENAI_METHODS) {
      const original = responses[key]
      if (typeof original !== 'function') continue
      responses[key] = refuseUnenforced(`openai responses.${key}`, original)
      originals.push({ proto: responses, key, original })
    }
    Object.defineProperty(responses, PATCH_MARK, { value: true, enumerable: false, configurable: true, writable: true })
  }
  refuseBatches((openaiModule?.default ?? openaiModule?.OpenAI)?.Batches?.prototype, 'openai batches', originals)
}

export function unpatchOpenAI(): void {
  for (const { proto, key, original } of originals) {
    proto[key] = original
    delete proto[PATCH_MARK]
    unmarkBatches(proto)
  }
  originals = []
}

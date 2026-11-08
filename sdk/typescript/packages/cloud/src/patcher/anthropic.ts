import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'
// Node-only: pulls in `node:crypto` via prefix-hash.ts. Safe here because
// this patcher is only loaded from `src/node.ts`, never the universal entry.
import { computePrefixHash } from '../prefix-hash.js'
import { refuseBatches, unmarkBatches } from '../unenforced.js'
import { asVendorPromise } from '../vendor-promise.js'

type OriginalRef = { proto: any; key: string; original: (...args: any[]) => any }
let originals: OriginalRef[] = []
const PATCH_MARK = Symbol.for('jamjet.anthropic.patched')

/**
 * The Messages resource classes: `messages` and `beta.messages`.
 * `@anthropic-ai/sdk` exposes them as statics on the default export
 * (`Anthropic.Messages`, `Anthropic.Beta.Messages`); it has no `resources`
 * named export, so looking only there patched nothing. `messages.parse()` and
 * `messages.stream()` (and their beta forms) go through `create`.
 */
function messagesClasses(anthropicModule: any): any[] {
  const root = anthropicModule?.default ?? anthropicModule?.Anthropic
  return [
    anthropicModule?.resources?.messages?.Messages ?? root?.Messages,
    anthropicModule?.resources?.beta?.messages?.Messages ?? root?.Beta?.Messages,
  ]
}

export function patchAnthropic(anthropicModule: any): void {
  for (const cls of messagesClasses(anthropicModule)) {
    if (!cls?.prototype) continue
    const proto = cls.prototype
    if (proto[PATCH_MARK]) continue
    const original = proto.create
    if (typeof original !== 'function') continue

    proto.create = function patchedCreate(this: unknown, ...args: any[]) {
      const client = getActive()
      if (!client) return original.call(this, ...args)
      let raw: unknown
      const enforced = runEnforcedCall({
        client,
        vendor: 'anthropic',
        // Pre-bind `this` so runEnforcedCall's apply(null, ...) contract is satisfied
        original: (...a: any[]) => (raw = original.call(this, ...a)),
        args,
        computePromptPrefixHash: computePrefixHash,
      })
      return asVendorPromise(enforced, () => raw, 'anthropic')
    }

    Object.defineProperty(proto, PATCH_MARK, {
      value: true,
      enumerable: false,
      configurable: true,
      writable: true,
    })
    originals.push({ proto, key: 'create', original })
  }
  const root = anthropicModule?.default ?? anthropicModule?.Anthropic
  refuseBatches(root?.Messages?.Batches?.prototype, 'anthropic messages.batches', originals)
  refuseBatches(root?.Beta?.Messages?.Batches?.prototype, 'anthropic beta.messages.batches', originals)
}

export function unpatchAnthropic(): void {
  for (const { proto, key, original } of originals) {
    proto[key] = original
    delete proto[PATCH_MARK]
    unmarkBatches(proto)
  }
  originals = []
}

import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'
// Node-only: pulls in `node:crypto` via prefix-hash.ts. Safe here because
// this patcher is only loaded from `src/node.ts`, never the universal entry.
import { computePrefixHash } from '../prefix-hash.js'
import { asVendorPromise } from '../vendor-promise.js'

type OriginalRef = { proto: any; original: (...args: any[]) => any }
let originals: OriginalRef[] = []
const PATCH_MARK = Symbol.for('jamjet.anthropic.patched')

/**
 * The Messages resource class. `@anthropic-ai/sdk` exposes it as a static on
 * the default export (`Anthropic.Messages`); it has no `resources` named
 * export, so looking only there patched nothing.
 */
function messagesClass(anthropicModule: any): any {
  return (
    anthropicModule?.resources?.messages?.Messages ??
    anthropicModule?.default?.Messages ??
    anthropicModule?.Anthropic?.Messages
  )
}

export function patchAnthropic(anthropicModule: any): void {
  const cls = messagesClass(anthropicModule)
  if (!cls?.prototype) return
  const proto = cls.prototype
  if (proto[PATCH_MARK]) return

  const original = proto.create
  if (typeof original !== 'function') return

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

export function unpatchAnthropic(): void {
  for (const { proto, original } of originals) {
    proto.create = original
    delete proto[PATCH_MARK]
  }
  originals = []
}

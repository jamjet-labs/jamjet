import { getActive } from '../client.js'
import { runEnforcedCall } from '../enforcement.js'

type OriginalRef = { proto: any; original: (...args: any[]) => any }
let originals: OriginalRef[] = []
const PATCH_MARK = Symbol.for('jamjet.anthropic.patched')

export function patchAnthropic(anthropicModule: any): void {
  const messagesClass = anthropicModule?.resources?.messages?.Messages
  if (!messagesClass?.prototype) return
  const proto = messagesClass.prototype
  if (proto[PATCH_MARK]) return

  const original = proto.create
  if (typeof original !== 'function') return

  proto.create = async function patchedCreate(this: unknown, ...args: any[]) {
    const client = getActive()
    if (!client) return original.call(this, ...args)
    return runEnforcedCall({
      client,
      vendor: 'anthropic',
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

export function unpatchAnthropic(): void {
  for (const { proto, original } of originals) {
    proto.create = original
    delete proto[PATCH_MARK]
  }
  originals = []
}

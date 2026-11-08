import { getActive } from './client.js'
import { JamjetUnenforceableCall } from './errors.js'

/**
 * SDK methods this package does not enforce (OpenAI's Responses API). They
 * run normally, unless a block or require_approval rule is active: then they
 * throw rather than run unchecked.
 */
export const UNENFORCED_OPENAI_METHODS = ['create', 'stream', 'parse'] as const

/**
 * Batch APIs submit many requests (tool definitions included) that run later,
 * outside this process; they are refused while restrictive rules exist.
 */
export function refuseBatches(proto: any, label: string, saved: Array<{ proto: any; key: string; original: any }>): void {
  if (!proto || typeof proto.create !== 'function' || proto[BATCH_MARK]) return
  const original = proto.create
  proto.create = refuseUnenforced(`${label}.create`, original)
  Object.defineProperty(proto, BATCH_MARK, { value: true, enumerable: false, configurable: true, writable: true })
  saved.push({ proto, key: 'create', original })
}

const BATCH_MARK = Symbol.for('jamjet.batches.refused')

export function unmarkBatches(proto: any): void {
  if (proto) delete proto[BATCH_MARK]
}

export function refuseUnenforced<F extends (...args: any[]) => any>(label: string, original: F): F {
  return function refusing(this: unknown, ...args: any[]) {
    if (getActive()?._policy.hasRestrictiveRules()) throw new JamjetUnenforceableCall(label)
    return original.apply(this, args)
  } as F
}

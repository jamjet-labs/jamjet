import { getActive } from './client.js'
import { JamjetUnenforceableCall } from './errors.js'

/**
 * SDK methods this package does not enforce (OpenAI's Responses API). They
 * run normally, unless a block or require_approval rule is active: then they
 * throw rather than run unchecked.
 */
export const UNENFORCED_OPENAI_METHODS = ['create', 'stream', 'parse'] as const

export function refuseUnenforced<F extends (...args: any[]) => any>(label: string, original: F): F {
  return function refusing(this: unknown, ...args: any[]) {
    if (getActive()?._policy.hasRestrictiveRules()) throw new JamjetUnenforceableCall(label)
    return original.apply(this, args)
  } as F
}

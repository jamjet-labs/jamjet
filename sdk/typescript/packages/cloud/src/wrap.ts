import { getActive } from './client.js'
import type { AgentRef, UserContext } from './context.js'
import { runEnforcedCall } from './enforcement.js'

const WRAPPED = Symbol.for('jamjet.wrapped')
type WrappedFn<T> = T & { [WRAPPED]?: true }

function isWrapped(fn: unknown): boolean {
  return typeof fn === 'function' && (fn as WrappedFn<unknown>)[WRAPPED] === true
}

export interface WrapOptions {
  agent?: AgentRef
  user?: UserContext
}

export function wrap<T extends object>(client: T, opts?: WrapOptions): T {
  const anyClient = client as any
  if (anyClient?.chat?.completions?.create) wrapMethod(anyClient.chat.completions, 'create', 'openai', opts)
  if (anyClient?.completions?.create) wrapMethod(anyClient.completions, 'create', 'openai', opts)
  if (anyClient?.messages?.create) wrapMethod(anyClient.messages, 'create', 'anthropic', opts)
  return client
}

function wrapMethod(
  target: Record<string, any>,
  key: string,
  vendor: 'openai' | 'anthropic',
  opts?: WrapOptions,
): void {
  const original = target[key]
  if (typeof original !== 'function' || isWrapped(original)) return

  const wrapped: WrappedFn<typeof original> = async function (this: unknown, ...args: any[]) {
    const client = getActive()
    if (!client) {
      // No client → behave as pass-through (Plan 1 contract)
      return original.call(this, ...args)
    }
    return runEnforcedCall({
      client,
      vendor,
      // Pre-bind `original` to `this` so runEnforcedCall can apply(null, ...) safely
      original: (...a: any[]) => original.call(this, ...a),
      args,
      ...(opts ? { override: opts } : {}),
    })
  } as WrappedFn<typeof original>

  ;(wrapped as WrappedFn<typeof original>)[WRAPPED] = true
  target[key] = wrapped
}

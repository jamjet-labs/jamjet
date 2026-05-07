import { getActive } from './client.js'
import { estimateCost } from './cost.js'
import { Span } from './span.js'

const WRAPPED = Symbol.for('jamjet.wrapped')

type WrappedFn<T> = T & { [WRAPPED]?: true }

function isWrapped(fn: unknown): boolean {
  return typeof fn === 'function' && (fn as WrappedFn<unknown>)[WRAPPED] === true
}

function newId(): string {
  return Array.from({ length: 16 }, () =>
    Math.floor(Math.random() * 16).toString(16),
  ).join('')
}

export function wrap<T extends object>(client: T): T {
  const anyClient = client as any
  if (anyClient?.chat?.completions?.create) {
    wrapMethod(anyClient.chat.completions, 'create', 'openai')
  }
  if (anyClient?.completions?.create) {
    wrapMethod(anyClient.completions, 'create', 'openai')
  }
  if (anyClient?.messages?.create) {
    wrapMethod(anyClient.messages, 'create', 'anthropic')
  }
  return client
}

function wrapMethod(
  target: Record<string, any>,
  key: string,
  vendor: 'openai' | 'anthropic',
): void {
  const original = target[key]
  if (typeof original !== 'function' || isWrapped(original)) return

  const wrapped: WrappedFn<typeof original> = async function (this: unknown, ...args: any[]) {
    const arg0 = args[0] ?? {}
    const model = typeof arg0.model === 'string' ? arg0.model : 'unknown'
    const span = new Span({
      traceId: newId(),
      spanId: newId(),
      kind: 'llm_call',
      name: `${vendor}.${model}`,
    })
    span.model = model

    const client = getActive()
    try {
      const result = await original.call(this, ...args)
      const usage = result?.usage ?? {}
      const inputTokens = usage.prompt_tokens ?? usage.input_tokens ?? 0
      const outputTokens = usage.completion_tokens ?? usage.output_tokens ?? 0
      span.inputTokens = Number(inputTokens) || 0
      span.outputTokens = Number(outputTokens) || 0
      const actualModel = typeof result?.model === 'string' ? result.model : model
      span.model = actualModel
      span.name = `${vendor}.${actualModel}`
      span.costUsd = estimateCost(actualModel, span.inputTokens, span.outputTokens)
      if (client?.config.agent) span.agentName = client.config.agent
      if (client?.config.environment) span.environment = client.config.environment
      span.finish('ok')
      client?.recordSpan(span.toEventDict())
      return result
    } catch (err) {
      span.finish('error')
      span.payload = { error: (err as Error).message }
      if (client?.config.agent) span.agentName = client.config.agent
      if (client?.config.environment) span.environment = client.config.environment
      client?.recordSpan(span.toEventDict())
      throw err
    }
  } as WrappedFn<typeof original>

  ;(wrapped as WrappedFn<typeof original>)[WRAPPED] = true
  target[key] = wrapped
}

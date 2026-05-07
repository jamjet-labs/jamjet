import { getActive } from '../client.js'
import { estimateCost } from '../cost.js'
import { Span } from '../span.js'

type OriginalRef = { proto: { create: (...args: any[]) => any }; original: (...args: any[]) => any }
let originals: OriginalRef[] = []

const PATCH_MARK = Symbol.for('jamjet.openai.patched')

function newId(): string {
  return Array.from({ length: 16 }, () => Math.floor(Math.random() * 16).toString(16)).join('')
}

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
      const arg0 = args[0] ?? {}
      const model = typeof arg0.model === 'string' ? arg0.model : 'unknown'
      const span = new Span({
        traceId: newId(),
        spanId: newId(),
        kind: 'llm_call',
        name: `openai.${model}`,
      })
      span.model = model
      const client = getActive()
      try {
        const result = await original.call(this, ...args)
        const usage = result?.usage ?? {}
        const inputTokens = Number(usage.prompt_tokens ?? 0) || 0
        const outputTokens = Number(usage.completion_tokens ?? 0) || 0
        const actualModel = typeof result?.model === 'string' ? result.model : model
        span.model = actualModel
        span.name = `openai.${actualModel}`
        span.inputTokens = inputTokens
        span.outputTokens = outputTokens
        span.costUsd = estimateCost(actualModel, inputTokens, outputTokens)
        if (client?.config.agent) span.agentName = client.config.agent
        if (client?.config.environment) span.environment = client.config.environment
        span.finish('ok')
        client?.recordSpan(span.toEventDict())
        return result
      } catch (err) {
        span.finish('error')
        span.payload = { error: (err as Error).message }
        if (client?.config.agent) span.agentName = client.config.agent
        client?.recordSpan(span.toEventDict())
        throw err
      }
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

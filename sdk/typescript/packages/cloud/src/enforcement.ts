import type { MessageParam } from '@anthropic-ai/sdk/resources/messages'
import type { Client } from './client.js'
import type { AgentRef, UserContext } from './context.js'
import { estimateCost } from './cost.js'
import { JamjetPolicyBlocked } from './errors.js'
import { Span } from './span.js'

type Vendor = 'openai' | 'anthropic'

export interface EnforcedCallOptions {
  client: Client
  vendor: Vendor
  original: (...args: any[]) => any
  args: any[]
  override?: { agent?: AgentRef; user?: UserContext }
  /**
   * Optional callback to compute a prefix hash of the prompt for cost-waste
   * detection. Injected by Node-only patchers (see `src/patcher/anthropic.ts`)
   * so this module stays runtime-agnostic — universal callers (`src/wrap.ts`)
   * omit it and the field is simply absent from the recorded span.
   *
   * If the callback throws, the LLM call MUST still complete; a hash failure
   * is recorded as a warning and the span goes out without the field.
   */
  computePromptPrefixHash?: (input: string | MessageParam[]) => string
}

function newId(): string {
  return Array.from({ length: 16 }, () => Math.floor(Math.random() * 16).toString(16)).join('')
}

function estimatePromptTokens(args0: unknown): number {
  try {
    const a = args0 as Record<string, unknown>
    const messages = Array.isArray(a['messages']) ? a['messages'] : []
    const totalChars = JSON.stringify(messages).length
    return Math.ceil(totalChars / 4)
  } catch {
    return 0
  }
}

function getToolCalls(vendor: Vendor, response: unknown): Array<{ id?: string; name: string }> {
  const res = response as Record<string, unknown>
  if (vendor === 'openai') {
    const choices = Array.isArray(res['choices']) ? res['choices'] : []
    const first = choices[0] as Record<string, unknown> | undefined
    const message = (first?.['message'] ?? {}) as Record<string, unknown>
    const tcs = Array.isArray(message['tool_calls']) ? message['tool_calls'] : []
    return (tcs as Array<Record<string, unknown>>).map((tc) => {
      const fn = (tc['function'] ?? {}) as Record<string, unknown>
      const name = typeof fn['name'] === 'string' ? fn['name'] : ''
      const entry: { id?: string; name: string } = { name }
      if (typeof tc['id'] === 'string') entry.id = tc['id']
      return entry
    })
  }
  // Anthropic
  const content = Array.isArray(res['content']) ? res['content'] : []
  return (content as Array<Record<string, unknown>>)
    .filter((b) => b['type'] === 'tool_use')
    .map((b) => {
      const name = typeof b['name'] === 'string' ? b['name'] : ''
      const entry: { id?: string; name: string } = { name }
      if (typeof b['id'] === 'string') entry.id = b['id']
      return entry
    })
}

function extractUsage(vendor: Vendor, response: unknown): { input: number; output: number } {
  const res = response as Record<string, unknown>
  const usage = (res['usage'] ?? {}) as Record<string, unknown>
  if (vendor === 'openai') {
    return { input: Number(usage['prompt_tokens']) || 0, output: Number(usage['completion_tokens']) || 0 }
  }
  return { input: Number(usage['input_tokens']) || 0, output: Number(usage['output_tokens']) || 0 }
}

export async function runEnforcedCall(opts: EnforcedCallOptions): Promise<unknown> {
  const { client, vendor, original, args, override, computePromptPrefixHash } = opts
  const arg0 = (args[0] ?? {}) as Record<string, unknown>
  const model = typeof arg0['model'] === 'string' ? arg0['model'] : 'unknown'
  const isStreaming = arg0['stream'] === true
  const messages = Array.isArray(arg0['messages']) ? (arg0['messages'] as MessageParam[]) : []

  // Resolve identity from override > context > config default
  const ctx = client._governanceContext.getCurrentContext()
  const agentRef = override?.agent ?? ctx?.agent
  const userCtx = override?.user ?? ctx?.user

  // 1. Pre-call policy: filter tools
  let mutatedArgs = args
  const policyDecisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }> = []
  if (Array.isArray(arg0['tools']) && arg0['tools'].length > 0) {
    const { allowed, blocked } = client._policy.filterTools(arg0['tools'] as Array<{ function?: { name?: string } }>)
    for (const t of blocked) {
      const name = t.function?.name ?? ''
      const d = client._policy.evaluate(name)
      policyDecisions.push({ tool_name: name, policy_kind: d.policyKind, pattern: d.pattern })
    }
    if (blocked.length > 0) {
      mutatedArgs = [{ ...arg0, tools: allowed }, ...args.slice(1)]
    }
  }

  // 2. Pre-call budget check
  const estTokens = estimatePromptTokens(arg0)
  const estCost = estimateCost(model, estTokens, 0)
  client._budget.checkOrThrow({ estimatedCost: estCost })

  // 3. Open span
  const span = new Span({ traceId: newId(), spanId: newId(), kind: 'llm_call', name: `${vendor}.${model}` })
  span.model = model

  if (agentRef?.name !== undefined) {
    span.agentName = agentRef.name
  } else if (typeof client.config.agent === 'string') {
    span.agentName = client.config.agent
  }

  if (userCtx?.userId !== undefined) span.userId = userCtx.userId
  if (userCtx?.email !== undefined) span.userEmail = userCtx.email
  if (userCtx?.attrs !== undefined) span.userAttrs = userCtx.attrs

  if (client.config.environment !== undefined) span.environment = client.config.environment
  if (client.config.releaseVersion !== undefined) span.releaseVersion = client.config.releaseVersion

  if (policyDecisions.length > 0) span.policyDecisions = policyDecisions
  span.budgetCheck = { estimated: estCost, allowed: true }

  // Compute prompt prefix hash for cost-waste detection. The hash function
  // lives in a Node-only module (uses node:crypto), so it's injected here by
  // the patchers rather than imported directly — keeps this file runtime-
  // agnostic and prevents `node:crypto` from leaking into the universal
  // bundle via `wrap.ts`. A failing hash MUST NEVER break an LLM call.
  if (computePromptPrefixHash !== undefined) {
    try {
      span.setPromptPrefixHash(computePromptPrefixHash(messages))
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err)
      console.warn(`[jamjet] prompt prefix hash failed: ${msg}`)
    }
  }

  try {
    const result = await original.apply(null, mutatedArgs)

    if (!isStreaming) {
      // 4. Post-decision policy: re-check tool_calls returned by the model
      // NOTE (Plan 2 limitation): require_approval matches do not gate the call
      // here. Tools matched by require_approval rules pass through to the model
      // and to user code unchanged. Pre-call approval gating is deferred to a
      // future release; see docs/superpowers/specs/2026-05-08-ts-sdk-plan2.md §6.
      const toolCalls = getToolCalls(vendor, result)
      for (const tc of toolCalls) {
        const d = client._policy.evaluate(tc.name)
        if (d.blocked) {
          const blockedEntry: { id?: string; name: string } = { name: tc.name }
          if (tc.id !== undefined) blockedEntry.id = tc.id
          span.policyBlockedToolCalls = [blockedEntry]
          span.finish('error')
          client.recordSpan(span.toEventDict())
          throw new JamjetPolicyBlocked(tc.name, d.pattern ?? '*', { cause: tc })
        }
      }

      // 5. Post-call cost record
      const usage = extractUsage(vendor, result)
      const res = result as Record<string, unknown>
      const actualModel = typeof res['model'] === 'string' ? res['model'] : model
      span.model = actualModel
      span.name = `${vendor}.${actualModel}`
      span.inputTokens = usage.input
      span.outputTokens = usage.output
      span.costUsd = estimateCost(actualModel, usage.input, usage.output)
      client._budget.record(span.costUsd)
    }

    span.finish('ok')
    client.recordSpan(span.toEventDict())
    return result
  } catch (err) {
    if (!(err instanceof JamjetPolicyBlocked)) {
      span.finish('error')
      const errMsg = err instanceof Error ? err.message : String(err)
      span.payload = { error: errMsg }
      client.recordSpan(span.toEventDict())
    }
    throw err
  }
}

import type { MessageParam } from '@anthropic-ai/sdk/resources/messages'
import type { Client } from './client.js'
import type { AgentRef, UserContext } from './context.js'
import { applyCacheInject } from './cache-inject.js'
import { applyCompaction } from './compaction.js'
import { estimateCost } from './cost.js'
import { JamjetPolicyBlocked } from './errors.js'
import { toolNameOf } from './policy.js'
import { Span } from './span.js'
import { enforceStream, type StreamedToolCall, type StreamTally, type Vendor } from './stream-enforcement.js'

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

/** ~4 characters per token over everything sent as input: messages, tool definitions, system, legacy prompt. */
function estimatePromptTokens(args0: unknown): number {
  try {
    const a = args0 as Record<string, unknown>
    const input = [a['messages'], a['tools'], a['system'], a['prompt']].filter((v) => v !== undefined)
    const totalChars = JSON.stringify(input).length
    return Math.ceil(totalChars / 4)
  } catch {
    return 0
  }
}

function getToolCalls(vendor: Vendor, response: unknown): Array<{ id?: string; name: string }> {
  const res = (response ?? {}) as Record<string, unknown>
  if (vendor === 'openai') {
    // Every choice: with n > 1 a blocked call in any of them must be caught.
    const choices = Array.isArray(res['choices']) ? res['choices'] : []
    return (choices as Array<Record<string, unknown>>).flatMap((choice) => {
      const message = (choice?.['message'] ?? {}) as Record<string, unknown>
      const tcs = Array.isArray(message['tool_calls']) ? message['tool_calls'] : []
      const calls = (tcs as Array<Record<string, unknown>>).map((tc) => {
        const entry: { id?: string; name: string } = { name: toolNameOf(tc) }
        if (typeof tc['id'] === 'string') entry.id = tc['id']
        return entry
      })
      // Legacy `functions` API: a single message.function_call.
      const fc = message['function_call']
      if (typeof fc === 'object' && fc !== null) calls.push({ name: toolNameOf(fc) })
      return calls
    })
  }
  // Anthropic: client tool calls and server tool calls (e.g. web_search)
  const content = Array.isArray(res['content']) ? res['content'] : []
  return (content as Array<Record<string, unknown>>)
    .filter((b) => b['type'] === 'tool_use' || b['type'] === 'server_tool_use' || b['type'] === 'mcp_tool_use')
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

function extractCacheRead(_vendor: Vendor, response: unknown): number {
  const usage = ((response as Record<string, any>)?.usage ?? {}) as Record<string, unknown>
  return Number(usage['cache_read_input_tokens'] ?? 0) || 0
}

// Per-MTok savings = input price − cache_read price; mirrors pricing.rs v_2026_05_17.
const CACHE_SAVING_PER_MTOK: Record<string, number> = {
  'claude-opus-4-7': 15.0 - 1.5,
  'claude-sonnet-4-6': 3.0 - 0.3,
  'claude-haiku-4-5': 1.0 - 0.1,
  'claude-haiku-4-5-20251001': 1.0 - 0.1,
}

function cacheReadSavingsUsd(model: string, cacheReadTokens: number): number {
  const per = CACHE_SAVING_PER_MTOK[model] ?? 0
  return (cacheReadTokens / 1_000_000) * per
}

/**
 * Tools the vendor runs itself (Anthropic server tools such as web_search, and
 * MCP toolsets) execute before the response comes back, so refusing their
 * calls afterwards is too late: a rule that could match them strips them here.
 */
function isServerExecuted(tool: unknown): boolean {
  if (typeof tool !== 'object' || tool === null) return false
  const t = tool as Record<string, unknown>
  const type = t['type']
  if (typeof type !== 'string' || type === 'function' || type === 'custom') return false
  return !('input_schema' in t)
}

function strippedBy(client: Client, tool: unknown): { name: string; kind: string; pattern: string | null } | null {
  const t = tool as Record<string, unknown>
  if (t?.['type'] === 'mcp_toolset') {
    // Tool names inside an MCP server are unknown before the call; any
    // restrictive rule could match one.
    return client._policy.hasRestrictiveRules()
      ? { name: `mcp_toolset:${String(t['mcp_server_name'] ?? '')}`, kind: 'block', pattern: '*' }
      : null
  }
  const name = toolNameOf(tool)
  const d = client._policy.evaluate(name)
  if (d.blocked) return { name, kind: d.policyKind, pattern: d.pattern }
  if (d.policyKind === 'require_approval' && isServerExecuted(tool)) {
    return { name, kind: d.policyKind, pattern: d.pattern }
  }
  return null
}

/**
 * Remove refused tools from `tools` and from the legacy OpenAI `functions`.
 * Refuses a call whose tool_choice / function_call forces a refused tool, and
 * drops the settings vendors reject once no tools are left. Returns arg0
 * unchanged when nothing was removed.
 */
function filterRequestTools(
  client: Client,
  arg0: Record<string, unknown>,
  decisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }>,
): Record<string, unknown> {
  let next = arg0
  const lists: Array<{ key: 'tools' | 'functions'; choice: 'tool_choice' | 'function_call'; drop: string[] }> = [
    { key: 'tools', choice: 'tool_choice', drop: ['tool_choice', 'parallel_tool_calls'] },
    { key: 'functions', choice: 'function_call', drop: ['function_call'] },
  ]
  for (const { key, choice, drop } of lists) {
    const list = arg0[key]
    if (!Array.isArray(list) || list.length === 0) continue
    const kept: unknown[] = []
    const removed: string[] = []
    for (const tool of list) {
      const hit = strippedBy(client, tool)
      if (hit === null) {
        kept.push(tool)
        continue
      }
      removed.push(hit.name)
      decisions.push({ tool_name: hit.name, policy_kind: hit.kind, pattern: hit.pattern })
    }
    if (removed.length === 0) continue
    // A forced choice of a removed tool cannot be honoured: refuse the call
    // rather than let the vendor reject an unknown tool name.
    const forced = toolNameOf(arg0[choice])
    if (forced !== '' && removed.includes(forced)) {
      const d = client._policy.evaluate(forced)
      throw new JamjetPolicyBlocked(forced, d.pattern ?? '*')
    }
    next = next === arg0 ? { ...arg0 } : next
    next[key] = kept
    if (kept.length === 0) {
      // Vendors reject an empty list, and the settings that only make sense with tools.
      delete next[key]
      for (const k of drop) delete next[k]
    }
  }
  // Remote MCP servers run their tools on the vendor side under names unknown
  // before the call; with any restrictive rule they are not offered.
  const servers = arg0['mcp_servers']
  if (Array.isArray(servers) && servers.length > 0 && client._policy.hasRestrictiveRules()) {
    next = next === arg0 ? { ...arg0 } : next
    delete next['mcp_servers']
    decisions.push({ tool_name: 'mcp_servers', policy_kind: 'block', pattern: '*' })
  }
  return next
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

  // 1. Pre-call policy: strip refused tools from the request.
  const policyDecisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }> = []
  const filtered = filterRequestTools(client, arg0, policyDecisions)
  let mutatedArgs = filtered === arg0 ? args : [filtered, ...args.slice(1)]

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
  let prefixHash: string | undefined
  if (computePromptPrefixHash !== undefined) {
    try {
      prefixHash = computePromptPrefixHash(messages)
      span.setPromptPrefixHash(prefixHash)
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err)
      console.warn(`[jamjet] prompt prefix hash failed: ${msg}`)
    }
  }

  // Tool-result compaction: truncate oversized tool_result messages before the model call.
  // Opt-in per tool via tool_compaction policy. Fail-open: if no rules, stage is skipped.
  let compactionSaved = 0
  if (client._compaction.hasRules()) {
    const { mutated, tokensSaved } = applyCompaction(
      mutatedArgs[0] as Record<string, unknown>,
      client._compaction,
    )
    if (tokensSaved > 0) {
      mutatedArgs = [mutated, ...mutatedArgs.slice(1)]
      compactionSaved = tokensSaved
    }
  }

  // Cache injection: add cache_control to stable prompt prefix when policy matches.
  // Safe-identical: Anthropic caches are content-addressed — price changes, output does not.
  let cacheInjected = false
  if (vendor === 'anthropic' && client._cacheInject.shouldInject(prefixHash)) {
    const arg0obj = mutatedArgs[0] as Record<string, unknown>
    const { mutated, injected } = applyCacheInject(arg0obj)
    if (injected) {
      mutatedArgs = [mutated, ...mutatedArgs.slice(1)]
      cacheInjected = true
    }
  }

  const state: CallState = { client, span, vendor, model, cacheInjected, compactionSaved }

  let result: unknown
  try {
    result = await original.apply(null, mutatedArgs)
  } catch (err) {
    failSpan(state, err)
    throw err
  }

  if (isStreaming) {
    const streamState = (tally: StreamTally): CallState => ({ ...state, model: tally.model ?? model })
    const refuseStreamed = (calls: ReadonlyArray<StreamedToolCall>, tally: StreamTally): void => {
      // Check the name the openai SDK keeps (last fragment) and the joined
      // fragments, so a consumer that concatenates them cannot get a blocked name.
      const names = calls.flatMap((c) => {
        const base = c.id !== undefined ? { id: c.id } : {}
        return c.joinedName !== '' && c.joinedName !== c.name
          ? [{ ...base, name: c.name }, { ...base, name: c.joinedName }]
          : [{ ...base, name: c.name }]
      })
      if (firstBlocked(client, names) === null) return
      recordUsage(streamState(tally), streamUsage(tally, estTokens, false), tally.cacheReadTokens, {
        usage_estimated: true,
        stream_incomplete: true,
      })
      refuseBlocked(state, names)
    }
    const enforced = enforceStream(result, {
      vendor,
      holdToolCalls: () => client._policy.hasRules(),
      onToolCallStarted: (call, tally) => refuseStreamed([{ ...call, joinedName: call.name, arguments: '' }], tally),
      onToolCalls: (calls, tally) => refuseStreamed(calls, tally),
      onEnd: (tally, end) => {
        if (end.kind === 'error') {
          if (end.error instanceof JamjetPolicyBlocked) return // recorded where it was raised
          // The tokens the stream produced before failing were still spent.
          recordUsage(streamState(tally), streamUsage(tally, estTokens, false), tally.cacheReadTokens, {
            usage_estimated: true,
            stream_incomplete: true,
          })
          failSpan(state, end.error)
          return
        }
        const complete = end.kind === 'complete' && tally.finished
        const reported = tally.inputTokens !== undefined && tally.outputTokens !== undefined
        recordUsage(streamState(tally), streamUsage(tally, estTokens, complete), tally.cacheReadTokens, {
          ...(complete && reported ? {} : { usage_estimated: true }),
          ...(complete ? {} : { stream_incomplete: true }),
        })
        span.finish('ok')
        client.recordSpan(span.toEventDict())
      },
    })
    if (enforced !== null) return enforced
    // Not iterable: nothing to observe. Record what we know and pass it on.
    span.finish('ok')
    client.recordSpan(span.toEventDict())
    return result
  }

  // 4. Post-call cost record. Runs before the policy re-check so a refused
  // response still counts against the budget: the tokens were spent.
  const res = result as Record<string, unknown>
  recordUsage(
    { ...state, model: typeof res['model'] === 'string' ? res['model'] : model },
    extractUsage(vendor, result),
    extractCacheRead(vendor, result),
  )

  // 5. Post-decision policy: re-check tool calls returned by the model
  // NOTE (Plan 2 limitation): require_approval matches do not gate the call
  // here. Tools matched by require_approval rules pass through to the model
  // and to user code unchanged. Pre-call approval gating is deferred to a
  // future release; see docs/superpowers/specs/2026-05-08-ts-sdk-plan2.md §6.
  refuseBlocked(state, getToolCalls(vendor, result))

  span.finish('ok')
  client.recordSpan(span.toEventDict())
  return result
}

interface CallState {
  client: Client
  span: Span
  vendor: Vendor
  model: string
  cacheInjected: boolean
  compactionSaved: number
}

function firstBlocked(
  client: Client,
  calls: ReadonlyArray<{ id?: string; name: string }>,
): { call: { id?: string; name: string }; pattern: string } | null {
  for (const call of calls) {
    const d = client._policy.evaluate(call.name)
    if (d.blocked) return { call, pattern: d.pattern ?? '*' }
  }
  return null
}

/** Throws JamjetPolicyBlocked (and records the span) on the first blocked call. */
function refuseBlocked(state: CallState, calls: ReadonlyArray<{ id?: string; name: string }>): void {
  const hit = firstBlocked(state.client, calls)
  if (hit === null) return
  const blockedEntry: { id?: string; name: string } = { name: hit.call.name }
  if (hit.call.id !== undefined) blockedEntry.id = hit.call.id
  state.span.policyBlockedToolCalls = [blockedEntry]
  state.span.finish('error')
  state.client.recordSpan(state.span.toEventDict())
  throw new JamjetPolicyBlocked(hit.call.name, hit.pattern, { cause: hit.call })
}

function failSpan(state: CallState, err: unknown): void {
  if (err instanceof JamjetPolicyBlocked) return
  state.span.finish('error')
  const errMsg = err instanceof Error ? err.message : String(err)
  state.span.payload = { ...state.span.payload, error: errMsg }
  state.client.recordSpan(state.span.toEventDict())
}

/**
 * Usage a stream reported, or an estimate (prompt estimate, ~4 characters per
 * output token). A stream cut short has only a partial output count, so the
 * larger of the reported count and the estimate is used.
 */
function streamUsage(
  tally: StreamTally,
  estimatedPromptTokens: number,
  complete: boolean,
): { input: number; output: number } {
  const estimatedOutput = Math.ceil(tally.outputChars / 4)
  const output = complete
    ? (tally.outputTokens ?? estimatedOutput)
    : Math.max(tally.outputTokens ?? 0, estimatedOutput)
  return { input: tally.inputTokens ?? estimatedPromptTokens, output }
}

function recordUsage(
  state: CallState,
  usage: { input: number; output: number },
  cacheRead: number,
  extraPayload: Record<string, unknown> = {},
): void {
  const { span, vendor, model } = state
  span.model = model
  span.name = `${vendor}.${model}`
  span.inputTokens = usage.input
  span.outputTokens = usage.output
  span.costUsd = estimateCost(model, usage.input, usage.output)
  state.client._budget.record(span.costUsd)

  if (state.cacheInjected) {
    const savedUsd = cacheReadSavingsUsd(model, cacheRead)
    span.payload = {
      ...span.payload,
      // Fractional cents intentional — Math.round would zero sub-cent per-call
      // savings. Authoritative totals are recomputed server-side from
      // cache_read_input_tokens; this is a secondary per-call annotation.
      saved_cents: savedUsd * 100,
      cache_injected: true,
    }
  }

  if (state.compactionSaved > 0) {
    span.payload = { ...span.payload, tokens_saved: state.compactionSaved, compacted: true }
  }
  if (Object.keys(extraPayload).length > 0) span.payload = { ...span.payload, ...extraPayload }
}

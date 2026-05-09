import type { AgentRef, UserContext } from '@jamjet/cloud'
import { estimateCost, getActive, JamjetPolicyBlocked, Span } from '@jamjet/cloud'
import type { LanguageModelMiddleware } from 'ai'

const NOT_INIT = 'JamJet Cloud not initialized. Call init() first.'

export interface JamjetMiddlewareOptions {
  agent?: AgentRef
  user?: UserContext
}

interface AISDKTool {
  type: 'function' | string
  name?: string
  description?: string
  inputSchema?: unknown
}

interface AISDKUsage {
  inputTokens?: number
  outputTokens?: number
  totalTokens?: number
}

function modelIdOf(model: any, params: any): string {
  return (
    (typeof params?.model === 'string' && params.model) ||
    (typeof model?.modelId === 'string' && model.modelId) ||
    'unknown'
  )
}

function estimatePromptTokens(params: any): number {
  try {
    const prompt = params?.prompt ?? params?.messages ?? []
    return Math.ceil(JSON.stringify(prompt).length / 4)
  } catch {
    return 0
  }
}

function filterToolsForAISDK(
  client: ReturnType<typeof getActive> & object,
  tools: AISDKTool[] | undefined,
): {
  allowed: AISDKTool[]
  decisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }>
} {
  if (!Array.isArray(tools) || tools.length === 0) {
    return { allowed: [], decisions: [] }
  }
  const allowed: AISDKTool[] = []
  const decisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }> = []
  for (const tool of tools) {
    const name = tool.name ?? ''
    const decision = (client as any)._policy.evaluate(name)
    if (decision.blocked) {
      decisions.push({ tool_name: name, policy_kind: decision.policyKind, pattern: decision.pattern })
    } else {
      allowed.push(tool)
    }
  }
  return { allowed, decisions }
}

function resolveIdentity(
  client: ReturnType<typeof getActive> & object,
  override: JamjetMiddlewareOptions | undefined,
): { agent?: AgentRef; user?: UserContext } {
  const ctx = (client as any)._governanceContext.getCurrentContext()
  const out: { agent?: AgentRef; user?: UserContext } = {}
  const a = override?.agent ?? ctx?.agent
  const u = override?.user ?? ctx?.user
  if (a) out.agent = a
  if (u) out.user = u
  return out
}

function newId(): string {
  return Array.from({ length: 16 }, () => Math.floor(Math.random() * 16).toString(16)).join('')
}

function openSpan(
  client: ReturnType<typeof getActive> & object,
  modelId: string,
  flow: 'generate' | 'stream',
  identity: { agent?: AgentRef; user?: UserContext },
  decisions: Array<{ tool_name: string; policy_kind: string; pattern: string | null }>,
  estimatedCost: number,
): Span {
  const span = new Span({
    traceId: newId(),
    spanId: newId(),
    kind: 'llm_call',
    name: `ai.${flow === 'stream' ? 'streamText' : 'generateText'}.${modelId}`,
  })
  span.model = modelId
  if (identity.agent?.name) span.agentName = identity.agent.name
  else if ((client as any).config?.agent) span.agentName = (client as any).config.agent
  if (identity.user?.userId) span.userId = identity.user.userId
  if (identity.user?.email) span.userEmail = identity.user.email
  if (identity.user?.attrs) span.userAttrs = identity.user.attrs
  if ((client as any).config?.environment) span.environment = (client as any).config.environment
  if ((client as any).config?.releaseVersion) span.releaseVersion = (client as any).config.releaseVersion
  if (decisions.length > 0) span.policyDecisions = decisions
  span.budgetCheck = { estimated: estimatedCost, allowed: true }
  return span
}

function spanWithSource(span: Span): Record<string, unknown> {
  const dict = span.toEventDict() as Record<string, unknown>
  dict.source = 'middleware'
  return dict
}

export function jamjetMiddleware(opts?: JamjetMiddlewareOptions): LanguageModelMiddleware {
  return {
    async wrapGenerate({ doGenerate, params, model }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)

      const identity = resolveIdentity(client, opts)
      const modelId = modelIdOf(model, params)

      // Pre-call: filter tools (AI SDK 5.x shape: top-level `name`, not nested under `function.name`)
      const { allowed, decisions } = filterToolsForAISDK(client, (params as any).tools)
      if (decisions.length > 0) {
        ;(params as any).tools = allowed
      }

      // Pre-call: budget check
      const estTokens = estimatePromptTokens(params)
      const estCost = estimateCost(modelId, estTokens, 0)
      client._budget.checkOrThrow({ estimatedCost: estCost })

      // Open span
      const span = openSpan(client, modelId, 'generate', identity, decisions, estCost)

      try {
        const result = await doGenerate()

        // Post-decision: scan content[] for tool-call entries and evaluate against policy
        const toolCalls = ((result.content ?? []) as any[])
          .filter((c: any) => c.type === 'tool-call')
          .map((c: any) => ({ id: c.toolCallId, name: c.toolName }))
        for (const tc of toolCalls) {
          const d = (client as any)._policy.evaluate(tc.name)
          if (d.blocked) {
            span.policyBlockedToolCalls = [tc]
            span.finish('error')
            client.recordSpan(spanWithSource(span) as any)
            throw new JamjetPolicyBlocked(tc.name, d.pattern ?? '*', { cause: tc })
          }
        }

        // Post-call: record actual cost from response usage
        const usage = ((result as any).usage ?? {}) as AISDKUsage
        const inputTokens = Number(usage.inputTokens ?? 0) || 0
        const outputTokens = Number(usage.outputTokens ?? 0) || 0
        span.inputTokens = inputTokens
        span.outputTokens = outputTokens
        span.costUsd = estimateCost(modelId, inputTokens, outputTokens)
        client._budget.record(span.costUsd)

        span.finish('ok')
        client.recordSpan(spanWithSource(span) as any)
        return result
      } catch (err) {
        if (!(err instanceof JamjetPolicyBlocked)) {
          span.finish('error')
          span.payload = { error: (err as Error).message }
          client.recordSpan(spanWithSource(span) as any)
        }
        throw err
      }
    },
    async wrapStream({ doStream }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)
      // Task 5 will add streaming enforcement. For now, pass-through.
      return doStream()
    },
  }
}

import type { AgentRef, UserContext } from '@jamjet/cloud'
import { estimateCost, getActive } from '@jamjet/cloud'
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

export function jamjetMiddleware(opts?: JamjetMiddlewareOptions): LanguageModelMiddleware {
  return {
    async wrapGenerate({ doGenerate, params, model }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)

      // Reserve identity for Task 4 (span attribution); call site is here.
      // eslint-disable-next-line @typescript-eslint/no-unused-vars
      const _identity = resolveIdentity(client, opts)
      void _identity
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

      return doGenerate()
    },
    async wrapStream({ doStream }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)
      // Task 5 will add streaming enforcement. For now, pass-through.
      return doStream()
    },
  }
}

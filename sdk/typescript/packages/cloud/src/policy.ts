export type PolicyAction = 'block' | 'allow' | 'require_approval' | 'audit'

export interface PolicyDecision {
  blocked: boolean
  policyKind: PolicyAction
  pattern: string | null
  toolName: string
}

/**
 * The name of a tool definition, in any shape the patchers see:
 * OpenAI function tools `{ type: 'function', function: { name } }`,
 * OpenAI custom tools `{ type: 'custom', custom: { name } }`, and
 * Anthropic tools `{ name, input_schema }` (client and server tools).
 * Also reads a forced `tool_choice` in either vendor's shape.
 * Returns '' when no name is present.
 */
export function toolNameOf(tool: unknown): string {
  if (typeof tool !== 'object' || tool === null) return ''
  const t = tool as Record<string, unknown>
  for (const key of ['function', 'custom']) {
    const inner = t[key]
    if (typeof inner === 'object' && inner !== null) {
      const name = (inner as Record<string, unknown>)['name']
      if (typeof name === 'string') return name
    }
  }
  return typeof t['name'] === 'string' ? t['name'] : ''
}

function globToRegExp(pattern: string): RegExp {
  // fnmatch semantics: * matches any chars (including dots), ? matches single, escape regex specials
  let re = ''
  for (const ch of pattern) {
    if (ch === '*') re += '.*'
    else if (ch === '?') re += '.'
    else if (/[.+^${}()|[\]\\]/.test(ch)) re += '\\' + ch
    else re += ch
  }
  return new RegExp(`^${re}$`)
}

export class PolicyEvaluator {
  private rules: Array<{ action: PolicyAction; pattern: string; regex: RegExp }> = []

  add(action: PolicyAction, pattern: string): void {
    this.rules.push({ action, pattern, regex: globToRegExp(pattern) })
  }

  evaluate(toolName: string): PolicyDecision {
    // First-match-wins per policy spec §5 (rules evaluated top-to-bottom,
    // first matching rule is returned; later rules cannot override).
    let matchedAction: PolicyAction | null = null
    let matchedPattern: string | null = null
    for (const rule of this.rules) {
      if (rule.regex.test(toolName)) {
        matchedAction = rule.action
        matchedPattern = rule.pattern
        break
      }
    }
    if (matchedAction === null) {
      return { blocked: false, policyKind: 'allow', pattern: null, toolName }
    }
    return {
      blocked: matchedAction === 'block',
      policyKind: matchedAction,
      pattern: matchedPattern,
      toolName,
    }
  }

  /** True once any rule has been added. */
  hasRules(): boolean {
    return this.rules.length > 0
  }

  filterTools<T>(tools: readonly T[]): { allowed: T[]; blocked: T[] } {
    const allowed: T[] = []
    const blocked: T[] = []
    for (const tool of tools) {
      const decision = this.evaluate(toolNameOf(tool))
      if (decision.blocked) blocked.push(tool)
      else allowed.push(tool)
    }
    return { allowed, blocked }
  }
}

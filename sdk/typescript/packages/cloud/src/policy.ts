export type PolicyAction = 'block' | 'allow' | 'require_approval' | 'audit'

export interface PolicyDecision {
  blocked: boolean
  policyKind: PolicyAction
  pattern: string | null
  toolName: string
}

interface ToolLike {
  function?: { name?: string }
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

  filterTools<T extends ToolLike>(tools: T[]): { allowed: T[]; blocked: T[] } {
    const allowed: T[] = []
    const blocked: T[] = []
    for (const tool of tools) {
      const name = tool.function?.name ?? ''
      const decision = this.evaluate(name)
      if (decision.blocked) blocked.push(tool)
      else allowed.push(tool)
    }
    return { allowed, blocked }
  }
}

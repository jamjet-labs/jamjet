import { readFileSync, existsSync } from 'node:fs'
import { join } from 'node:path'
import { homedir } from 'node:os'
import { parse } from 'yaml'

export type PolicyAction = 'allow' | 'block' | 'require_approval' | 'audit'

export interface PolicyRule {
  match: string
  action: PolicyAction
  approval_timeout?: number
}

export interface PolicyBudget {
  max_tool_calls?: number
  max_usd?: number
}

export interface Policy {
  version: 1
  rules: PolicyRule[]
  budgets?: { default?: PolicyBudget }
  audit?: { destination?: string; rotate_daily?: boolean }
}

const ACTIONS: ReadonlySet<PolicyAction> = new Set<PolicyAction>([
  'allow',
  'block',
  'require_approval',
  'audit',
])

export function loadPolicy(path?: string): Policy {
  const resolved = resolvePath(path)
  if (!resolved) {
    throw new Error(
      'No policy file found. Set JAMJET_POLICY_FILE, or place policy.yaml in cwd or ~/.jamjet/',
    )
  }
  const raw = readFileSync(resolved, 'utf-8')
  const parsed = parse(raw)
  return validate(parsed)
}

function resolvePath(explicit?: string): string | null {
  if (explicit) return explicit
  if (process.env['JAMJET_POLICY_FILE']) return process.env['JAMJET_POLICY_FILE']
  const cwdCandidate = join(process.cwd(), 'policy.yaml')
  if (existsSync(cwdCandidate)) return cwdCandidate
  const homeCandidate = join(homedir(), '.jamjet', 'policy.yaml')
  if (existsSync(homeCandidate)) return homeCandidate
  return null
}

function validate(parsed: unknown): Policy {
  if (typeof parsed !== 'object' || parsed === null) {
    throw new Error('policy.yaml must be an object')
  }
  const p = parsed as Record<string, unknown>
  if (p['version'] !== 1) {
    throw new Error(`unsupported policy version: ${String(p['version'])}`)
  }
  if (!Array.isArray(p['rules'])) {
    throw new Error('policy.rules must be an array')
  }
  const rules: PolicyRule[] = (p['rules'] as unknown[]).map((r, i) => {
    if (typeof r !== 'object' || r === null) throw new Error(`rule[${i}] must be an object`)
    const rule = r as Record<string, unknown>
    if (typeof rule['match'] !== 'string') throw new Error(`rule[${i}].match must be a string`)
    if (typeof rule['action'] !== 'string' || !ACTIONS.has(rule['action'] as PolicyAction)) {
      throw new Error(`rule[${i}]: unknown action: ${String(rule['action'])}`)
    }
    const out: PolicyRule = {
      match: rule['match'],
      action: rule['action'] as PolicyAction,
    }
    if (typeof rule['approval_timeout'] === 'number') {
      out.approval_timeout = rule['approval_timeout']
    }
    return out
  })
  const result: Policy = { version: 1, rules }
  if (p['budgets'] !== undefined && p['budgets'] !== null) {
    result.budgets = p['budgets'] as NonNullable<Policy['budgets']>
  }
  if (p['audit'] !== undefined && p['audit'] !== null) {
    result.audit = p['audit'] as NonNullable<Policy['audit']>
  }
  return result
}

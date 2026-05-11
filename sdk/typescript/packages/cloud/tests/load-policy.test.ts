import { describe, it, expect } from 'vitest'
import { loadPolicy } from '../src/load-policy.js'
import { writeFileSync, mkdtempSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'

describe('loadPolicy', () => {
  it('loads a v1 policy YAML from an explicit path', () => {
    const dir = mkdtempSync(join(tmpdir(), 'jamjet-policy-'))
    const path = join(dir, 'policy.yaml')
    writeFileSync(path, `
version: 1
rules:
  - match: "*delete*"
    action: block
  - match: "payments.*"
    action: require_approval
    approval_timeout: 600
budgets:
  default:
    max_tool_calls: 20
    max_usd: 1.00
`)
    const policy = loadPolicy(path)
    expect(policy.version).toBe(1)
    expect(policy.rules).toHaveLength(2)
    expect(policy.rules[0]!.match).toBe('*delete*')
    expect(policy.rules[0]!.action).toBe('block')
    expect(policy.rules[1]!.approval_timeout).toBe(600)
    expect(policy.budgets?.default?.max_usd).toBe(1)
  })

  it('throws on unknown version', () => {
    const dir = mkdtempSync(join(tmpdir(), 'jamjet-policy-'))
    const path = join(dir, 'policy.yaml')
    writeFileSync(path, `version: 2\nrules: []`)
    expect(() => loadPolicy(path)).toThrow(/unsupported policy version/i)
  })

  it('throws on unknown action', () => {
    const dir = mkdtempSync(join(tmpdir(), 'jamjet-policy-'))
    const path = join(dir, 'policy.yaml')
    writeFileSync(path, `version: 1\nrules:\n  - { match: "*", action: maybe }`)
    expect(() => loadPolicy(path)).toThrow(/unknown action/i)
  })

  it('accepts audit action', () => {
    const dir = mkdtempSync(join(tmpdir(), 'jamjet-policy-'))
    const path = join(dir, 'policy.yaml')
    writeFileSync(path, `version: 1\nrules:\n  - { match: "slack.send_message", action: audit }`)
    const policy = loadPolicy(path)
    expect(policy.rules[0]!.action).toBe('audit')
  })
})

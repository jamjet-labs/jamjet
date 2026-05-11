import { describe, it, expect } from 'vitest'
import { PolicyEvaluator } from '../src/policy.js'
import { existsSync, readFileSync } from 'node:fs'
import { parse } from 'yaml'

const CONFORMANCE_PATH = '/Users/sunilp/Development/sunil-ws/jamjet-policy/conformance/policy-decisions.yaml'

type Case = {
  id: string
  policy: { rules: Array<{ match: string; action: 'allow' | 'block' | 'require_approval' | 'audit' }> }
  tool: string
  expect: { decision: string; rule: string | null; rule_kind: string | null }
  requires_mcp_prefix_strip?: boolean
}

describe('PolicyEvaluator against jamjet-policy conformance suite', () => {
  if (!existsSync(CONFORMANCE_PATH)) {
    it.skip('conformance file not present locally — skipping', () => {})
    return
  }

  const suite = parse(readFileSync(CONFORMANCE_PATH, 'utf-8')) as { cases: Case[] }

  for (const c of suite.cases) {
    if (c.requires_mcp_prefix_strip) continue
    it(c.id, () => {
      const ev = new PolicyEvaluator()
      for (const r of c.policy.rules) ev.add(r.action, r.match)
      const d = ev.evaluate(c.tool)
      const decision = d.blocked
        ? 'BLOCKED'
        : d.policyKind === 'require_approval'
          ? 'WAITING_FOR_APPROVAL'
          : d.policyKind === 'audit'
            ? 'AUDIT'
            : 'ALLOWED'
      expect(decision).toBe(c.expect.decision)
      expect(d.pattern ?? null).toBe(c.expect.rule)
    })
  }
})

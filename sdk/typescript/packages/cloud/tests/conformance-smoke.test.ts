import { describe, it, expect } from 'vitest'
import { PolicyEvaluator } from '../src/policy.js'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { parse } from 'yaml'

// Vendored copy of jamjet-policy's suite; see sdk/conformance/README.md.
// Read unconditionally: a missing file fails the run instead of skipping it.
const CONFORMANCE_PATH = fileURLToPath(new URL('../../../../conformance/policy-decisions.yaml', import.meta.url))

type Case = {
  id: string
  policy: { rules: Array<{ match: string; action: 'allow' | 'block' | 'require_approval' | 'audit' }> }
  tool: string
  expect: { decision: string; rule: string | null; rule_kind: string | null }
  requires_mcp_prefix_strip?: boolean
}

const suite = parse(readFileSync(CONFORMANCE_PATH, 'utf-8')) as { cases: Case[] }

describe('PolicyEvaluator against the jamjet-policy conformance suite', () => {
  it('loads the vendored cases', () => {
    expect(suite.cases.length).toBeGreaterThanOrEqual(30)
  })

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

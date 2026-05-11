import { describe, it, expect, beforeEach } from 'vitest'
import { AuditWriter } from '../src/audit-writer.js'
import { mkdtempSync, readFileSync, existsSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'

describe('AuditWriter', () => {
  let dir: string
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'jamjet-audit-'))
  })

  it('writes JSONL with schema_version=1 and daily rotation', () => {
    const writer = new AuditWriter({ destination: dir, adapter: 'claude-code-hook' })
    writer.write({
      run_id: 'run_test1',
      tool: 'database.delete_all',
      decision: 'BLOCKED',
      rule: '*delete*',
      rule_kind: 'block',
      host: 'claude-code',
      args: {},
      executed: false,
    })
    const today = new Date().toISOString().slice(0, 10)
    const path = join(dir, today, 'claude-code-hook.jsonl')
    expect(existsSync(path)).toBe(true)
    const lines = readFileSync(path, 'utf-8').trim().split('\n')
    expect(lines).toHaveLength(1)
    const event = JSON.parse(lines[0]!)
    expect(event.schema_version).toBe(1)
    expect(event.adapter).toBe('claude-code-hook')
    expect(event.decision).toBe('BLOCKED')
    expect(event.ts).toMatch(/^\d{4}-\d{2}-\d{2}T/)
    expect(event.rule).toBe('*delete*')
    expect(event.rule_kind).toBe('block')
  })

  it('appends multiple events to the same file in one day', () => {
    const writer = new AuditWriter({ destination: dir, adapter: 'mcp-shim' })
    for (let i = 0; i < 3; i++) {
      writer.write({
        run_id: `run_n${i}`,
        tool: `tool.${i}`,
        decision: 'ALLOWED',
        rule: null,
        rule_kind: null,
        host: 'claude-desktop',
        args: {},
        executed: true,
      })
    }
    const today = new Date().toISOString().slice(0, 10)
    const path = join(dir, today, 'mcp-shim.jsonl')
    const lines = readFileSync(path, 'utf-8').trim().split('\n')
    expect(lines).toHaveLength(3)
  })

  it('emits snake_case wire format for rule_kind (not camelCase)', () => {
    const writer = new AuditWriter({ destination: dir, adapter: 'ts-sdk' })
    writer.write({
      run_id: 'run_sc1',
      tool: 'payments.refund',
      decision: 'WAITING_FOR_APPROVAL',
      rule: 'payments.*',
      rule_kind: 'require_approval',
      host: 'typescript',
      args: {},
      executed: false,
    })
    const today = new Date().toISOString().slice(0, 10)
    const path = join(dir, today, 'ts-sdk.jsonl')
    const line = readFileSync(path, 'utf-8').trim()
    expect(line).toContain('"rule_kind":"require_approval"')
    expect(line).not.toContain('ruleKind')
  })

  it('honors rotateDaily=false by writing to "all" subdir', () => {
    const writer = new AuditWriter({ destination: dir, adapter: 'python-sdk', rotateDaily: false })
    writer.write({
      run_id: 'run_nd1',
      tool: 'any.tool',
      decision: 'ALLOWED',
      rule: null,
      rule_kind: null,
      host: 'python',
      args: {},
      executed: true,
    })
    expect(existsSync(join(dir, 'all', 'python-sdk.jsonl'))).toBe(true)
  })
})

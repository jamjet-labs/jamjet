import { describe, it, expect, beforeEach } from 'vitest'
import { ApprovalQueue } from '../src/approval-queue.js'
import { mkdtempSync, existsSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'

describe('ApprovalQueue', () => {
  let dir: string
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'jamjet-approval-'))
  })

  it('enqueues a pending approval and writes a queue file', () => {
    const queue = new ApprovalQueue({ pendingDir: dir, defaultTimeoutMs: 1_000 })
    const runId = queue.enqueue({
      tool: 'payments.refund',
      args: { customer_id: 'c1' },
      adapter: 'mcp-shim',
    })
    expect(runId).toMatch(/^run_[a-z0-9]+$/)
    const path = join(dir, `${runId}.json`)
    expect(existsSync(path)).toBe(true)
    const data = JSON.parse(readFileSync(path, 'utf-8'))
    expect(data.status).toBe('pending')
    expect(data.tool).toBe('payments.refund')
  })

  it('resolves on approve and removes the queue file', async () => {
    const queue = new ApprovalQueue({ pendingDir: dir, defaultTimeoutMs: 5_000 })
    const runId = queue.enqueue({
      tool: 'payments.refund',
      args: {},
      adapter: 'mcp-shim',
    })
    const pending = queue.wait(runId)
    queue.approve(runId)
    const result = await pending
    expect(result.status).toBe('approved')
    expect(existsSync(join(dir, `${runId}.json`))).toBe(false)
  })

  it('auto-rejects on timeout', async () => {
    const queue = new ApprovalQueue({ pendingDir: dir, defaultTimeoutMs: 50 })
    const runId = queue.enqueue({
      tool: 'payments.refund',
      args: {},
      adapter: 'mcp-shim',
    })
    const result = await queue.wait(runId)
    expect(result.status).toBe('rejected')
    expect(result.reason).toBe('timeout')
  })

  it('lists pending approvals', () => {
    const queue = new ApprovalQueue({ pendingDir: dir, defaultTimeoutMs: 60_000 })
    queue.enqueue({ tool: 't1', args: {}, adapter: 'mcp-shim' })
    queue.enqueue({ tool: 't2', args: {}, adapter: 'claude-code-hook' })
    const list = queue.list()
    expect(list).toHaveLength(2)
    expect(list.map((p) => p.tool).sort()).toEqual(['t1', 't2'])
  })
})

import { describe, expect, it } from 'vitest'
import {
  JamjetApprovalRejected,
  JamjetApprovalTimeout,
  JamjetBudgetExceeded,
  JamjetPolicyBlocked,
} from '../src/errors.js'

describe('JamjetBudgetExceeded', () => {
  it('carries spent and limit', () => {
    const err = new JamjetBudgetExceeded(42.5, 50)
    expect(err).toBeInstanceOf(Error)
    expect(err.name).toBe('JamjetBudgetExceeded')
    expect(err.spent).toBe(42.5)
    expect(err.limit).toBe(50)
    expect(err.message).toMatch(/42\.5/)
    expect(err.message).toMatch(/50/)
  })
})

describe('JamjetPolicyBlocked', () => {
  it('carries toolName and pattern, attaches cause', () => {
    const toolCall = { id: 'tc_1', function: { name: 'wire_money', arguments: '{}' } }
    const err = new JamjetPolicyBlocked('wire_money', 'wire_*', { cause: toolCall })
    expect(err.name).toBe('JamjetPolicyBlocked')
    expect(err.toolName).toBe('wire_money')
    expect(err.pattern).toBe('wire_*')
    expect(err.cause).toBe(toolCall)
  })
})

describe('JamjetApprovalRejected', () => {
  it('carries approvalId and optional reason', () => {
    const err = new JamjetApprovalRejected('apr_123', 'too risky')
    expect(err.name).toBe('JamjetApprovalRejected')
    expect(err.approvalId).toBe('apr_123')
    expect(err.reason).toBe('too risky')
  })

  it('reason is optional', () => {
    const err = new JamjetApprovalRejected('apr_123')
    expect(err.reason).toBeUndefined()
  })
})

describe('JamjetApprovalTimeout', () => {
  it('carries approvalId and timeoutMs', () => {
    const err = new JamjetApprovalTimeout('apr_123', 60_000)
    expect(err.name).toBe('JamjetApprovalTimeout')
    expect(err.approvalId).toBe('apr_123')
    expect(err.timeoutMs).toBe(60_000)
  })

  it('supports cause: server_error marker', () => {
    const err = new JamjetApprovalTimeout('apr_123', 60_000, { cause: 'server_error' })
    expect(err.cause).toBe('server_error')
  })
})

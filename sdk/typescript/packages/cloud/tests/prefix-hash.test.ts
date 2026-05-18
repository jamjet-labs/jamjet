import { describe, expect, test } from 'vitest'
import type { MessageParam } from '@anthropic-ai/sdk/resources/messages'
import { computePrefixHash } from '../src/prefix-hash.js'

describe('computePrefixHash', () => {
  test('hashes a plain string deterministically', () => {
    // Total length 100 chars after normalization; first 80 chars are hashed.
    const input =
      'You are a careful assistant. Always answer in plain English and avoid speculation. Keep it short.'
    const hash = computePrefixHash(input)
    expect(hash).toMatch(/^[0-9a-f]{16}$/)
    expect(hash).toBe('66b60ecb411e5f93')
  })

  test('hashes MessageParam[] with system + user roles', () => {
    const input: MessageParam[] = [
      {
        role: 'user',
        content: [
          { type: 'text', text: 'System: You are an expert SQL reviewer for Postgres.' },
        ],
      },
      {
        role: 'assistant',
        content: 'Understood. Share the query and the schema.',
      },
      {
        role: 'user',
        content: [
          { type: 'text', text: 'Please review this query: SELECT * FROM accounts WHERE id = 42;' },
          // Non-text block should be skipped entirely.
          {
            type: 'image',
            source: { type: 'base64', media_type: 'image/png', data: 'AAAA' },
          },
        ],
      },
    ]
    const hash = computePrefixHash(input)
    expect(hash).toMatch(/^[0-9a-f]{16}$/)
    expect(hash).toBe('c59b56d52d72c39e')
  })

  test('two inputs differing only in the last 10% hash the same', () => {
    // Build a long input so that the last 10% is well within the dropped 20% tail.
    const prefix = 'a'.repeat(900)
    const a = prefix + 'b'.repeat(100) // total 1000; last 10% = "bbb...b"
    const c = prefix + 'c'.repeat(100) // same prefix, differs in last 10%
    const hashA = computePrefixHash(a)
    const hashC = computePrefixHash(c)
    expect(hashA).toBe(hashC)
    expect(hashA).toBe('0868513521f77faa')
  })

  test('two inputs differing in the first 30% hash differently', () => {
    const tail = 'z'.repeat(700)
    const a = 'a'.repeat(300) + tail // 1000 chars
    const b = 'b'.repeat(300) + tail // 1000 chars
    const hashA = computePrefixHash(a)
    const hashB = computePrefixHash(b)
    expect(hashA).not.toBe(hashB)
    expect(hashA).toBe('a2985aaaf7f42949')
    expect(hashB).toBe('b01f11ba300532df')
  })

  test('handles empty string without throwing', () => {
    const hash = computePrefixHash('')
    expect(hash).toMatch(/^[0-9a-f]{16}$/)
    // SHA-256("") truncated to 16 hex chars.
    expect(hash).toBe('e3b0c44298fc1c14')
  })

  test('handles empty MessageParam[] without throwing', () => {
    const hash = computePrefixHash([])
    expect(hash).toMatch(/^[0-9a-f]{16}$/)
    // Same sentinel as empty string — degenerate input maps to SHA-256("").
    expect(hash).toBe('e3b0c44298fc1c14')
  })

  test('normalizes case and whitespace', () => {
    // These should hash to the same value after normalization.
    const a = 'Hello   World\n\tFoo Bar'
    const b = 'hello world foo bar'
    expect(computePrefixHash(a)).toBe(computePrefixHash(b))
  })
})

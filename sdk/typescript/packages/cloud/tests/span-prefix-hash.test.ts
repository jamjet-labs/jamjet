import { describe, expect, test } from 'vitest'
import { Span } from '../src/span.js'

describe('Span.prompt_prefix_hash', () => {
  test('setPromptPrefixHash(string) surfaces in toEventDict()', () => {
    const span = new Span({ traceId: 't1', spanId: 's1', kind: 'llm_call', name: 'anthropic.claude' })
    span.setPromptPrefixHash('abc1234567890def')
    span.finish('ok', 100)
    const dict = span.toEventDict()
    expect(dict.prompt_prefix_hash).toBe('abc1234567890def')
  })

  test('omits prompt_prefix_hash from toEventDict() when never set', () => {
    const span = new Span({ traceId: 't1', spanId: 's1', kind: 'llm_call', name: 'anthropic.claude' })
    span.finish('ok', 100)
    const dict = span.toEventDict()
    expect(dict).not.toHaveProperty('prompt_prefix_hash')
  })

  test('setPromptPrefixHash(null) leaves field absent', () => {
    const span = new Span({ traceId: 't1', spanId: 's1', kind: 'llm_call', name: 'anthropic.claude' })
    span.setPromptPrefixHash(null)
    span.finish('ok', 100)
    const dict = span.toEventDict()
    expect(dict).not.toHaveProperty('prompt_prefix_hash')
  })

  test('setPromptPrefixHash(null) after a real hash clears it', () => {
    const span = new Span({ traceId: 't1', spanId: 's1', kind: 'llm_call', name: 'anthropic.claude' })
    span.setPromptPrefixHash('abc1234567890def')
    span.setPromptPrefixHash(null)
    span.finish('ok', 100)
    const dict = span.toEventDict()
    expect(dict).not.toHaveProperty('prompt_prefix_hash')
  })
})

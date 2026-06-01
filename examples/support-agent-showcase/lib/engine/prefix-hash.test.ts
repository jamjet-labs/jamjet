import { test, expect } from 'vitest'
import { computePrefixHash } from './prefix-hash.js'
test('stable 16-hex; identical 80% header groups; empty is sentinel; different prompt differs', () => {
  const a = computePrefixHash('SYSTEM: big knowledge base lorem ipsum dolor sit amet consectetur\nuser question one')
  const b = computePrefixHash('SYSTEM: big knowledge base lorem ipsum dolor sit amet consectetur\nuser question two')
  expect(a).toMatch(/^[0-9a-f]{16}$/)
  expect(a).toBe(b)                                        // first 80% identical -> same hash
  expect(computePrefixHash('')).toBe('e3b0c44298fc1c14')  // empty/sentinel
  expect(computePrefixHash([])).toBe('e3b0c44298fc1c14')  // [] also sentinel
  expect(computePrefixHash('a completely different prompt entirely with other words')).not.toBe(a)
})
test('message-array input extracts text blocks and string content', () => {
  const fromString = computePrefixHash('hello world this is a fairly long shared header for hashing purposes')
  const fromMsgs = computePrefixHash([{ role: 'user', content: 'hello world this is a fairly long shared header for hashing purposes' }])
  expect(fromMsgs).toBe(fromString)
})

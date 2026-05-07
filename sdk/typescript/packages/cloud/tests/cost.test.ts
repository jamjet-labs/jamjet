import { describe, expect, test } from 'vitest'
import { estimateCost } from '../src/cost.js'

describe('estimateCost', () => {
  test('gpt-4o exact lookup', () => {
    expect(estimateCost('gpt-4o', 100, 50)).toBeCloseTo(0.00075, 8)
  })

  test('claude-sonnet-4-6 exact lookup', () => {
    expect(estimateCost('claude-sonnet-4-6', 1000, 500)).toBeCloseTo(0.0105, 8)
  })

  test('unknown model uses fallback rate', () => {
    expect(estimateCost('mystery-model', 100, 50)).toBeCloseTo(0.00105, 8)
  })

  test('zero tokens returns 0', () => {
    expect(estimateCost('gpt-4o', 0, 0)).toBe(0)
  })
})

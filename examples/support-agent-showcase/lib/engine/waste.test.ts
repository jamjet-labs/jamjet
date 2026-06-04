import { test, expect } from 'vitest'
import { WasteTracker } from './waste.js'
import { cacheReadSavingsCents } from './savings.js'
test('repeated prefix accrues avoidable re-paid tokens; distinct prefixes do not group', () => {
  const t = new WasteTracker('claude-sonnet-4-6')
  const h = 'a1b2c3d4e5f6a7b8'
  for (let i = 0; i < 5; i++) t.record(h, 4000)          // same prefix 5x, 4000 input tok each
  t.record('different00000000', 4000)                     // a distinct prefix, once
  const w = t.detect()
  expect(w.length).toBe(1)                                // only the repeated one is waste
  expect(w[0].prefixHash).toBe(h)
  expect(w[0].repeats).toBe(5)
  expect(w[0].rePaidTokens).toBe(16000)                  // (5-1)*4000 avoidable
  expect(w[0].wastedCents).toBeGreaterThan(0)
})
test('empty sentinel hash is never reported as waste', () => {
  const t = new WasteTracker('claude-sonnet-4-6')
  t.record('e3b0c44298fc1c14', 4000); t.record('e3b0c44298fc1c14', 4000)
  expect(t.detect()).toEqual([])
})
test('cache savings are positive and scale with cache-read tokens', () => {
  expect(cacheReadSavingsCents('claude-sonnet-4-6', 0)).toBe(0)
  expect(cacheReadSavingsCents('claude-sonnet-4-6', 12000)).toBeGreaterThan(0)
  expect(cacheReadSavingsCents('claude-sonnet-4-6', 24000)).toBeGreaterThan(cacheReadSavingsCents('claude-sonnet-4-6', 12000))
})

import { test, expect } from 'vitest'

test('jamjet cloud public API is importable', async () => {
  const cloud = await import('@jamjet/cloud')
  expect(typeof cloud.applyCacheInject).toBe('function')
  expect(typeof cloud.estimateCost).toBe('function')
  expect(typeof cloud.redact).toBe('function')
  const node = await import('@jamjet/cloud/node')
  expect(typeof node.AuditWriter).toBe('function')
})

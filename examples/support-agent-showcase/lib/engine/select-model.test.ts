import { test, expect, beforeEach, afterEach } from 'vitest'
import { selectModel } from './select-model.js'
const saved = { ...process.env }
beforeEach(() => { delete process.env.ANTHROPIC_API_KEY })
afterEach(() => { process.env = { ...saved } })
test('picks mock when no anthropic key', () => {
  expect(selectModel().name).toContain('mock')   // returns the mock fn
})
test('picks live when ANTHROPIC_API_KEY is set', () => {
  process.env.ANTHROPIC_API_KEY = 'sk-test'
  expect(selectModel().name).toContain('live')
})

import { afterEach, beforeEach, describe, expect, test } from 'vitest'
import { resolveConfig, ConfigError } from '../src/config.js'

describe('resolveConfig', () => {
  const original = process.env

  beforeEach(() => {
    process.env = { ...original }
    delete process.env.JAMJET_API_KEY
    delete process.env.JAMJET_API_URL
  })

  afterEach(() => {
    process.env = original
  })

  test('explicit options take priority', () => {
    process.env.JAMJET_API_KEY = 'env-key'
    const cfg = resolveConfig({ apiKey: 'explicit-key', project: 'p' })
    expect(cfg.apiKey).toBe('explicit-key')
    expect(cfg.project).toBe('p')
  })

  test('falls back to JAMJET_API_KEY env var', () => {
    process.env.JAMJET_API_KEY = 'env-key'
    const cfg = resolveConfig({ project: 'p' })
    expect(cfg.apiKey).toBe('env-key')
  })

  test('throws when apiKey is absent', () => {
    expect(() => resolveConfig({ project: 'p' })).toThrow(ConfigError)
    expect(() => resolveConfig({ project: 'p' })).toThrow(/apiKey/i)
  })

  test('default apiUrl is api.jamjet.dev', () => {
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.apiUrl).toBe('https://api.jamjet.dev')
  })

  test('JAMJET_API_URL overrides default', () => {
    process.env.JAMJET_API_URL = 'http://localhost:8080'
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.apiUrl).toBe('http://localhost:8080')
  })

  test('default sampling is 1.0 with errors and approvals always kept', () => {
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.sampling.rate).toBe(1.0)
    expect(cfg.sampling.alwaysKeepErrors).toBe(true)
    expect(cfg.sampling.alwaysKeepApprovals).toBe(true)
  })

  test('default redaction is "standard"', () => {
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.redaction.mode).toBe('standard')
  })

  test('default debug is false', () => {
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.debug).toBe(false)
  })

  test('default agent is "default"', () => {
    const cfg = resolveConfig({ apiKey: 'k', project: 'p' })
    expect(cfg.agent).toBe('default')
  })

  test('rejects empty project', () => {
    expect(() => resolveConfig({ apiKey: 'k', project: '' })).toThrow(ConfigError)
  })

  test('rejects sampling rate outside 0-1', () => {
    expect(() => resolveConfig({ apiKey: 'k', project: 'p', sampling: { rate: 1.5 } }))
      .toThrow(ConfigError)
  })
})

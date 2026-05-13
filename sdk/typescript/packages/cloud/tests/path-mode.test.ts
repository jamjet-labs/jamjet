import { describe, it, expect, beforeEach, afterEach } from 'vitest'
import { detectPathMode } from '../src/path-mode.js'

const SERVERLESS_VARS = [
  'VERCEL',
  'CF_PAGES',
  'AWS_LAMBDA_FUNCTION_NAME',
  'GITHUB_ACTIONS',
  'NETLIFY',
] as const

describe('detectPathMode', () => {
  const originalEnv = { ...process.env }

  beforeEach(() => {
    delete process.env.JAMJET_CLOUD_TOKEN
    delete process.env.JAMJET_CLOUD_MODE
    for (const v of SERVERLESS_VARS) delete process.env[v]
  })

  afterEach(() => {
    process.env = { ...originalEnv }
  })

  it('local-only when no token (path B disabled by default)', () => {
    expect(detectPathMode()).toBe('local-only')
  })

  it('local-only when token but no serverless and no explicit mode', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    expect(detectPathMode()).toBe('local-only')
  })

  it('direct when token + explicit JAMJET_CLOUD_MODE=direct', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    process.env.JAMJET_CLOUD_MODE = 'direct'
    expect(detectPathMode()).toBe('direct')
  })

  it('local-only when token + explicit JAMJET_CLOUD_MODE=daemon', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    process.env.JAMJET_CLOUD_MODE = 'daemon'
    expect(detectPathMode()).toBe('local-only')
  })

  for (const serverlessVar of SERVERLESS_VARS) {
    it(`direct when token + ${serverlessVar} is set`, () => {
      process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
      process.env[serverlessVar] = '1'
      expect(detectPathMode()).toBe('direct')
    })
  }

  it('explicit daemon mode wins over serverless heuristic', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    process.env.VERCEL = '1'
    process.env.JAMJET_CLOUD_MODE = 'daemon'
    expect(detectPathMode()).toBe('local-only')
  })

  it('explicit direct mode wins over no-serverless default', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    process.env.JAMJET_CLOUD_MODE = 'direct'
    expect(detectPathMode()).toBe('direct')
  })

  it('unknown JAMJET_CLOUD_MODE values fall back to heuristic', () => {
    process.env.JAMJET_CLOUD_TOKEN = 'jj_test_key'
    process.env.JAMJET_CLOUD_MODE = 'bogus'
    expect(detectPathMode()).toBe('local-only')

    process.env.VERCEL = '1'
    expect(detectPathMode()).toBe('direct')
  })

  it('no token + serverless still returns local-only (token is required)', () => {
    process.env.VERCEL = '1'
    expect(detectPathMode()).toBe('local-only')
  })
})

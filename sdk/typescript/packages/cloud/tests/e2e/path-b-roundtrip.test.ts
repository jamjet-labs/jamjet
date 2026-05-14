// E2E Path B roundtrip against a real Cloud deployment.
//
// Skipped by default. To run against api.jamjet.dev (or a preview):
//
//   1. `jamjet cloud link` to get an api key for a test/preview project.
//   2. Export env:
//        JAMJET_E2E_API_BASE=https://api.jamjet.dev
//        JAMJET_E2E_API_KEY=jj_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
//   3. pnpm --filter @jamjet/cloud test -- path-b-roundtrip
//
// What this verifies end-to-end:
//   - detectPathMode() picks "direct" given a JAMJET_CLOUD_TOKEN + serverless
//     heuristic (VERCEL=1).
//   - A live POST to /v1/policy-audit/events from CloudPusher returns 2xx.
//   - The same event re-pushed returns 2xx too (Cloud R5 dedup; duplicates
//     count without errors).
//   - readTraceparent picks up a header source.
//
// Because we set process.env on Node, the test serializes per fixture and
// restores env in afterEach.
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
} from 'vitest'
import {
  CloudPusher,
  detectPathMode,
  readTraceparent,
  type CloudPusherEvent,
} from '../../src/index.js'

const PREVIEW_URL = process.env.JAMJET_E2E_API_BASE
const PREVIEW_KEY = process.env.JAMJET_E2E_API_KEY

const haveEnv = !!(PREVIEW_URL && PREVIEW_KEY)
const describeOrSkip = haveEnv ? describe : describe.skip

describeOrSkip('Path B direct-push roundtrip (e2e)', () => {
  const savedEnv = { ...process.env }

  beforeEach(() => {
    process.env.JAMJET_CLOUD_TOKEN = PREVIEW_KEY!
    process.env.JAMJET_API_BASE = PREVIEW_URL!
    process.env.VERCEL = '1'
  })

  afterEach(() => {
    process.env = { ...savedEnv }
  })

  it('detectPathMode resolves to "direct" in simulated serverless env', () => {
    expect(detectPathMode()).toBe('direct')
  })

  it('CloudPusher posts an event and returns true (200 OK)', async () => {
    const pusher = new CloudPusher({
      apiBase: PREVIEW_URL!,
      apiKey: PREVIEW_KEY!,
    })
    const event: CloudPusherEvent = {
      ts: new Date().toISOString(),
      run_id: `run_e2e${Date.now().toString(36)}`,
      adapter: 'openai-guardrail',
      host: 'openai-agents-sdk',
      tool: 'e2e.path-b',
      decision: 'BLOCKED',
      executed: false,
      schema_version: 1,
      args: { redacted: true },
      args_redaction: 'full',
    }
    const ok = await pusher.push(event)
    expect(ok, 'first push must succeed (state=ok)').toBe(true)
    expect(pusher.consecutiveFailures).toBe(0)

    // Re-push the same event: Cloud R5 dedup (project_id, run_id, ts, decision)
    // makes this idempotent. Still 200 OK; CloudPusher counts it as success.
    const okAgain = await pusher.push(event)
    expect(okAgain, 'second push of the same event must succeed (dedup ok)').toBe(true)
  }, 15_000)

  it('readTraceparent picks up a header source', () => {
    const headers = {
      traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
    }
    const t = readTraceparent({ headers })
    expect(t?.trace_id).toBe('0af7651916cd43dd8448eb211c80319c')
  })

  it('CloudPusher gracefully degrades (no throw, false) on bogus key', async () => {
    const pusher = new CloudPusher({
      apiBase: PREVIEW_URL!,
      apiKey: 'jj_definitely_not_a_real_key',
      timeoutMs: 2000,
    })
    const event: CloudPusherEvent = {
      ts: new Date().toISOString(),
      run_id: `run_e2e_bogus${Date.now().toString(36)}`,
      adapter: 'openai-guardrail',
      host: 'openai-agents-sdk',
      tool: 'e2e.path-b',
      decision: 'BLOCKED',
      executed: false,
      schema_version: 1,
      args: { redacted: true },
      args_redaction: 'full',
    }
    const ok = await pusher.push(event)
    expect(ok).toBe(false)
    expect(pusher.consecutiveFailures).toBeGreaterThan(0)
  }, 10_000)
})

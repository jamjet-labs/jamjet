import { describe, it, expect, afterEach } from 'vitest'
import {
  createServer,
  type Server,
  type IncomingMessage,
  type ServerResponse,
} from 'node:http'
import type { AddressInfo } from 'node:net'
import { CloudPusher, type CloudPusherEvent } from '../src/cloud-pusher.js'

function startMockServer(
  handler: (req: IncomingMessage, res: ServerResponse) => void,
): Promise<{ server: Server; port: number }> {
  return new Promise((resolve) => {
    const server = createServer(handler)
    server.listen(0, '127.0.0.1', () => {
      const port = (server.address() as AddressInfo).port
      resolve({ server, port })
    })
  })
}

const sampleEvent = (rid = 'run_a'): CloudPusherEvent => ({
  ts: '2026-05-12T00:00:00.000Z',
  run_id: rid,
  adapter: 'openai-guardrail',
  host: 'openai-agents-sdk',
  tool: 'x.y',
  decision: 'BLOCKED',
  executed: false,
  schema_version: 1,
  args: { redacted: true },
  args_redaction: 'full',
})

describe('CloudPusher', () => {
  let server: Server | undefined

  afterEach(async () => {
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()))
      server = undefined
    }
  })

  it('pushes an event and returns true on 2xx', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      res.writeHead(200)
      res.end('{}')
    }))
    const pusher = new CloudPusher({ apiBase: `http://127.0.0.1:${port}`, apiKey: 'jj_test' })
    expect(await pusher.push(sampleEvent())).toBe(true)
    expect(pusher.consecutiveFailures).toBe(0)
  })

  it('tags request body with path=direct', async () => {
    let bodyCaptured = ''
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        bodyCaptured = Buffer.concat(chunks).toString('utf-8')
        expect(req.headers.authorization).toBe('Bearer jj_test')
        res.writeHead(200)
        res.end('{}')
      })
    }))
    const pusher = new CloudPusher({ apiBase: `http://127.0.0.1:${port}`, apiKey: 'jj_test' })
    await pusher.push(sampleEvent())
    const parsed = JSON.parse(bodyCaptured)
    expect(parsed.path).toBe('direct')
    expect(parsed.events).toHaveLength(1)
    expect(parsed.events[0].run_id).toBe('run_a')
  })

  it('returns false on 5xx — fire-and-forget never throws', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      res.writeHead(503)
      res.end()
    }))
    const pusher = new CloudPusher({ apiBase: `http://127.0.0.1:${port}`, apiKey: 'jj_test' })
    expect(await pusher.push(sampleEvent())).toBe(false)
    expect(pusher.consecutiveFailures).toBe(1)
  })

  it('returns false on 4xx (drop semantics; no retry from direct-push)', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      res.writeHead(400)
      res.end()
    }))
    const pusher = new CloudPusher({ apiBase: `http://127.0.0.1:${port}`, apiKey: 'jj_test' })
    expect(await pusher.push(sampleEvent())).toBe(false)
  })

  it('honors the timeout (default 500ms)', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      // Never respond — server keeps the socket open.
      setTimeout(() => res.end(), 5000)
    }))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      timeoutMs: 200,
    })
    const start = Date.now()
    expect(await pusher.push(sampleEvent())).toBe(false)
    const elapsed = Date.now() - start
    expect(elapsed).toBeLessThan(1000)
    expect(elapsed).toBeGreaterThanOrEqual(150)
  })

  it('never throws on completely unreachable apiBase', async () => {
    // No server started → connection refused.
    const pusher = new CloudPusher({
      apiBase: 'http://127.0.0.1:1',
      apiKey: 'jj_test',
      timeoutMs: 200,
    })
    expect(await pusher.push(sampleEvent())).toBe(false)
  })

  it('circuit breaker opens after threshold consecutive failures', async () => {
    let port: number
    let calls = 0
    ;({ server, port } = await startMockServer((_req, res) => {
      calls++
      res.writeHead(503)
      res.end()
    }))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      circuitBreakerThreshold: 5,
      circuitBreakerResetMs: 60_000,
    })
    for (let i = 0; i < 5; i++) await pusher.push(sampleEvent())
    expect(pusher.isCircuitOpen()).toBe(true)
    expect(calls).toBe(5)

    // The 6th push must short-circuit — no actual HTTP attempt.
    await pusher.push(sampleEvent())
    expect(calls).toBe(5)
  })

  it('circuit breaker auto-resets after the reset window', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      res.writeHead(503)
      res.end()
    }))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      circuitBreakerThreshold: 2,
      circuitBreakerResetMs: 50,
    })
    for (let i = 0; i < 2; i++) await pusher.push(sampleEvent())
    expect(pusher.isCircuitOpen()).toBe(true)
    await new Promise((r) => setTimeout(r, 100))
    expect(pusher.isCircuitOpen()).toBe(false)
  })

  it('successful push resets the failure counter', async () => {
    let port: number
    let n = 0
    ;({ server, port } = await startMockServer((_req, res) => {
      n++
      if (n <= 3) {
        res.writeHead(503)
        res.end()
      } else {
        res.writeHead(200)
        res.end('{}')
      }
    }))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      circuitBreakerThreshold: 5,
      circuitBreakerResetMs: 60_000,
    })
    for (let i = 0; i < 3; i++) await pusher.push(sampleEvent())
    expect(pusher.consecutiveFailures).toBe(3)
    await pusher.push(sampleEvent())
    expect(pusher.consecutiveFailures).toBe(0)
    expect(pusher.isCircuitOpen()).toBe(false)
  })

  it('sends a configurable user-agent header', async () => {
    let port: number
    let ua = ''
    ;({ server, port } = await startMockServer((req, res) => {
      ua = (req.headers['user-agent'] as string) ?? ''
      res.writeHead(200)
      res.end('{}')
    }))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      userAgent: 'my-adapter/1.0',
    })
    await pusher.push(sampleEvent())
    expect(ua).toBe('my-adapter/1.0')
  })
})

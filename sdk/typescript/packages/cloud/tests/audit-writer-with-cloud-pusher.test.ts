import { describe, it, expect, afterEach } from 'vitest'
import {
  createServer,
  type Server,
  type IncomingMessage,
  type ServerResponse,
} from 'node:http'
import type { AddressInfo } from 'node:net'
import { mkdtempSync, rmSync, readdirSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { AuditWriter, type AuditEventInput } from '../src/audit-writer.js'
import { CloudPusher } from '../src/cloud-pusher.js'

const wait = (ms = 50) => new Promise((r) => setTimeout(r, ms))

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

const sampleEvent: AuditEventInput = {
  run_id: 'run_a',
  host: 'openai-agents-sdk',
  tool: 'payments.refund',
  args: { customer_id: 'cus_123' },
  decision: 'BLOCKED',
  rule: '*.delete',
  rule_kind: 'block',
  executed: false,
}

describe('AuditWriter + CloudPusher integration', () => {
  let server: Server | undefined
  let tmpDir: string

  afterEach(async () => {
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()))
      server = undefined
    }
    if (tmpDir) rmSync(tmpDir, { recursive: true, force: true })
  })

  it('writes locally AND pushes to Cloud when pusher provided', async () => {
    let pushed: unknown = null
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        pushed = JSON.parse(Buffer.concat(chunks).toString('utf-8'))
        res.writeHead(200)
        res.end('{}')
      })
    }))

    tmpDir = mkdtempSync(join(tmpdir(), 'aw-cp-'))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
    })
    const writer = new AuditWriter({
      destination: tmpDir,
      adapter: 'openai-guardrail',
      cloudPusher: pusher,
    })

    writer.write(sampleEvent)

    // Wait for the async pusher.push() to land.
    await wait(200)

    // Local JSONL written.
    const date = new Date().toISOString().slice(0, 10)
    const files = readdirSync(join(tmpDir, date))
    expect(files).toContain('openai-guardrail.jsonl')
    const line = readFileSync(join(tmpDir, date, 'openai-guardrail.jsonl'), 'utf-8')
    expect(JSON.parse(line.trim()).run_id).toBe('run_a')

    // Cloud received the same event with path=direct.
    expect(pushed).toMatchObject({
      path: 'direct',
      events: [{ run_id: 'run_a', adapter: 'openai-guardrail' }],
    })
  })

  it('write() does not throw if Cloud is unreachable', async () => {
    tmpDir = mkdtempSync(join(tmpdir(), 'aw-cp-'))
    // No server started → connect refused.
    const pusher = new CloudPusher({
      apiBase: 'http://127.0.0.1:1',
      apiKey: 'jj_test',
      timeoutMs: 100,
    })
    const writer = new AuditWriter({
      destination: tmpDir,
      adapter: 'openai-guardrail',
      cloudPusher: pusher,
    })

    expect(() => writer.write(sampleEvent)).not.toThrow()
    // Give the async push a chance to settle.
    await wait(200)

    // Local write succeeded despite Cloud being down.
    const date = new Date().toISOString().slice(0, 10)
    const files = readdirSync(join(tmpDir, date))
    expect(files).toContain('openai-guardrail.jsonl')
  })

  it('skips push when cloudPusher is null (Path A only)', async () => {
    tmpDir = mkdtempSync(join(tmpdir(), 'aw-cp-'))
    const writer = new AuditWriter({
      destination: tmpDir,
      adapter: 'openai-guardrail',
      cloudPusher: null,
    })

    writer.write(sampleEvent)

    const date = new Date().toISOString().slice(0, 10)
    const files = readdirSync(join(tmpDir, date))
    expect(files).toContain('openai-guardrail.jsonl')
  })

  it('write() does not block on a slow Cloud — synchronous return', async () => {
    let port: number
    ;({ server, port } = await startMockServer((_req, res) => {
      // Take 3s to respond.
      setTimeout(() => {
        res.writeHead(200)
        res.end('{}')
      }, 3000)
    }))

    tmpDir = mkdtempSync(join(tmpdir(), 'aw-cp-'))
    const pusher = new CloudPusher({
      apiBase: `http://127.0.0.1:${port}`,
      apiKey: 'jj_test',
      timeoutMs: 200,
    })
    const writer = new AuditWriter({
      destination: tmpDir,
      adapter: 'openai-guardrail',
      cloudPusher: pusher,
    })

    const start = Date.now()
    writer.write(sampleEvent)
    const elapsed = Date.now() - start
    // Synchronous write() must not wait on the HTTP push.
    expect(elapsed).toBeLessThan(50)
  })
})

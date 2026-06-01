import { describe, it, expect, afterEach } from 'vitest'
import {
  createServer,
  type Server,
  type IncomingMessage,
  type ServerResponse,
} from 'node:http'
import type { AddressInfo } from 'node:net'
import { recordOutcome } from '../src/outcomes.js'
import type { Outcome } from '../src/outcomes.js'

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

describe('recordOutcome', () => {
  let server: Server | undefined

  afterEach(async () => {
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()))
      server = undefined
    }
  })

  it('POSTs to /v1/outcomes with bearer auth and correct body', async () => {
    let capturedMethod = ''
    let capturedPath = ''
    let capturedAuth = ''
    let capturedBody = ''
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      capturedMethod = req.method ?? ''
      capturedPath = req.url ?? ''
      capturedAuth = (req.headers.authorization as string) ?? ''
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        capturedBody = Buffer.concat(chunks).toString('utf-8')
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ recorded: true }))
      })
    }))

    await recordOutcome('jj_key', `http://127.0.0.1:${port}`, 'trace_abc', 'success')

    expect(capturedMethod).toBe('POST')
    expect(capturedPath).toBe('/v1/outcomes')
    expect(capturedAuth).toBe('Bearer jj_key')
    const body = JSON.parse(capturedBody)
    expect(body).toEqual({ trace_id: 'trace_abc', outcome: 'success' })
  })

  it('includes optional score and metadata in the body', async () => {
    let capturedBody = ''
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        capturedBody = Buffer.concat(chunks).toString('utf-8')
        res.writeHead(200)
        res.end(JSON.stringify({ recorded: true }))
      })
    }))

    await recordOutcome('jj_key', `http://127.0.0.1:${port}`, 'trace_xyz', 'failure', {
      score: 0.42,
      metadata: { reason: 'timeout', retries: 3 },
    })

    const body = JSON.parse(capturedBody)
    expect(body.trace_id).toBe('trace_xyz')
    expect(body.outcome).toBe('failure')
    expect(body.score).toBe(0.42)
    expect(body.metadata).toEqual({ reason: 'timeout', retries: 3 })
  })

  it.each([
    'success',
    'failure',
    'approved',
    'rejected',
    'resolved',
    'unresolved',
  ] satisfies Outcome[])('accepts valid outcome "%s"', async (outcome) => {
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        res.writeHead(200)
        res.end(JSON.stringify({ recorded: true }))
      })
    }))

    await expect(
      recordOutcome('k', `http://127.0.0.1:${port}`, 't1', outcome),
    ).resolves.toBeUndefined()
  })

  it('throws TypeError for an invalid outcome string', async () => {
    await expect(
      recordOutcome('k', 'http://localhost', 't1', 'wrong' as Outcome),
    ).rejects.toThrow(TypeError)
    await expect(
      recordOutcome('k', 'http://localhost', 't1', 'wrong' as Outcome),
    ).rejects.toThrow(/Invalid outcome "wrong"/)
  })

  it('throws RangeError when score is below 0', async () => {
    await expect(
      recordOutcome('k', 'http://localhost', 't1', 'success', { score: -0.1 }),
    ).rejects.toThrow(RangeError)
    await expect(
      recordOutcome('k', 'http://localhost', 't1', 'success', { score: -0.1 }),
    ).rejects.toThrow(/score must be a number between 0 and 1/)
  })

  it('throws RangeError when score is above 1', async () => {
    await expect(
      recordOutcome('k', 'http://localhost', 't1', 'success', { score: 1.01 }),
    ).rejects.toThrow(RangeError)
  })

  it('accepts score boundary values 0 and 1', async () => {
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        res.writeHead(200)
        res.end(JSON.stringify({ recorded: true }))
      })
    }))

    await expect(
      recordOutcome('k', `http://127.0.0.1:${port}`, 't1', 'success', { score: 0 }),
    ).resolves.toBeUndefined()
    await expect(
      recordOutcome('k', `http://127.0.0.1:${port}`, 't1', 'success', { score: 1 }),
    ).resolves.toBeUndefined()
  })

  it('throws when server returns a non-2xx status', async () => {
    let port: number
    ;({ server, port } = await startMockServer((req, res) => {
      const chunks: Buffer[] = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        res.writeHead(422)
        res.end()
      })
    }))

    await expect(
      recordOutcome('k', `http://127.0.0.1:${port}`, 't1', 'success'),
    ).rejects.toThrow(/recordOutcome failed: 422/)
  })
})

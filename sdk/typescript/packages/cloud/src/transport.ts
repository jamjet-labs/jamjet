import type { SpanEventDict } from './span.js'

export type TransportOptions = {
  apiKey: string
  apiUrl: string
  project: string
  maxRetries?: number
  initialBackoffMs?: number
  maxBackoffMs?: number
  fetchImpl?: typeof fetch
}

const GZIP_THRESHOLD_BYTES = 4 * 1024

export class TransportError extends Error {
  constructor(message: string, public readonly status?: number) {
    super(message)
    this.name = 'TransportError'
  }
}

export class Transport {
  private readonly apiKey: string
  private readonly apiUrl: string
  private readonly project: string
  private readonly maxRetries: number
  private readonly initialBackoffMs: number
  private readonly maxBackoffMs: number
  private readonly fetchImpl: typeof fetch

  constructor(opts: TransportOptions) {
    this.apiKey = opts.apiKey
    this.apiUrl = opts.apiUrl.replace(/\/$/, '')
    this.project = opts.project
    this.maxRetries = opts.maxRetries ?? 5
    this.initialBackoffMs = opts.initialBackoffMs ?? 250
    this.maxBackoffMs = opts.maxBackoffMs ?? 8000
    this.fetchImpl = opts.fetchImpl ?? globalThis.fetch.bind(globalThis)
  }

  async send(events: SpanEventDict[]): Promise<void> {
    if (events.length === 0) return

    const url = `${this.apiUrl}/v1/events/ingest`
    const json = JSON.stringify({ project: this.project, events })
    const bodyBytes = new TextEncoder().encode(json)
    let body: BodyInit = bodyBytes as unknown as BodyInit
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.apiKey}`,
      'Content-Type': 'application/json',
    }

    if (bodyBytes.byteLength > GZIP_THRESHOLD_BYTES && typeof CompressionStream !== 'undefined') {
      body = (await gzip(bodyBytes)) as unknown as BodyInit
      headers['Content-Encoding'] = 'gzip'
    }

    let attempt = 0
    let backoff = this.initialBackoffMs

    while (true) {
      let res: Response
      try {
        res = await this.fetchImpl(url, { method: 'POST', headers, body })
      } catch (err) {
        if (attempt >= this.maxRetries) {
          throw new TransportError(`network error: ${(err as Error).message}`)
        }
        await sleep(backoff)
        backoff = Math.min(backoff * 2, this.maxBackoffMs)
        attempt++
        continue
      }

      if (res.ok) return

      if (res.status === 429) {
        const retryAfter = parseInt(res.headers.get('Retry-After') ?? '0', 10)
        if (attempt >= this.maxRetries) {
          throw new TransportError(`rate limited (429) after ${attempt} retries`, 429)
        }
        await sleep(retryAfter * 1000 || backoff)
        backoff = Math.min(backoff * 2, this.maxBackoffMs)
        attempt++
        continue
      }

      if (res.status >= 500) {
        if (attempt >= this.maxRetries) {
          throw new TransportError(`server error ${res.status} after ${attempt} retries`, res.status)
        }
        await sleep(backoff)
        backoff = Math.min(backoff * 2, this.maxBackoffMs)
        attempt++
        continue
      }

      throw new TransportError(`request failed with status ${res.status}`, res.status)
    }
  }
}

async function gzip(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes as unknown as BlobPart])
    .stream()
    .pipeThrough(new CompressionStream('gzip'))
  const buf = await new Response(stream).arrayBuffer()
  return new Uint8Array(buf)
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

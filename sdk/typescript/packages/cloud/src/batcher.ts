import type { SpanEventDict } from './span.js'

export type BatcherOptions = {
  send: (events: SpanEventDict[]) => Promise<void>
  batchSize?: number
  flushIntervalMs?: number
  maxBufferSize?: number
  onError?: (err: unknown) => void
}

export class Batcher {
  private readonly send: (events: SpanEventDict[]) => Promise<void>
  private readonly batchSize: number
  private readonly flushIntervalMs: number
  private readonly maxBufferSize: number
  private readonly onError: (err: unknown) => void

  private buffer: SpanEventDict[] = []
  private timer: ReturnType<typeof setInterval> | null = null
  private flushing = false
  public droppedCount = 0

  constructor(opts: BatcherOptions) {
    this.send = opts.send
    this.batchSize = opts.batchSize ?? 32
    this.flushIntervalMs = opts.flushIntervalMs ?? 2000
    this.maxBufferSize = opts.maxBufferSize ?? 1024
    this.onError = opts.onError ?? (() => {})
    this.startTimer()
  }

  add(event: SpanEventDict): void {
    if (this.buffer.length >= this.maxBufferSize) {
      this.buffer.shift()
      this.droppedCount++
    }
    this.buffer.push(event)
    if (this.buffer.length >= this.batchSize) {
      void this.flush()
    }
  }

  async flush(): Promise<void> {
    if (this.flushing || this.buffer.length === 0) return
    this.flushing = true
    const batch = this.buffer.splice(0, this.buffer.length)
    try {
      await this.send(batch)
    } catch (err) {
      this.onError(err)
    } finally {
      this.flushing = false
    }
  }

  async shutdown(): Promise<void> {
    if (this.timer !== null) {
      clearInterval(this.timer)
      this.timer = null
    }
    await this.flush()
  }

  private startTimer(): void {
    if (typeof setInterval === 'undefined') return
    this.timer = setInterval(() => {
      void this.flush()
    }, this.flushIntervalMs)
    if (typeof (this.timer as { unref?: () => void }).unref === 'function') {
      ;(this.timer as { unref: () => void }).unref()
    }
  }
}

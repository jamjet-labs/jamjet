import { describe, expect, test, vi } from 'vitest'
import { Batcher } from '../src/batcher.js'
import type { SpanEventDict } from '../src/span.js'

const span = (id: string): SpanEventDict =>
  ({ type: 'span', trace_id: 't', span_id: id, kind: 'k', name: 'n', sequence: 0, timestamp: '', status: 'ok' } as SpanEventDict)

describe('Batcher', () => {
  test('flushes when batch reaches batchSize', async () => {
    const sent: SpanEventDict[][] = []
    const b = new Batcher({
      send: async (events) => { sent.push(events) },
      batchSize: 3,
      flushIntervalMs: 9999,
      maxBufferSize: 100,
    })
    b.add(span('a'))
    b.add(span('b'))
    b.add(span('c'))
    await b.shutdown()
    expect(sent).toHaveLength(1)
    expect(sent[0]?.map((s) => s.span_id)).toEqual(['a', 'b', 'c'])
  })

  test('flushes on interval timer', async () => {
    vi.useFakeTimers()
    const sent: SpanEventDict[][] = []
    const b = new Batcher({
      send: async (events) => { sent.push(events) },
      batchSize: 100,
      flushIntervalMs: 200,
      maxBufferSize: 100,
    })
    b.add(span('a'))
    expect(sent).toHaveLength(0)
    await vi.advanceTimersByTimeAsync(250)
    expect(sent).toHaveLength(1)
    expect(sent[0]?.[0]?.span_id).toBe('a')
    await b.shutdown()
    vi.useRealTimers()
  })

  test('drops oldest on overflow with dropped_count counter', async () => {
    const sent: SpanEventDict[][] = []
    const b = new Batcher({
      send: async (events) => { sent.push(events) },
      batchSize: 100,
      flushIntervalMs: 9999,
      maxBufferSize: 3,
    })
    b.add(span('a'))
    b.add(span('b'))
    b.add(span('c'))
    b.add(span('d'))
    b.add(span('e'))
    await b.shutdown()
    const flat = sent.flat().map((s) => s.span_id)
    expect(flat).toEqual(expect.arrayContaining(['c', 'd', 'e']))
    expect(flat).not.toContain('a')
    expect(flat).not.toContain('b')
    expect(b.droppedCount).toBe(2)
  })

  test('shutdown flushes remaining buffer', async () => {
    const sent: SpanEventDict[][] = []
    const b = new Batcher({
      send: async (events) => { sent.push(events) },
      batchSize: 100,
      flushIntervalMs: 9999,
      maxBufferSize: 100,
    })
    b.add(span('a'))
    await b.shutdown()
    expect(sent).toHaveLength(1)
    expect(sent[0]?.[0]?.span_id).toBe('a')
  })

  test('swallows transport errors (fail-soft)', async () => {
    const b = new Batcher({
      send: async () => { throw new Error('boom') },
      batchSize: 1,
      flushIntervalMs: 9999,
      maxBufferSize: 100,
    })
    expect(() => b.add(span('a'))).not.toThrow()
    await b.shutdown()
  })
})

import { describe, it, expect, afterEach } from 'vitest'
import { parseTraceparent, readTraceparent } from '../src/trace-context.js'

describe('parseTraceparent', () => {
  it('parses a valid W3C traceparent', () => {
    const result = parseTraceparent(
      '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
    )
    expect(result).toEqual({
      version: '00',
      trace_id: '0af7651916cd43dd8448eb211c80319c',
      parent_id: 'b7ad6b7169203331',
      flags: '01',
    })
  })

  it('returns null for malformed input', () => {
    expect(parseTraceparent('garbage')).toBeNull()
    expect(parseTraceparent('00-not-hex-flags')).toBeNull()
    expect(parseTraceparent('')).toBeNull()
    expect(parseTraceparent(undefined)).toBeNull()
    expect(parseTraceparent(null)).toBeNull()
  })

  it('rejects unsupported version', () => {
    expect(
      parseTraceparent('ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'),
    ).toBeNull()
  })

  it('rejects all-zero trace_id (W3C invalid)', () => {
    expect(
      parseTraceparent('00-00000000000000000000000000000000-b7ad6b7169203331-01'),
    ).toBeNull()
  })

  it('rejects all-zero parent_id (W3C invalid)', () => {
    expect(
      parseTraceparent('00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01'),
    ).toBeNull()
  })

  it('normalizes hex to lowercase', () => {
    const result = parseTraceparent(
      '00-0AF7651916CD43DD8448EB211C80319C-B7AD6B7169203331-01',
    )
    expect(result?.trace_id).toBe('0af7651916cd43dd8448eb211c80319c')
    expect(result?.parent_id).toBe('b7ad6b7169203331')
  })

  it('tolerates surrounding whitespace', () => {
    const result = parseTraceparent(
      '  00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01  ',
    )
    expect(result?.trace_id).toBe('0af7651916cd43dd8448eb211c80319c')
  })
})

describe('readTraceparent', () => {
  const originalEnv = { ...process.env }
  afterEach(() => {
    process.env = { ...originalEnv }
  })

  it('reads from headers if provided', () => {
    const headers = {
      traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
    }
    expect(readTraceparent({ headers })?.trace_id).toBe(
      '0af7651916cd43dd8448eb211c80319c',
    )
  })

  it('accepts the canonical capitalized header name', () => {
    const headers = {
      Traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
    }
    expect(readTraceparent({ headers })?.trace_id).toBe(
      '0af7651916cd43dd8448eb211c80319c',
    )
  })

  it('handles array header values (node http req.headers shape)', () => {
    const headers = {
      traceparent: ['00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'],
    }
    expect(readTraceparent({ headers })?.trace_id).toBe(
      '0af7651916cd43dd8448eb211c80319c',
    )
  })

  it('reads OTEL_TRACE_ID env when no headers', () => {
    delete process.env.OTEL_TRACE_ID
    process.env.OTEL_TRACE_ID = '0af7651916cd43dd8448eb211c80319c'
    expect(readTraceparent()?.trace_id).toBe('0af7651916cd43dd8448eb211c80319c')
  })

  it('returns null when no source has a usable trace id', () => {
    delete process.env.OTEL_TRACE_ID
    expect(readTraceparent()).toBeNull()
  })

  it('ignores malformed OTEL_TRACE_ID', () => {
    process.env.OTEL_TRACE_ID = 'too-short'
    expect(readTraceparent()).toBeNull()
  })

  it('header source wins over env', () => {
    process.env.OTEL_TRACE_ID = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    const headers = {
      traceparent: '00-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cccccccccccccccc-01',
    }
    expect(readTraceparent({ headers })?.trace_id).toBe(
      'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
    )
  })
})

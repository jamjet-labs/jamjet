import { describe, expect, test } from 'vitest'
import { redact, redactDict, DEFAULT_PII_TYPES } from '../src/redaction.js'

describe('redact', () => {
  test('redacts email', () => {
    expect(redact('contact alice@example.com please')).toBe('contact [EMAIL_ADDRESS] please')
  })

  test('redacts phone numbers', () => {
    expect(redact('call 555-123-4567')).toBe('call [PHONE_NUMBER]')
    expect(redact('call (555) 123-4567')).toBe('call [PHONE_NUMBER]')
    expect(redact('call +1 555 123 4567')).toBe('call [PHONE_NUMBER]')
  })

  test('redacts SSN', () => {
    expect(redact('SSN: 123-45-6789')).toBe('SSN: [US_SSN]')
  })

  test('redacts credit card-like 13-16 digit runs', () => {
    expect(redact('card 4111 1111 1111 1111')).toBe('card [CREDIT_CARD]')
  })

  test('redacts IP addresses', () => {
    expect(redact('connect to 192.168.1.1')).toBe('connect to [IP_ADDRESS]')
  })

  test('redacts IBAN', () => {
    expect(redact('iban DE89370400440532013000')).toBe('iban [IBAN_CODE]')
  })

  test('passes through non-PII text unchanged', () => {
    expect(redact('hello world')).toBe('hello world')
  })

  test('handles multiple PII in one string', () => {
    const out = redact('email alice@example.com or call 555-123-4567')
    expect(out).toBe('email [EMAIL_ADDRESS] or call [PHONE_NUMBER]')
  })

  test('respects pii_types filter', () => {
    expect(redact('alice@example.com 555-123-4567', { piiTypes: ['EMAIL_ADDRESS'] }))
      .toBe('[EMAIL_ADDRESS] 555-123-4567')
  })

  test('returns text unchanged when piiTypes is empty', () => {
    expect(redact('alice@example.com', { piiTypes: [] })).toBe('alice@example.com')
  })
})

describe('redactDict', () => {
  test('recursively redacts strings in nested dict', () => {
    const obj = {
      user: { email: 'alice@example.com', name: 'Alice' },
      items: ['call 555-123-4567', 'plain text'],
    }
    const out = redactDict(obj) as typeof obj
    expect(out.user.email).toBe('[EMAIL_ADDRESS]')
    expect(out.user.name).toBe('Alice')
    expect(out.items[0]).toBe('call [PHONE_NUMBER]')
    expect(out.items[1]).toBe('plain text')
  })

  test('preserves non-string types', () => {
    const obj = { count: 42, active: true, ratio: 0.5, none: null }
    const out = redactDict(obj)
    expect(out).toEqual(obj)
  })
})

describe('DEFAULT_PII_TYPES', () => {
  test('matches Python parity set', () => {
    expect(DEFAULT_PII_TYPES).toEqual([
      'EMAIL_ADDRESS',
      'CREDIT_CARD',
      'US_SSN',
      'PHONE_NUMBER',
      'IP_ADDRESS',
      'IBAN_CODE',
    ])
  })
})

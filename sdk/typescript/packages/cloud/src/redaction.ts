const REGEX_PATTERNS: Record<string, RegExp> = {
  EMAIL_ADDRESS: /[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}/g,
  CREDIT_CARD: /\b(?:\d[\s\-]?){13,15}\d\b/g,
  US_SSN: /\b\d{3}[-\s]?\d{2}[-\s]?\d{4}\b/g,
  PHONE_NUMBER: /(?<!\w)(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}(?!\w)/g,
  IP_ADDRESS: /\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b/g,
  IBAN_CODE: /\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b/g,
}

export const DEFAULT_PII_TYPES = [
  'EMAIL_ADDRESS',
  'CREDIT_CARD',
  'US_SSN',
  'PHONE_NUMBER',
  'IP_ADDRESS',
  'IBAN_CODE',
] as const

export type PiiType = (typeof DEFAULT_PII_TYPES)[number]

export type RedactOptions = {
  piiTypes?: readonly string[]
  replacementFormat?: string
}

function makeReplacement(piiType: string, format: string): string {
  return format.replace('{type}', piiType)
}

export function redact(text: string, opts: RedactOptions = {}): string {
  const types = opts.piiTypes ?? DEFAULT_PII_TYPES
  const format = opts.replacementFormat ?? '[{type}]'
  let result = text
  for (const piiType of types) {
    const pattern = REGEX_PATTERNS[piiType]
    if (pattern) {
      result = result.replace(pattern, makeReplacement(piiType, format))
    }
  }
  return result
}

export function redactDict(obj: unknown, opts: RedactOptions = {}): unknown {
  if (typeof obj === 'string') return redact(obj, opts)
  if (Array.isArray(obj)) return obj.map((item) => redactDict(item, opts))
  if (obj !== null && typeof obj === 'object') {
    const out: Record<string, unknown> = {}
    for (const [k, v] of Object.entries(obj)) {
      out[k] = redactDict(v, opts)
    }
    return out
  }
  return obj
}

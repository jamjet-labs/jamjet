// What a reviewer sees in an approval request, and how the request is bound
// to the call it approves. Mirrors sdk/python/jamjet/cloud/approval_context.py.
import { redact } from './redaction.js'

// Fields whose value is a credential, whatever it looks like. An explicit
// list: a name pattern such as `*_token` would also hide business fields
// (`payee_token`); credentials under other names are caught by shape below.
const SECRET_KEY =
  /^(password|passwd|passphrase|secret|client_secret|api_key|apikey|x-api-key|authorization|access_token|refresh_token|id_token|private_key)$/i

// Credentials recognisable by shape inside any string.
const SECRET_VALUES: RegExp[] = [
  /\bBearer\s+[A-Za-z0-9._~+/=-]+/gi,
  /\bBasic\s+[A-Za-z0-9+/=]{8,}/g,
  /\bsk-[A-Za-z0-9_-]{8,}/g,
  /\bjj_[A-Za-z0-9_-]{8,}/g,
  /\bAKIA[0-9A-Z]{16}\b/g,
  /\bgh[pousr]_[A-Za-z0-9]{20,}/g,
  /\bxox[abprs]-[A-Za-z0-9-]{10,}/g,
  /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g,
  // JWTs
  /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g,
]

// Long high-entropy runs (32+ characters mixing upper case, lower case and
// digits) are treated as keys too. Hex digests and plain numbers do not mix
// cases, so they are kept.
const HIGH_ENTROPY = /[A-Za-z0-9+/_=-]{32,}/g
function looksRandom(run: string): boolean {
  return /[A-Z]/.test(run) && /[a-z]/.test(run) && /\d/.test(run)
}

// With redaction on: these are masked outright...
const PII_MASKED = ['EMAIL_ADDRESS', 'US_SSN', 'PHONE_NUMBER', 'IP_ADDRESS'] as const
// ...and account numbers keep their last 4, which a payment reviewer needs.
const ACCOUNT_NUMBERS: RegExp[] = [/\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b/g, /\b(?:\d[\s-]?){12,18}\d\b/g]

const SECRET = '[SECRET]'

function maskString(text: string, pii: boolean): string {
  let out = text
  for (const re of SECRET_VALUES) out = out.replace(re, SECRET)
  out = out.replace(HIGH_ENTROPY, (run) => (looksRandom(run) ? SECRET : run))
  if (!pii) return out
  out = redact(out, { piiTypes: PII_MASKED })
  for (const re of ACCOUNT_NUMBERS) {
    out = out.replace(re, (m) => {
      const digits = m.replace(/[\s-]/g, '')
      return `****${digits.slice(-4)}`
    })
  }
  return out
}

/**
 * The approval context as sent to Cloud. Credentials are always masked: by
 * an explicit list of field names, and by shape (bearer and basic headers,
 * JWTs, known key prefixes, long high-entropy strings). With `pii` on (redaction not `off`), emails,
 * SSNs, phone numbers and IPs are masked too, and IBANs and card numbers keep
 * their last 4. Amounts, payees and other business fields are kept.
 */
export function maskForApproval(value: unknown, pii: boolean): unknown {
  if (typeof value === 'string') return maskString(value, pii)
  if (Array.isArray(value)) return value.map((v) => maskForApproval(v, pii))
  if (value !== null && typeof value === 'object') {
    const out: Record<string, unknown> = {}
    for (const [k, v] of Object.entries(value)) {
      out[k] = SECRET_KEY.test(k) && v !== null && v !== '' ? SECRET : maskForApproval(v, pii)
    }
    return out
  }
  return value
}

/** Tool arguments as an object when they are a JSON string (OpenAI), else as given. */
export function parsedArguments(args: unknown): unknown {
  if (typeof args !== 'string') return args
  try {
    return JSON.parse(args)
  } catch {
    return args
  }
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical)
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((k) => [k, canonical((value as Record<string, unknown>)[k])]),
    )
  }
  return value
}

/**
 * SHA-256 (hex) of the canonical JSON of the arguments: keys sorted, no
 * whitespace, a JSON string argument parsed first. Sent with the approval and
 * re-checked before the approved call is released, so an approval cannot be
 * reused for different arguments. The Python SDK computes the same digest.
 */
export async function argumentsDigest(args: unknown): Promise<string> {
  const text = JSON.stringify(canonical(parsedArguments(args)) ?? null)
  const bytes = await globalThis.crypto.subtle.digest('SHA-256', new TextEncoder().encode(text))
  return Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, '0')).join('')
}

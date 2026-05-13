// Path-mode selection for Cloud Sync v0.1 (spec §3.4).
//
// Two modes the adapter can run in:
//
//   local-only — Phase 2 behavior. Adapter only writes audit JSONL to disk.
//                The sidecar daemon (Path A) observes the JSONL via filesystem
//                if it's running; adapters don't need to know.
//
//   direct     — Path B. In addition to writing local JSONL, the adapter
//                inline-POSTs each event to Cloud's /v1/policy-audit/events.
//                Required for environments without a persistent filesystem
//                (Vercel, Cloudflare Workers, AWS Lambda, GH Actions, etc.).
//
// Selection rules, in order:
//   1. No JAMJET_CLOUD_TOKEN → always local-only (Cloud is unconfigured).
//   2. JAMJET_CLOUD_MODE=direct → direct (operator override).
//   3. JAMJET_CLOUD_MODE=daemon → local-only (operator override).
//   4. Any serverless indicator env var is set → direct (auto-detect).
//   5. Otherwise → local-only (assume a local daemon will catch the JSONL).
//
// Unknown JAMJET_CLOUD_MODE values are ignored and fall through to the
// heuristic — we don't want a typo to silently disable Path B.
export type PathMode = 'local-only' | 'direct'

const SERVERLESS_ENV_VARS = [
  'VERCEL',
  'CF_PAGES',
  'AWS_LAMBDA_FUNCTION_NAME',
  'GITHUB_ACTIONS',
  'NETLIFY',
] as const

export function detectPathMode(): PathMode {
  const token = process.env.JAMJET_CLOUD_TOKEN
  if (!token) return 'local-only'

  const explicit = process.env.JAMJET_CLOUD_MODE
  if (explicit === 'direct') return 'direct'
  if (explicit === 'daemon') return 'local-only'

  for (const v of SERVERLESS_ENV_VARS) {
    if (process.env[v]) return 'direct'
  }
  return 'local-only'
}

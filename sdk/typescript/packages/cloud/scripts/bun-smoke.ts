/**
 * Does @jamjet/cloud actually work on the Bun runtime?
 *
 * Imports the BUILT bundle — what a user installs — rather than the sources, and
 * exercises real behaviour rather than only checking that the module loads.
 *
 * A plain script, not a test-runner file, on purpose. This job used to run
 * `vitest` under `--bun`; vitest drives its own module runner, and its
 * interop with Bun is a property of vitest, not of this SDK. A failure there
 * says nothing about whether the package works for a Bun user, which is the
 * only question this job exists to answer.
 *
 * Merely importing the bundle is itself a real check: `config.ts` builds its
 * zod schemas at module scope, so a broken ESM/CJS interop fails here.
 */
import assert from 'node:assert/strict'

import { VERSION, redact, estimateCost, ConfigError, PolicyEvaluator } from '../dist/index.js'

const checks: [string, () => void][] = [
  ['VERSION is published', () => {
    assert.equal(typeof VERSION, 'string')
    assert.match(VERSION, /^\d+\.\d+\.\d+/)
  }],
  ['zod-backed config module loaded', () => {
    // ConfigError comes from config.ts, whose zod schemas are built at module
    // scope. If zod's named export did not resolve under Bun, the import above
    // has already thrown; this pins the symbol as well.
    assert.equal(typeof ConfigError, 'function')
    assert.ok(new ConfigError('x') instanceof Error)
  }],
  ['redaction behaves', () => {
    assert.equal(redact('contact alice@example.com please'), 'contact [EMAIL_ADDRESS] please')
    assert.equal(redact('call 555-123-4567'), 'call [PHONE_NUMBER]')
  }],
  ['cost estimation behaves', () => {
    const c = estimateCost('gpt-4o', 100, 50)
    assert.ok(Math.abs(c - 0.00075) < 1e-8, `expected ~0.00075, got ${c}`)
  }],
  ['classes construct', () => {
    assert.equal(typeof PolicyEvaluator, 'function')
  }],
]

let failed = 0
for (const [name, run] of checks) {
  try {
    run()
    console.log(`ok - ${name}`)
  } catch (err) {
    failed++
    console.error(`not ok - ${name}`)
    console.error(err instanceof Error ? err.stack : String(err))
  }
}

const bun = (globalThis as { Bun?: { version: string } }).Bun
console.log(`\n${checks.length - failed}/${checks.length} passed on ${bun ? `Bun ${bun.version}` : process.version}`)
if (failed > 0) process.exit(1)

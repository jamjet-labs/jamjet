import { redact, applyCacheInject, estimateCost, CloudPusher } from '@jamjet/cloud'
import { type Session } from '../session.js'
import { type FeatureEvent } from './events.js'
import { SYSTEM_PROMPT } from './knowledge-base.js'
import { type MockModelArgs } from './model-mock.js'
import { computePrefixHash } from './prefix-hash.js'
import { cacheReadSavingsCents } from './savings.js'
import { selectModel } from './select-model.js'

export interface TurnResult {
  reply: string
  events: FeatureEvent[]
}

export async function runTurn(
  session: Session,
  input: { text: string },
): Promise<TurnResult> {
  const events: FeatureEvent[] = []

  // 1. Budget pre-check: block without calling the model
  if (session.spentCents >= session.budgetCents) {
    events.push({
      kind: 'budget_exceeded',
      spentCents: session.spentCents,
      capCents: session.budgetCents,
    })
    return { reply: '(budget cap reached — request blocked)', events }
  }

  // 2. Redact PII
  const redacted = redact(input.text)
  if (redacted !== input.text) {
    events.push({ kind: 'redaction', type: 'PII', count: 1 })
  }

  // 3. Build base args
  let callArgs: MockModelArgs = {
    model: session.model,
    system: SYSTEM_PROMPT,
    messages: [{ role: 'user', content: redacted }],
  }

  // 4. Prefix hash for waste tracking
  const hash = computePrefixHash([
    { role: 'system', content: SYSTEM_PROMPT },
    { role: 'user', content: redacted },
  ])

  // 5. Cache inject (prevention)
  if (session.cacheInjectOn) {
    const { mutated } = applyCacheInject(callArgs as unknown as Record<string, unknown>)
    callArgs = mutated as unknown as MockModelArgs
  }

  // 6. Call the model (live when ANTHROPIC_API_KEY is set, mock otherwise)
  const res = await selectModel()(callArgs)

  // 7. Record waste
  session.tracker.record(hash, res.usage.input_tokens)
  const wasteEntry = session.tracker.detect().find((d) => d.prefixHash === hash)
  if (wasteEntry !== undefined) {
    events.push({
      kind: 'waste_detected',
      prefixHash: wasteEntry.prefixHash,
      repeats: wasteEntry.repeats,
      rePaidTokens: wasteEntry.rePaidTokens,
      wastedCents: wasteEntry.wastedCents,
    })
  }

  // 8. Cost accounting
  const cents = estimateCost(res.model, res.usage.input_tokens, res.usage.output_tokens) * 100
  events.push({
    kind: 'cost',
    cents,
    model: res.model,
    inTok: res.usage.input_tokens,
    outTok: res.usage.output_tokens,
  })
  const exceeded = session.addSpend(cents)
  if (exceeded) {
    events.push({
      kind: 'budget_exceeded',
      spentCents: session.spentCents,
      capCents: session.budgetCents,
    })
  }

  // 9. Cache savings
  const cacheRead = res.usage.cache_read_input_tokens
  if (cacheRead > 0) {
    events.push({
      kind: 'cache_saved',
      savedCents: cacheReadSavingsCents(res.model, cacheRead),
      cacheReadTokens: cacheRead,
    })
  }

  // 10. Best-effort dashboard streaming via CloudPusher (JAMJET_API_KEY gated)
  //     A push failure must NEVER break the turn — wrapped in try/catch, fire-and-forget.
  if (process.env.JAMJET_API_KEY) {
    try {
      const pusher = new CloudPusher({
        apiBase: process.env.JAMJET_API_BASE ?? 'https://api.jamjet.dev',
        apiKey: process.env.JAMJET_API_KEY,
      })
      void pusher.push({
        ts: new Date().toISOString(),
        run_id: crypto.randomUUID(),
        adapter: 'support-agent-showcase',
        host: 'showcase',
        tool: 'model_call',
        decision: 'allowed',
        executed: true,
        schema_version: 1,
        args: {
          model: res.model,
          cents,
          inTok: res.usage.input_tokens,
          outTok: res.usage.output_tokens,
          cacheReadTokens: cacheRead,
        },
      })
    } catch {
      // intentionally swallowed — dashboard push is non-critical
    }
  }

  // 12. Refund approval gate — detect refund intent in the (already redacted) input
  if (/refund/i.test(redacted)) {
    const id = session.openApproval('issue_refund')
    events.push({ kind: 'approval_required', id, tool: 'issue_refund' })
    return {
      reply: 'A refund needs a human approval before I can issue it.',
      events,
    }
  }

  // 13. Return result
  return { reply: res.content[0].text, events }
}

'use client'

import type { FeatureEvent } from '../../lib/engine/events.js'

interface Props {
  events: FeatureEvent[]
  savedCents: number
  spentCents: number
}

export function SavingsPanel({ events, savedCents, spentCents }: Props) {
  const savedEvents = events.filter(
    (e): e is Extract<FeatureEvent, { kind: 'cache_saved' }> => e.kind === 'cache_saved',
  )

  const totalSpentIfNoCache = spentCents + savedCents
  const pctCut =
    totalSpentIfNoCache > 0 ? ((savedCents / totalSpentIfNoCache) * 100).toFixed(1) : null

  return (
    <div className="cost-section">
      <h3 className="cost-section-title">Cache Savings</h3>
      {savedEvents.length === 0 ? (
        <p className="cost-section-empty">No cache hits yet. Enable cache_inject to start saving.</p>
      ) : (
        <>
          <ul className="savings-list">
            {savedEvents.map((ev, i) => (
              <li key={i} className="savings-entry" data-testid="cache-saved">
                <span className="savings-amount">+{ev.savedCents.toFixed(4)}¢ saved</span>
                <span className="savings-tokens">
                  {ev.cacheReadTokens.toLocaleString()} cache-read tokens
                </span>
              </li>
            ))}
          </ul>
          <div className="savings-total">
            Total saved: <strong>{savedCents.toFixed(4)}¢</strong>
            {pctCut && <span className="savings-pct"> ({pctCut}% cost reduction)</span>}
          </div>
        </>
      )}
    </div>
  )
}

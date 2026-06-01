'use client'

import type { FeatureEvent } from '../../lib/engine/events.js'

interface Props {
  events: FeatureEvent[]
  spentCents: number
}

const DEFAULT_CAP_CENTS = 5

export function SpendBar({ events, spentCents }: Props) {
  const budgetExceededEvent = events.findLast((e) => e.kind === 'budget_exceeded') as
    | Extract<FeatureEvent, { kind: 'budget_exceeded' }>
    | undefined

  const capCents = budgetExceededEvent?.capCents ?? DEFAULT_CAP_CENTS
  const exceeded = !!budgetExceededEvent
  const pct = Math.min(100, (spentCents / capCents) * 100)

  return (
    <div className="spend-bar-wrapper" data-testid="spend-bar">
      <div className="spend-bar-labels">
        <span>spent {spentCents.toFixed(2)}¢</span>
        <span className={exceeded ? 'spend-bar-exceeded-label' : ''}>
          {exceeded ? 'budget cap reached' : `cap ${capCents.toFixed(2)}¢`}
        </span>
      </div>
      <div className={`spend-bar-track${exceeded ? ' spend-bar-track--exceeded' : ''}`}>
        <div
          className={`spend-bar-fill${exceeded ? ' spend-bar-fill--exceeded' : ''}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  )
}

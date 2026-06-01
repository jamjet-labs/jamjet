'use client'

import type { FeatureEvent } from '../../lib/engine/events.js'

interface Props {
  events: FeatureEvent[]
}

export function WasteDetector({ events }: Props) {
  const wasteEvents = events.filter(
    (e): e is Extract<FeatureEvent, { kind: 'waste_detected' }> => e.kind === 'waste_detected',
  )

  return (
    <div className="cost-section">
      <h3 className="cost-section-title">Waste Detection</h3>
      {wasteEvents.length === 0 ? (
        <p className="cost-section-empty">No repeated prefixes detected yet.</p>
      ) : (
        <ul className="waste-list">
          {wasteEvents.map((ev, i) => (
            <li key={i} className="waste-alert" data-testid="waste-alert">
              <span className="waste-hash">prefix {ev.prefixHash.slice(0, 8)}</span> sent{' '}
              <strong>{ev.repeats}×</strong>, ~{ev.rePaidTokens.toLocaleString()} tokens re-paid,{' '}
              <span className="waste-cost">≈ {ev.wastedCents.toFixed(3)}¢ wasted</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

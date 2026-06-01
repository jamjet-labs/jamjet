'use client'

import type { FeatureEvent } from '../../lib/engine/events.js'
import type { Totals } from '../lib/useDemo'
import { SavingsTicker } from './SavingsTicker'
import { WasteDetector } from './WasteDetector'
import { SavingsPanel } from './SavingsPanel'
import { GovernanceStrip } from './GovernanceStrip'

interface Props {
  events: FeatureEvent[]
  totals: Totals
  pendingApprovalId: string | null
  approve: (id: string, decision: 'approved' | 'rejected') => Promise<void>
}

export function CostPanel({ events, totals, pendingApprovalId, approve }: Props) {
  return (
    <div className="cost-panel">
      <SavingsTicker savedCents={totals.savedCents} />
      <WasteDetector events={events} />
      <SavingsPanel events={events} savedCents={totals.savedCents} spentCents={totals.spentCents} />
      <GovernanceStrip events={events} pendingApprovalId={pendingApprovalId} approve={approve} />
    </div>
  )
}

'use client'

import type { FeatureEvent } from '../../lib/engine/events.js'

interface Props {
  events: FeatureEvent[]
  pendingApprovalId: string | null
  approve: (id: string, decision: 'approved' | 'rejected') => Promise<void>
}

type GovEvent = Extract<
  FeatureEvent,
  { kind: 'redaction' | 'policy_blocked' | 'approval_required' | 'approval_resolved' | 'audit' }
>

export function GovernanceStrip({ events, pendingApprovalId, approve }: Props) {
  const govEvents = events.filter((e): e is GovEvent =>
    ['redaction', 'policy_blocked', 'approval_required', 'approval_resolved', 'audit'].includes(
      e.kind,
    ),
  )

  if (govEvents.length === 0) {
    return (
      <div className="cost-section">
        <h3 className="cost-section-title">Governance</h3>
        <p className="cost-section-empty">No governance events yet.</p>
      </div>
    )
  }

  return (
    <div className="cost-section">
      <h3 className="cost-section-title">Governance</h3>
      <ul className="gov-list">
        {govEvents.map((ev, i) => {
          if (ev.kind === 'redaction') {
            return (
              <li key={i} className="gov-entry gov-entry--redaction">
                PII redacted: <strong>{ev.type}</strong> ({ev.count} occurrence
                {ev.count !== 1 ? 's' : ''})
              </li>
            )
          }
          if (ev.kind === 'policy_blocked') {
            return (
              <li key={i} className="gov-entry gov-entry--blocked">
                Policy blocked tool: <strong>{ev.tool}</strong>
              </li>
            )
          }
          if (ev.kind === 'approval_required') {
            const isPending = pendingApprovalId === ev.id
            return (
              <li key={i} className="gov-entry gov-entry--approval" data-testid="approval-card">
                <span>
                  Approval required: <strong>{ev.tool}</strong> (id: {ev.id.slice(0, 8)})
                </span>
                {isPending && (
                  <span className="gov-approval-btns">
                    <button
                      className="gov-btn gov-btn--approve"
                      data-testid="approve-btn"
                      onClick={() => void approve(ev.id, 'approved')}
                    >
                      Approve
                    </button>
                    <button
                      className="gov-btn gov-btn--reject"
                      data-testid="reject-btn"
                      onClick={() => void approve(ev.id, 'rejected')}
                    >
                      Reject
                    </button>
                  </span>
                )}
              </li>
            )
          }
          if (ev.kind === 'approval_resolved') {
            return (
              <li key={i} className="gov-entry gov-entry--resolved">
                Approval {ev.decision}: id {ev.id.slice(0, 8)}
              </li>
            )
          }
          if (ev.kind === 'audit') {
            return (
              <li key={i} className="gov-entry gov-entry--audit" data-testid="audit-entry">
                Audit: <strong>{ev.tool}</strong> (id: {ev.id.slice(0, 8)})
              </li>
            )
          }
          return null
        })}
      </ul>
    </div>
  )
}

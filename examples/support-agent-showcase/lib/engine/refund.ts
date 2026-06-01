import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { AuditWriter } from '@jamjet/cloud/node'
import type { Session } from '../session.js'
import type { GovEvent } from './events.js'

export async function resolveRefund(
  session: Session,
  id: string,
  decision: 'approved' | 'rejected',
): Promise<{ events: GovEvent[] }> {
  const events: GovEvent[] = []

  if (!session.pendingApproval(id)) {
    return { events }
  }

  session.resolveApproval(id, decision)
  events.push({ kind: 'approval_resolved', id, decision })

  if (decision === 'approved') {
    const writer = new AuditWriter({
      destination: join(tmpdir(), 'jamjet-audit'),
      adapter: 'ts-sdk',
    })
    writer.write({
      run_id: id,
      host: 'typescript',
      tool: 'issue_refund',
      args: {},
      decision: 'APPROVED',
      rule: 'refund-human-approval',
      rule_kind: 'require_approval',
      executed: true,
    })
    events.push({ kind: 'audit', id, tool: 'issue_refund' })
  }

  return { events }
}

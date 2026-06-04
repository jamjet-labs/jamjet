export const runtime = 'nodejs'

import { resolveRefund } from '../../../lib/engine/refund.js'
import { getServerSession } from '../../../lib/server-session.js'

const VALID_DECISIONS = new Set(['approved', 'rejected'])

export async function POST(req: Request): Promise<Response> {
  let body: unknown
  try {
    body = await req.json()
  } catch {
    return Response.json({ error: 'id and decision required' }, { status: 400 })
  }

  const { id, decision } = body as Record<string, unknown>

  if (!id || typeof id !== 'string') {
    return Response.json({ error: 'id required' }, { status: 400 })
  }
  if (!decision || !VALID_DECISIONS.has(decision as string)) {
    return Response.json({ error: 'decision must be approved or rejected' }, { status: 400 })
  }

  const out = await resolveRefund(
    getServerSession(),
    id,
    decision as 'approved' | 'rejected',
  )
  return Response.json(out)
}

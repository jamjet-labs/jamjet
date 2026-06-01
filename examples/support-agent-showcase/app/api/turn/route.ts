export const runtime = 'nodejs'

import { runTurn } from '../../../lib/engine/run-turn.js'
import { getServerSession } from '../../../lib/server-session.js'

export async function POST(req: Request): Promise<Response> {
  let body: unknown
  try {
    body = await req.json()
  } catch {
    return Response.json({ error: 'text required' }, { status: 400 })
  }

  const text = (body as Record<string, unknown>)?.text
  if (!text || typeof text !== 'string' || text.trim() === '') {
    return Response.json({ error: 'text required' }, { status: 400 })
  }

  const out = await runTurn(getServerSession(), { text })
  return Response.json(out)
}

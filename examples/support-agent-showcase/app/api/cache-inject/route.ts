export const runtime = 'nodejs'

import { getServerSession } from '../../../lib/server-session.js'

export async function POST(_req: Request): Promise<Response> {
  getServerSession().setCacheInject(true)
  return Response.json({ ok: true, cacheInjectOn: true })
}

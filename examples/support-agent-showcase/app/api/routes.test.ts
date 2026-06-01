import { test, expect, beforeEach } from 'vitest'
import { POST as turn } from './turn/route.js'
import { POST as cacheInject } from './cache-inject/route.js'
import { POST as approve } from './approve/route.js'
import { resetServerSession } from '../../lib/server-session.js'

beforeEach(() => resetServerSession())

function req(body: unknown) {
  return new Request('http://localhost/api', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })
}

test('POST /api/turn returns reply + events', async () => {
  const res = await turn(req({ text: 'how do I reset my password?' }))
  expect(res.status).toBe(200)
  const json = await res.json()
  expect(typeof json.reply).toBe('string')
  expect(Array.isArray(json.events)).toBe(true)
})
test('POST /api/cache-inject toggles caching on', async () => {
  const res = await cacheInject(req({}))
  const json = await res.json()
  expect(json.cacheInjectOn).toBe(true)
})
test('POST /api/approve resolves a refund approval', async () => {
  const t = await (await turn(req({ text: 'please refund my order' }))).json()
  const ap = t.events.find((e: any) => e.kind === 'approval_required')
  expect(ap).toBeTruthy()
  const res = await approve(req({ id: ap.id, decision: 'approved' }))
  const json = await res.json()
  expect(json.events.some((e: any) => e.kind === 'audit')).toBe(true)
})
test('POST /api/turn with bad body returns 400', async () => {
  const res = await turn(new Request('http://localhost/api', { method: 'POST', body: 'not json' }))
  expect(res.status).toBe(400)
})

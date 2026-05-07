import { http, HttpResponse } from 'msw'

export const successHandler = http.post('https://api.jamjet.dev/v1/events/ingest', async () => {
  return HttpResponse.json({ accepted: true })
})

export const flakyHandler = (failuresBeforeSuccess: number) => {
  let attempts = 0
  return http.post('https://api.jamjet.dev/v1/events/ingest', async () => {
    attempts++
    if (attempts <= failuresBeforeSuccess) {
      return HttpResponse.json({ error: 'transient' }, { status: 503 })
    }
    return HttpResponse.json({ accepted: true })
  })
}

export const rateLimitHandler = (retryAfterSec: number) => {
  let attempts = 0
  return http.post('https://api.jamjet.dev/v1/events/ingest', async () => {
    attempts++
    if (attempts === 1) {
      return new HttpResponse(JSON.stringify({ error: 'rate_limited' }), {
        status: 429,
        headers: { 'Retry-After': String(retryAfterSec) },
      })
    }
    return HttpResponse.json({ accepted: true })
  })
}

export const unauthorizedHandler = http.post(
  'https://api.jamjet.dev/v1/events/ingest',
  () => HttpResponse.json({ error: 'unauthorized' }, { status: 401 }),
)

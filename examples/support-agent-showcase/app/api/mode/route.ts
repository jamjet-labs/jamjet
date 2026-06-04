export const runtime = 'nodejs'

export function GET(): Response {
  let mode: 'mock' | 'live' | 'live+dashboard'

  if (process.env.JAMJET_API_KEY) {
    mode = 'live+dashboard'
  } else if (process.env.ANTHROPIC_API_KEY) {
    mode = 'live'
  } else {
    mode = 'mock'
  }

  return Response.json({ mode })
}

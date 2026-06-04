// Module-singleton demo session.
// A real multi-user app would key sessions by cookie/JWT; this demo uses one shared session.
//
// We store the session on globalThis so Next.js dev-mode HMR re-imports of this
// module don't silently reset pending approvals between the /api/turn and
// /api/approve requests.
import { createSession, type Session } from './session.js'

declare const globalThis: typeof global & { __demoSession?: Session }

function makeSession(): Session {
  return createSession({ budgetCents: 5, model: 'claude-sonnet-4-6' })
}

export function getServerSession(): Session {
  if (!globalThis.__demoSession) {
    globalThis.__demoSession = makeSession()
  }
  return globalThis.__demoSession
}

/** Recreate the singleton — used in tests to isolate each case. */
export function resetServerSession(): void {
  globalThis.__demoSession = makeSession()
}

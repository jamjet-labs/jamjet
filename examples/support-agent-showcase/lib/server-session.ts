// Module-singleton demo session.
// A real multi-user app would key sessions by cookie/JWT; this demo uses one shared session.
import { createSession, type Session } from './session.js'

let _session: Session = makeSession()

function makeSession(): Session {
  return createSession({ budgetCents: 5, model: 'claude-sonnet-4-6' })
}

export function getServerSession(): Session {
  return _session
}

/** Recreate the singleton — used in tests to isolate each case. */
export function resetServerSession(): void {
  _session = makeSession()
}

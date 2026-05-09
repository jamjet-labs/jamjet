import type { AgentRef, UserContext } from '@jamjet/cloud'
import { getActive } from '@jamjet/cloud'
import type { LanguageModelMiddleware } from 'ai'

const NOT_INIT = 'JamJet Cloud not initialized. Call init() first.'

export interface JamjetMiddlewareOptions {
  agent?: AgentRef
  user?: UserContext
}

export function jamjetMiddleware(_opts?: JamjetMiddlewareOptions): LanguageModelMiddleware {
  return {
    async wrapGenerate({ doGenerate }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)
      // Tasks 3-4 will add enforcement. For now, pass-through.
      return doGenerate()
    },
    async wrapStream({ doStream }) {
      const client = getActive()
      if (!client) throw new Error(NOT_INIT)
      // Task 5 will add streaming enforcement. For now, pass-through.
      return doStream()
    },
  }
}

import { WasteTracker } from './engine/waste.js'

export interface Session {
  spentCents: number
  cacheInjectOn: boolean
  readonly tracker: WasteTracker
  budgetCents: number
  model: string
  addSpend(cents: number): boolean
  setCacheInject(on: boolean): void
  openApproval(tool: string): string
  pendingApproval(id: string): { id: string; tool: string } | undefined
  resolveApproval(id: string, decision: 'approved' | 'rejected'): boolean
}

export function createSession(opts: { budgetCents: number; model: string }): Session {
  let spentCents = 0
  let cacheInjectOn = false
  const pending = new Map<string, { id: string; tool: string }>()
  const tracker = new WasteTracker(opts.model)

  return {
    get spentCents() { return spentCents },
    get cacheInjectOn() { return cacheInjectOn },
    get tracker() { return tracker },
    budgetCents: opts.budgetCents,
    model: opts.model,

    addSpend(cents: number): boolean {
      spentCents += cents
      return spentCents > opts.budgetCents
    },

    setCacheInject(on: boolean): void {
      cacheInjectOn = on
    },

    openApproval(tool: string): string {
      const id = crypto.randomUUID()
      pending.set(id, { id, tool })
      return id
    },

    pendingApproval(id: string): { id: string; tool: string } | undefined {
      return pending.get(id)
    },

    resolveApproval(id: string, _decision: 'approved' | 'rejected'): boolean {
      if (!pending.has(id)) return false
      pending.delete(id)
      return true
    },
  }
}

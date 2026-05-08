import { AsyncLocalStorage } from 'node:async_hooks'

export interface AgentRef {
  readonly name: string
  readonly cardUri?: string
  readonly description?: string
}

export interface UserContext {
  userId: string
  email?: string
  attrs?: Record<string, string | number | boolean>
}

export interface ScopeFrame {
  agent?: AgentRef
  user?: UserContext
}

export interface GovernanceContextOptions {
  alsAvailable?: boolean
  warn?: (msg: string) => void
}

const DEFAULT_WARN_MSG =
  'AsyncLocalStorage unavailable; pass agent explicitly via wrap(openai, { agent }) for cross-await scoping. See https://docs.jamjet.dev/sdk/typescript/edge-runtimes.'

export class GovernanceContext {
  private als: AsyncLocalStorage<ScopeFrame> | null = null
  private fallbackFrame: ScopeFrame | null = null
  private warned = false
  private warnFn: (msg: string) => void

  constructor(opts: GovernanceContextOptions = {}) {
    const alsAvailable = opts.alsAvailable ?? typeof AsyncLocalStorage === 'function'
    if (alsAvailable) {
      try {
        this.als = new AsyncLocalStorage<ScopeFrame>()
      } catch {
        this.als = null
      }
    }
    this.warnFn = opts.warn ?? ((msg) => console.warn(`[jamjet] ${msg}`))
  }

  getCurrentContext(): ScopeFrame | null {
    if (this.als) return this.als.getStore() ?? this.fallbackFrame
    return this.fallbackFrame
  }

  async runInContext<T>(frame: ScopeFrame, fn: () => T | Promise<T>): Promise<T> {
    const merged = this.merge(this.getCurrentContext(), frame)
    if (this.als) {
      return this.als.run(merged, async () => Promise.resolve(fn()))
    }
    if (!this.warned) {
      this.warned = true
      this.warnFn(DEFAULT_WARN_MSG)
    }
    const prev = this.fallbackFrame
    this.fallbackFrame = merged
    try {
      return await Promise.resolve(fn())
    } finally {
      this.fallbackFrame = prev
    }
  }

  setProcessFrame(frame: ScopeFrame | null): void {
    this.fallbackFrame = frame
  }

  private merge(parent: ScopeFrame | null, child: ScopeFrame): ScopeFrame {
    if (!parent) return child
    const agent = child.agent ?? parent.agent
    const user = child.user ?? parent.user
    return {
      ...(agent !== undefined ? { agent } : {}),
      ...(user !== undefined ? { user } : {}),
    }
  }
}

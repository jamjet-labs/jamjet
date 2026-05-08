// AsyncLocalStorage is loaded lazily so that the universal-core bundle
// (platform: 'neutral') never contains a static import of 'node:async_hooks'.
// Edge runtimes (Vercel Edge, Cloudflare Workers) that lack the module will
// simply get null back and fall through to the sync fallback path.

type ALSInstance<T> = {
  run<R>(store: T, fn: () => R): R
  getStore(): T | undefined
}

type ALSConstructor = new <T>() => ALSInstance<T>

// One shared promise for the entire module — resolved on first use.
let _alsPromise: Promise<ALSConstructor | null> | undefined

function getALSPromise(): Promise<ALSConstructor | null> {
  if (_alsPromise === undefined) {
    // The string indirection hides the specifier from bundler static analysis,
    // preventing esbuild / tsup from treating 'node:async_hooks' as a
    // resolvable import that gets inlined or rewritten.
    const specifier = 'node:async_hooks'
    const dynImport = (s: string) =>
      // eslint-disable-next-line @typescript-eslint/no-implied-eval
      import(/* @vite-ignore */ s) as Promise<{ AsyncLocalStorage: ALSConstructor }>
    _alsPromise = dynImport(specifier)
      .then((m) => m.AsyncLocalStorage as ALSConstructor)
      .catch(() => null)
  }
  return _alsPromise
}

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
  private als: ALSInstance<ScopeFrame> | null = null
  private alsLoading: Promise<void> | null = null
  private fallbackFrame: ScopeFrame | null = null
  private warned = false
  private warnFn: (msg: string) => void

  constructor(opts: GovernanceContextOptions = {}) {
    const tryLoad = opts.alsAvailable ?? true
    this.warnFn = opts.warn ?? ((msg) => console.warn(`[jamjet] ${msg}`))

    if (tryLoad) {
      this.alsLoading = getALSPromise().then((Ctor) => {
        if (Ctor) {
          try {
            this.als = new Ctor<ScopeFrame>()
          } catch {
            this.als = null
          }
        }
      })
    }
    // When tryLoad is false (alsAvailable: false), alsLoading stays null and
    // als stays null — the fallback path is used immediately.
  }

  getCurrentContext(): ScopeFrame | null {
    // Synchronous: this.als is populated after the first runInContext awaits
    // the loading promise.  Before that, this.fallbackFrame (or null) is returned.
    if (this.als) return this.als.getStore() ?? this.fallbackFrame
    return this.fallbackFrame
  }

  async runInContext<T>(frame: ScopeFrame, fn: () => T | Promise<T>): Promise<T> {
    // Await the lazy ALS load exactly once — subsequent calls are instant.
    if (this.alsLoading) {
      await this.alsLoading
      this.alsLoading = null
    }

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

import { estimateCost } from '@jamjet/cloud'

// SHA-256 of the empty string — produced when there is no prompt content to hash.
// Never report this as waste since it carries no useful signal.
const EMPTY_SENTINEL = 'e3b0c44298fc1c14'

interface Bucket {
  repeats: number
  inputTokens: number
}

export interface WasteEntry {
  prefixHash: string
  repeats: number
  rePaidTokens: number
  wastedCents: number
}

export class WasteTracker {
  private readonly model: string
  private readonly buckets = new Map<string, Bucket>()

  constructor(model: string) {
    this.model = model
  }

  /** Record a call whose prompt shared the given prefix hash. */
  record(prefixHash: string, inputTokens: number): void {
    const existing = this.buckets.get(prefixHash)
    if (existing === undefined) {
      this.buckets.set(prefixHash, { repeats: 1, inputTokens })
    } else {
      existing.repeats += 1
      existing.inputTokens = inputTokens
    }
  }

  /**
   * Returns waste entries for prefix hashes that were seen 2+ times,
   * excluding the empty sentinel hash.
   *
   * rePaidTokens = inputTokens * (repeats - 1)  — tokens that could have been
   *   served from cache on the 2nd+ call but were re-sent as regular input.
   * wastedCents  = estimated cost of those avoidable re-paid tokens.
   */
  detect(): WasteEntry[] {
    const entries: WasteEntry[] = []
    for (const [prefixHash, bucket] of Array.from(this.buckets)) {
      if (prefixHash === EMPTY_SENTINEL) continue
      if (bucket.repeats < 2) continue
      const rePaidTokens = bucket.inputTokens * (bucket.repeats - 1)
      const wastedCents = estimateCost(this.model, rePaidTokens, 0) * 100
      entries.push({ prefixHash, repeats: bucket.repeats, rePaidTokens, wastedCents })
    }
    return entries
  }
}

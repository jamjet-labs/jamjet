export type CostEvent =
  | { kind: 'waste_detected'; prefixHash: string; repeats: number; rePaidTokens: number; wastedCents: number }
  | { kind: 'cache_saved'; savedCents: number; cacheReadTokens: number }
  | { kind: 'cost'; cents: number; model: string; inTok: number; outTok: number }
  | { kind: 'budget_exceeded'; spentCents: number; capCents: number }

export type GovEvent =
  | { kind: 'redaction'; type: string; count: number }
  | { kind: 'policy_blocked'; tool: string }
  | { kind: 'approval_required'; id: string; tool: string }
  | { kind: 'approval_resolved'; id: string; decision: 'approved' | 'rejected' }
  | { kind: 'audit'; id: string; tool: string }

export type FeatureEvent = CostEvent | GovEvent

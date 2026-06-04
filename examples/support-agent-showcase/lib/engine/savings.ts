// Per-MTok savings = input price − cache_read price.
// Values replicated from sdk/typescript/packages/cloud/src/enforcement.ts
// (CACHE_SAVING_PER_MTOK table; mirrors pricing.rs v_2026_05_17).
const CACHE_SAVING_PER_MTOK: Record<string, number> = {
  'claude-opus-4-7': 15.0 - 1.5,          // 13.5 USD/MTok saved
  'claude-sonnet-4-6': 3.0 - 0.3,         //  2.7 USD/MTok saved
  'claude-haiku-4-5': 1.0 - 0.1,          //  0.9 USD/MTok saved
  'claude-haiku-4-5-20251001': 1.0 - 0.1, //  0.9 USD/MTok saved
}

/**
 * Returns the estimated savings in cents from Anthropic prompt-cache reads.
 * Returns 0 for unknown models or zero token counts.
 */
export function cacheReadSavingsCents(model: string, cacheReadTokens: number): number {
  if (cacheReadTokens === 0) return 0
  const perMTok = CACHE_SAVING_PER_MTOK[model] ?? 0
  if (perMTok === 0) return 0
  return (cacheReadTokens / 1_000_000) * perMTok * 100
}

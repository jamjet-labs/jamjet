// USD per million tokens: [input, output, cacheRead?]. Must equal
// sdk/conformance/cloud-sdk-vectors.json `prices.models` (tests compare them),
// which also records where the prices come from. cacheRead is present only
// where the price is published.
export const PRICES_PER_MTOK: Readonly<Record<string, readonly [number, number, number?]>> = {
  'gpt-4o': [2.5, 10],
  'gpt-4o-mini': [0.15, 0.6],
  'gpt-4-turbo': [10, 30],
  'gpt-4': [30, 60],
  'gpt-3.5-turbo': [0.5, 1.5],
  'claude-fable-5-1': [10, 50, 0.25],
  'claude-fable-5': [10, 50],
  'claude-opus-5-5': [4, 20, 0.2],
  'claude-opus-5': [5, 25],
  'claude-opus-4-8': [5, 25],
  'claude-opus-4-7': [5, 25, 0.5],
  'claude-opus-4-6': [5, 25, 0.5],
  'claude-opus-4-5': [5, 25, 0.5],
  'claude-opus-4-20250514': [15, 75],
  'claude-sonnet-5-5': [2, 10, 0.2],
  'claude-sonnet-5': [2, 10],
  'claude-sonnet-4-6': [3, 15, 0.3],
  'claude-sonnet-4-20250514': [3, 15],
  'claude-haiku-4-5': [1, 5, 0.1],
  'claude-haiku-4-5-20251001': [1, 5, 0.1],
  'claude-3-5-haiku-20241022': [0.8, 4],
  'claude-3-haiku-20240307': [0.25, 1.25],
}

const FALLBACK_PER_MTOK: readonly [number, number] = [3, 15]

/**
 * The table key for a model ID: the ID itself when listed, else the ID with a
 * provider prefix, a Bedrock version (-v1:0), a Vertex @YYYYMMDD, an Anthropic
 * -YYYYMMDD and an OpenAI -YYYY-MM-DD or -MMDD date removed.
 */
export function normalizeModel(model: string): string {
  if (model in PRICES_PER_MTOK) return model
  let m = model.trim().replace(/^(openai|anthropic)[/.]/, '')
  if (m in PRICES_PER_MTOK) return m
  m = m
    .replace(/-v\d+(:\d+)?$/, '')
    .replace(/@\d{8}$/, '')
    .replace(/-\d{4}-\d{2}-\d{2}$/, '')
    .replace(/-\d{8}$/, '')
    .replace(/-\d{4}$/, '')
  return m
}

function pricesFor(model: string): readonly [number, number, number?] | undefined {
  return PRICES_PER_MTOK[model] ?? PRICES_PER_MTOK[normalizeModel(model)]
}

/**
 * Estimated USD for a call. `cache` is Anthropic's prompt-cache usage, billed
 * on top of `inputTokens`: reads at the listed cache-read price (0.1x input
 * when none is listed), writes at 1.25x input.
 */
export function estimateCost(
  model: string,
  inputTokens: number,
  outputTokens: number,
  cache: { read?: number; write?: number } = {},
): number {
  const prices = pricesFor(model)
  const [input, output] = prices ?? FALLBACK_PER_MTOK
  const cacheRead = prices?.[2] ?? input * 0.1
  return (
    (inputTokens * input + outputTokens * output + (cache.read ?? 0) * cacheRead + (cache.write ?? 0) * input * 1.25) /
    1_000_000
  )
}

/** USD saved by reading `cacheReadTokens` from the prompt cache instead of as fresh input. */
export function cacheReadSavingsUsd(model: string, cacheReadTokens: number): number {
  const prices = pricesFor(model)
  if (prices === undefined || prices[2] === undefined) return 0
  return (cacheReadTokens * (prices[0] - prices[2])) / 1_000_000
}

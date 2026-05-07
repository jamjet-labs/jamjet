const COST_PER_TOKEN: Record<string, readonly [number, number]> = {
  'gpt-4o': [2.5e-6, 10e-6],
  'gpt-4o-mini': [0.15e-6, 0.6e-6],
  'gpt-4-turbo': [10e-6, 30e-6],
  'gpt-4': [30e-6, 60e-6],
  'gpt-3.5-turbo': [0.5e-6, 1.5e-6],
  'claude-sonnet-4-6': [3e-6, 15e-6],
  'claude-sonnet-4-20250514': [3e-6, 15e-6],
  'claude-opus-4-6': [15e-6, 75e-6],
  'claude-opus-4-20250514': [15e-6, 75e-6],
  'claude-3-5-haiku-20241022': [0.8e-6, 4e-6],
  'claude-3-haiku-20240307': [0.25e-6, 1.25e-6],
}

const FALLBACK_RATES: readonly [number, number] = [3e-6, 15e-6]

export function estimateCost(model: string, inputTokens: number, outputTokens: number): number {
  const rates = COST_PER_TOKEN[model] ?? FALLBACK_RATES
  return inputTokens * rates[0] + outputTokens * rates[1]
}

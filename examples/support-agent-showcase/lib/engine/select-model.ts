/**
 * select-model.ts
 *
 * Returns the live model function when ANTHROPIC_API_KEY is present in the
 * environment, otherwise falls back to the deterministic mock. The names of
 * the returned functions ('mockModel' / 'liveModel') are used by tests to
 * verify which path was chosen.
 */

import { mockModel } from './model-mock.js'
import { liveModel } from './model-live.js'

export function selectModel(): typeof mockModel | typeof liveModel {
  if (process.env.ANTHROPIC_API_KEY) {
    return liveModel
  }
  return mockModel
}

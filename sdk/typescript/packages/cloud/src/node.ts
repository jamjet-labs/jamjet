import { init as universalInit } from './init.js'
import type { InitOptions } from './config.js'
import { patchOpenAI } from './patcher/openai.js'
import { patchAnthropic } from './patcher/anthropic.js'

export async function init(opts: InitOptions): Promise<void> {
  await universalInit(opts)
  await tryPatchOpenAI()
  await tryPatchAnthropic()
}

async function tryPatchOpenAI(): Promise<void> {
  try {
    const mod = await import('openai').catch(() => null)
    if (mod) patchOpenAI(mod as any)
  } catch {
    // openai not installed — fine.
  }
}

async function tryPatchAnthropic(): Promise<void> {
  try {
    const mod = await import('@anthropic-ai/sdk').catch(() => null)
    if (mod) patchAnthropic(mod as any)
  } catch {
    // anthropic not installed — fine.
  }
}

export { wrap } from './wrap.js'
export { VERSION } from './index.js'
export type { InitOptions } from './config.js'

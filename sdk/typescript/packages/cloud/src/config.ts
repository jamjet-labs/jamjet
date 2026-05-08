import { z } from 'zod'

const SamplingSchema = z.object({
  rate: z.number().min(0).max(1).default(1.0),
  alwaysKeepErrors: z.boolean().default(true),
  alwaysKeepApprovals: z.boolean().default(true),
})

const RedactionSchema = z.object({
  mode: z.enum(['strict', 'standard', 'off']).default('standard'),
  custom: z.array(z.instanceof(RegExp)).default([]),
})

export const InitOptionsSchema = z.object({
  apiKey: z.string().min(1).optional(),
  project: z.string().min(1, 'project is required'),
  agent: z.string().min(1).default('default'),
  environment: z.string().optional(),
  releaseVersion: z.string().optional(),
  apiUrl: z.string().url().optional(),
  sampling: SamplingSchema.optional().default({}),
  redaction: RedactionSchema.optional().default({}),
  debug: z.boolean().default(false),
  ctx: z.unknown().optional(),
  maxCostUsd: z.number().positive().optional(),
})

export type InitOptions = z.input<typeof InitOptionsSchema>
export type ResolvedConfig = z.output<typeof InitOptionsSchema> & { apiKey: string; apiUrl: string; maxCostUsd?: number }

export class ConfigError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ConfigError'
  }
}

const DEFAULT_API_URL = 'https://api.jamjet.dev'

export function resolveConfig(opts: InitOptions): ResolvedConfig {
  let parsed: z.output<typeof InitOptionsSchema>
  try {
    parsed = InitOptionsSchema.parse(opts)
  } catch (err) {
    if (err instanceof z.ZodError) {
      throw new ConfigError(
        `Invalid JamJet init options: ${err.errors
          .map((e) => `${e.path.join('.')}: ${e.message}`)
          .join('; ')}`,
      )
    }
    throw err
  }

  const apiKey = parsed.apiKey ?? process.env.JAMJET_API_KEY
  if (!apiKey) {
    throw new ConfigError(
      'JamJet apiKey not provided. Pass `apiKey` to init() or set JAMJET_API_KEY env var.',
    )
  }

  const apiUrl = parsed.apiUrl ?? process.env.JAMJET_API_URL ?? DEFAULT_API_URL

  return { ...parsed, apiKey, apiUrl }
}

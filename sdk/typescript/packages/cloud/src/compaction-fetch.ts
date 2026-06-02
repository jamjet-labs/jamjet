import type { CompactionRule } from './compaction.js'

interface FetchOpts {
  apiBase: string
  token: string
  fetchImpl?: typeof fetch
}

/** Pulls active tool_compaction policy rules from Cloud. Fail-open: any
 * error returns [] so a cloud hiccup never breaks the user's LLM calls. */
export async function fetchCompactionRules(opts: FetchOpts): Promise<CompactionRule[]> {
  const f = opts.fetchImpl ?? fetch
  try {
    const res = await f(`${opts.apiBase}/v1/policies?kind=tool_compaction&active=true`, {
      headers: { Authorization: `Bearer ${opts.token}` },
    })
    if (!res.ok) return []
    const body = (await res.json()) as {
      policies?: Array<{ pattern?: string; config?: { max_result_tokens?: number } }>
    }
    return (body.policies ?? [])
      .map((p) => ({
        toolPattern: p.pattern ?? '',
        maxResultTokens: Number(p.config?.max_result_tokens ?? 0),
      }))
      .filter((r) => r.toolPattern !== '' && r.maxResultTokens > 0)
  } catch {
    return []
  }
}

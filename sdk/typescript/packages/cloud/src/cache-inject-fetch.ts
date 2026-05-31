interface FetchOpts {
  apiBase: string
  token: string
  fetchImpl?: typeof fetch
}
/** Pulls active cache_inject policy prefix-hashes from Cloud. Fail-open: any
 * error returns [] so a cloud hiccup never breaks the user's LLM calls. */
export async function fetchCacheInjectHashes(opts: FetchOpts): Promise<string[]> {
  const f = opts.fetchImpl ?? fetch
  try {
    const res = await f(`${opts.apiBase}/v1/policies?kind=cache_inject&active=true`, {
      headers: { Authorization: `Bearer ${opts.token}` },
    })
    if (!res.ok) return []
    const body = (await res.json()) as { policies?: Array<{ pattern: string }> }
    return (body.policies ?? []).map((p) => p.pattern).filter((h): h is string => typeof h === 'string')
  } catch {
    return []
  }
}

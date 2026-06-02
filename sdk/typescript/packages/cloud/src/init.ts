import { Client, resetActive, setActive } from './client.js'
import { fetchCacheInjectHashes } from './cache-inject-fetch.js'
import { CacheInjectResolver } from './cache-inject.js'
import { resolveConfig, type InitOptions } from './config.js'

export async function init(opts: InitOptions): Promise<void> {
  const config = resolveConfig(opts)
  await resetActive()
  const client = new Client(config)
  setActive(client)
  void fetchCacheInjectHashes({ apiBase: config.apiUrl, token: config.apiKey })
    .then((hashes) => { client._cacheInject = new CacheInjectResolver(hashes) })
  await readinessCheck(config.apiUrl, config.project, config.apiKey, config.debug)
}

async function readinessCheck(
  apiUrl: string,
  project: string,
  apiKey: string,
  debug: boolean,
): Promise<void> {
  try {
    const url = `${apiUrl.replace(/\/$/, '')}/v1/projects/${encodeURIComponent(project)}/readiness`
    const res = await fetch(url, {
      headers: { Authorization: `Bearer ${apiKey}` },
      signal: AbortSignal.timeout(3000),
    })
    if (!res.ok) {
      if (debug) {
        console.warn(`[jamjet] readiness check failed: HTTP ${res.status}`)
      } else {
        console.warn(`[jamjet] readiness check returned ${res.status} — spans may be rejected`)
      }
    }
  } catch (err) {
    console.warn('[jamjet] readiness check error:', (err as Error).message)
  }
}

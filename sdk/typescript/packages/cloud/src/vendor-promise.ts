/**
 * Vendor SDK helpers chain methods onto the promise that `create()` returns:
 * Anthropic's `messages.stream()` calls `.withResponse()`, and OpenAI's
 * `chat.completions.parse()` calls `._thenUnwrap()`; callers use
 * `.asResponse()`. A patched `create` that returns a bare Promise breaks all
 * three, so the enforced promise carries them, backed by the enforced result.
 *
 * `raw` returns the vendor's own promise for the call (undefined if the call
 * never reached the vendor, e.g. refused before sending).
 */
export function asVendorPromise<T>(enforced: Promise<T>, raw: () => unknown): Promise<T> {
  const p = enforced as Promise<T> & Record<string, unknown>
  p['withResponse'] = async () => {
    const data = await enforced
    const vendor = raw() as { withResponse?: () => Promise<Record<string, unknown>> } | undefined
    const extra = typeof vendor?.withResponse === 'function' ? await vendor.withResponse() : {}
    return { ...extra, data }
  }
  // The raw Response's body was already consumed (and, for streams, would
  // skip enforcement), so build a new one from the enforced result.
  p['asResponse'] = async () => {
    const data = await enforced
    const vendor = raw() as { asResponse?: () => Promise<Response> } | undefined
    const original = typeof vendor?.asResponse === 'function' ? await vendor.asResponse() : undefined
    const headers = new Headers(original?.headers)
    headers.delete('content-length')
    headers.delete('content-encoding')
    const stream = data as { toReadableStream?: () => ReadableStream }
    if (typeof stream?.toReadableStream === 'function') {
      headers.set('content-type', 'application/x-ndjson')
      return new Response(stream.toReadableStream(), { status: original?.status ?? 200, headers })
    }
    headers.set('content-type', 'application/json')
    return new Response(JSON.stringify(data), { status: original?.status ?? 200, headers })
  }
  p['_thenUnwrap'] = <U>(transform: (data: T) => U) =>
    asVendorPromise(
      enforced.then((data) => transform(data)),
      raw,
    )
  return p
}

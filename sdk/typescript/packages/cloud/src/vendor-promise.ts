/**
 * Vendor SDK helpers chain methods onto the promise that `create()` returns:
 * Anthropic's `messages.stream()` calls `.withResponse()`, and OpenAI's
 * `chat.completions.parse()` calls `._thenUnwrap()`. A patched `create` that
 * returns a bare Promise breaks both helpers, so the enforced promise carries
 * the same methods, backed by the enforced result.
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
  p['_thenUnwrap'] = <U>(transform: (data: T) => U) =>
    asVendorPromise(
      enforced.then((data) => transform(data)),
      raw,
    )
  return p
}

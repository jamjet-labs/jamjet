/**
 * Vendor SDK helpers chain methods onto the promise that `create()` returns:
 * Anthropic's `messages.stream()` calls `.withResponse()`, and OpenAI's
 * `chat.completions.parse()` calls `._thenUnwrap()`; callers use
 * `.asResponse()`. A patched `create` that returns a bare Promise breaks all
 * three, so the enforced promise carries them, backed by the enforced result.
 *
 * The vendor's own Response is never handed out with its body: for a stream
 * that body is the raw event stream, which would skip enforcement. The
 * Response these methods return is rebuilt from the enforced result, in the
 * vendor's wire format (JSON, or server-sent events for a stream).
 *
 * `raw` returns the vendor's own promise for the call (undefined if the call
 * never reached the vendor, e.g. refused before sending).
 */
export function asVendorPromise<T>(
  enforced: Promise<T>,
  raw: () => unknown,
  vendor: 'openai' | 'anthropic',
): Promise<T> {
  const p = enforced as Promise<T> & Record<string, unknown>
  const rebuilt = async (data: T): Promise<Response> => {
    const v = raw() as { asResponse?: () => Promise<Response> } | undefined
    const original = typeof v?.asResponse === 'function' ? await v.asResponse() : undefined
    return enforcedResponse(data, original, vendor)
  }
  p['withResponse'] = async () => {
    const data = await enforced
    const v = raw() as { withResponse?: () => Promise<Record<string, unknown>> } | undefined
    const extra = typeof v?.withResponse === 'function' ? await v.withResponse() : {}
    return { ...extra, data, response: await rebuilt(data) }
  }
  p['asResponse'] = async () => rebuilt(await enforced)
  p['_thenUnwrap'] = <U>(transform: (data: T) => U) =>
    asVendorPromise(
      enforced.then((data) => transform(data)),
      raw,
      vendor,
    )
  return p
}

function enforcedResponse(data: unknown, original: Response | undefined, vendor: 'openai' | 'anthropic'): Response {
  const headers = new Headers(original?.headers)
  headers.delete('content-length')
  headers.delete('content-encoding')
  const init = { status: original?.status ?? 200, headers }
  const stream = data as { [Symbol.asyncIterator]?: () => AsyncIterator<unknown> } | null
  if (stream !== null && typeof stream === 'object' && typeof stream[Symbol.asyncIterator] === 'function') {
    headers.set('content-type', 'text/event-stream')
    return new Response(sse(stream as AsyncIterable<unknown>, vendor), init)
  }
  headers.set('content-type', 'application/json')
  return new Response(JSON.stringify(data), init)
}

/** The enforced stream re-encoded as the vendor's server-sent events. */
function sse(stream: AsyncIterable<unknown>, vendor: 'openai' | 'anthropic'): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder()
  let iterator: AsyncIterator<unknown> | undefined
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      iterator ??= stream[Symbol.asyncIterator]()
      try {
        const { value, done } = await iterator.next()
        if (done) {
          if (vendor === 'openai') controller.enqueue(encoder.encode('data: [DONE]\n\n'))
          controller.close()
          return
        }
        const type = (value as { type?: unknown } | null)?.type
        const event = vendor === 'anthropic' && typeof type === 'string' ? `event: ${type}\n` : ''
        controller.enqueue(encoder.encode(`${event}data: ${JSON.stringify(value)}\n\n`))
      } catch (err) {
        controller.error(err)
      }
    },
    async cancel() {
      await iterator?.return?.()
    },
  }, { highWaterMark: 0 }) // read nothing until the body is read: `data` may be iterated instead
}

// Policy and budget enforcement for `stream: true` calls.
//
// A streaming create() resolves to an async iterable of chunks (OpenAI) or
// events (Anthropic) before the model has produced anything, so the checks a
// non-streaming call runs on the finished response have to run on the stream
// instead. The enforced iterator:
//
//   - passes text through as it arrives;
//   - holds every chunk from the first tool call onward, so a tool call the
//     policy refuses never reaches the caller (OpenAI can rename a call in a
//     later delta, and approval needs the full arguments);
//   - at the end of the stream, hands the finished tool calls to `onToolCalls`
//     (which throws to refuse them) and only then releases the held chunks;
//   - records usage and cost once, whether the stream ends, fails, or the
//     caller stops reading early.

export type Vendor = 'openai' | 'anthropic'

export interface StreamedToolCall {
  id?: string
  name: string
  /** Raw argument text as streamed (OpenAI JSON string / Anthropic input_json deltas). */
  arguments: string
}

export interface StreamTally {
  model: string | undefined
  inputTokens: number | undefined
  outputTokens: number | undefined
  cacheReadTokens: number
  /** Characters of generated text and tool arguments; used when the stream reports no usage. */
  outputChars: number
  toolCalls: StreamedToolCall[]
}

export type StreamEnd =
  | { kind: 'complete' }
  | { kind: 'incomplete' }
  | { kind: 'error'; error: unknown }

export interface StreamEnforcementHooks {
  vendor: Vendor
  /** Hold tool-call chunks back until the calls are checked. Off when no policy rule exists. */
  holdToolCalls: boolean
  /** Throw to refuse the calls; the held chunks are then never released. */
  onToolCalls: (calls: StreamedToolCall[], tally: StreamTally) => void | Promise<void>
  /** Anthropic names a tool in full at content_block_start; throw here to stop early. */
  onToolCallStarted?: (call: { id?: string; name: string }, tally: StreamTally) => void
  /** Called exactly once with what the stream reported. */
  onEnd: (tally: StreamTally, end: StreamEnd) => void
}

function num(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined
}

class Tally implements StreamTally {
  model: string | undefined = undefined
  inputTokens: number | undefined = undefined
  outputTokens: number | undefined = undefined
  cacheReadTokens = 0
  outputChars = 0
  toolCalls: StreamedToolCall[] = []
  private byKey = new Map<string, StreamedToolCall>()

  private call(key: string): StreamedToolCall {
    let entry = this.byKey.get(key)
    if (!entry) {
      entry = { name: '', arguments: '' }
      this.byKey.set(key, entry)
      this.toolCalls.push(entry)
    }
    return entry
  }

  /**
   * Fold one chunk into the tally. Returns whether the chunk carries tool-call
   * content, and the Anthropic tool call it starts, if any.
   */
  observe(vendor: Vendor, chunk: unknown): { toolChunk: boolean; started?: { id?: string; name: string } } {
    if (typeof chunk !== 'object' || chunk === null) return { toolChunk: false }
    const c = chunk as Record<string, any>
    return vendor === 'openai' ? this.observeOpenAI(c) : this.observeAnthropic(c)
  }

  private observeOpenAI(c: Record<string, any>): { toolChunk: boolean } {
    if (typeof c['model'] === 'string') this.model = c['model']
    const usage = c['usage']
    if (usage && typeof usage === 'object') {
      this.inputTokens = num(usage['prompt_tokens']) ?? this.inputTokens
      this.outputTokens = num(usage['completion_tokens']) ?? this.outputTokens
    }
    let toolChunk = false
    const choices = Array.isArray(c['choices']) ? c['choices'] : []
    for (const choice of choices) {
      const delta = (choice?.['delta'] ?? {}) as Record<string, any>
      if (typeof delta['content'] === 'string') this.outputChars += delta['content'].length
      const calls = Array.isArray(delta['tool_calls']) ? delta['tool_calls'] : []
      for (const tc of calls) {
        toolChunk = true
        const entry = this.call(`${choice?.['index'] ?? 0}:${tc?.['index'] ?? 0}`)
        if (typeof tc?.['id'] === 'string') entry.id = tc['id']
        const fn = (tc?.['function'] ?? tc?.['custom'] ?? {}) as Record<string, unknown>
        // Mirrors the openai SDK's own accumulator: a non-empty name replaces,
        // argument text appends.
        if (typeof fn['name'] === 'string' && fn['name'] !== '') entry.name = fn['name']
        for (const key of ['arguments', 'input']) {
          const text = fn[key]
          if (typeof text === 'string') {
            entry.arguments += text
            this.outputChars += text.length
          }
        }
      }
    }
    return { toolChunk }
  }

  private observeAnthropic(c: Record<string, any>): { toolChunk: boolean; started?: { id?: string; name: string } } {
    switch (c['type']) {
      case 'message_start': {
        const message = (c['message'] ?? {}) as Record<string, any>
        if (typeof message['model'] === 'string') this.model = message['model']
        const usage = (message['usage'] ?? {}) as Record<string, unknown>
        this.inputTokens = num(usage['input_tokens']) ?? this.inputTokens
        this.outputTokens = num(usage['output_tokens']) ?? this.outputTokens
        this.cacheReadTokens = num(usage['cache_read_input_tokens']) ?? this.cacheReadTokens
        return { toolChunk: false }
      }
      case 'message_delta': {
        const usage = (c['usage'] ?? {}) as Record<string, unknown>
        this.outputTokens = num(usage['output_tokens']) ?? this.outputTokens
        this.inputTokens = num(usage['input_tokens']) ?? this.inputTokens
        this.cacheReadTokens = num(usage['cache_read_input_tokens']) ?? this.cacheReadTokens
        return { toolChunk: false }
      }
      case 'content_block_start': {
        const block = (c['content_block'] ?? {}) as Record<string, any>
        if (block['type'] === 'tool_use' || block['type'] === 'server_tool_use') {
          const entry = this.call(String(c['index'] ?? 0))
          entry.name = typeof block['name'] === 'string' ? block['name'] : ''
          if (typeof block['id'] === 'string') entry.id = block['id']
          const started: { id?: string; name: string } = { name: entry.name }
          if (entry.id !== undefined) started.id = entry.id
          return { toolChunk: true, started }
        }
        return { toolChunk: false }
      }
      case 'content_block_delta': {
        const delta = (c['delta'] ?? {}) as Record<string, any>
        if (typeof delta['text'] === 'string') this.outputChars += delta['text'].length
        if (typeof delta['partial_json'] === 'string') {
          this.outputChars += delta['partial_json'].length
          const entry = this.byKey.get(String(c['index'] ?? 0))
          if (entry) {
            entry.arguments += delta['partial_json']
            return { toolChunk: true }
          }
        }
        return { toolChunk: false }
      }
      default:
        return { toolChunk: false }
    }
  }
}

function enforcedIterator(
  inner: AsyncIterator<unknown>,
  hooks: StreamEnforcementHooks,
): AsyncGenerator<unknown> {
  const tally = new Tally()
  let ended = false
  const end = (how: StreamEnd): void => {
    if (ended) return
    ended = true
    hooks.onEnd(tally, how)
  }

  return (async function* () {
    const held: unknown[] = []
    let holding = false
    let upstreamDone = false
    try {
      while (true) {
        const step = await inner.next()
        if (step.done) {
          upstreamDone = true
          break
        }
        const { toolChunk, started } = tally.observe(hooks.vendor, step.value)
        if (started !== undefined) hooks.onToolCallStarted?.(started, tally)
        if (hooks.holdToolCalls && toolChunk) holding = true
        if (holding) held.push(step.value)
        else yield step.value
      }
      await hooks.onToolCalls(
        tally.toolCalls.filter((c) => c.name !== '' || c.arguments !== ''),
        tally,
      )
      end({ kind: 'complete' })
      for (const value of held) yield value
    } catch (err) {
      end({ kind: 'error', error: err })
      throw err
    } finally {
      if (!upstreamDone) {
        // The caller stopped reading, or a check refused the stream: close the
        // upstream request so the model stops generating.
        try {
          await inner.return?.()
        } catch {
          // closing is best effort
        }
      }
      end({ kind: 'incomplete' })
    }
  })()
}

/**
 * Route every read of `stream` through the enforced iterator, in place, so the
 * object keeps its class, its `controller`, and helpers such as `tee()` and
 * `toReadableStream()`. Returns null when `stream` is not async-iterable.
 */
export function enforceStream<T>(stream: T, hooks: StreamEnforcementHooks): T | null {
  if (typeof stream !== 'object' || stream === null) return null
  const s = stream as Record<string | symbol, any>
  // Stainless SDK streams (openai, @anthropic-ai/sdk) read through an own
  // `iterator` property; `[Symbol.asyncIterator]`, `tee()` and
  // `toReadableStream()` all call it.
  if (typeof s['iterator'] === 'function') {
    const original = s['iterator'] as () => AsyncIterator<unknown>
    s['iterator'] = () => enforcedIterator(original.call(stream), hooks)
    return stream
  }
  const iterate = s[Symbol.asyncIterator]
  if (typeof iterate !== 'function') return null
  Object.defineProperty(stream, Symbol.asyncIterator, {
    value: () => enforcedIterator(iterate.call(stream), hooks),
    configurable: true,
    writable: true,
  })
  return stream
}

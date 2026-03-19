import { useState, useEffect } from 'react'
import { useInspectorStore } from '@/store/inspector'
import { useExecution, useEvents } from '@/hooks/useExecution'
import { cn } from '@/lib/utils'
import type { NodeStartedEvent, NodeCompletedEvent } from '@/api/types'

// ─── Types ────────────────────────────────────────────────────────────────────

interface ReplayResult {
  output: unknown
  error?: string
  unavailable?: boolean
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

function extractNodeInput(
  events: import('@/api/types').Event[],
  nodeId: string,
): unknown | undefined {
  for (const ev of events) {
    if (ev.kind.type === 'node_started' && (ev.kind as NodeStartedEvent).node_id === nodeId) {
      // The node_started event may carry an `input` field on the kind payload.
      const kind = ev.kind as NodeStartedEvent & { input?: unknown }
      return kind.input
    }
  }
  return undefined
}

function isNodeCompleted(
  events: import('@/api/types').Event[],
  nodeId: string,
): boolean {
  return events.some(
    (ev) =>
      (ev.kind.type === 'node_completed' || ev.kind.type === 'node_failed') &&
      (ev.kind as NodeCompletedEvent).node_id === nodeId,
  )
}

function safeStringify(value: unknown): string {
  if (value === undefined) return ''
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

function isDifferent(original: unknown, next: unknown): boolean {
  return JSON.stringify(original) !== JSON.stringify(next)
}

// ─── Component ────────────────────────────────────────────────────────────────

export function ReplayControls() {
  const selectedExecutionId = useInspectorStore((s) => s.selectedExecutionId)
  const selectedNodeId = useInspectorStore((s) => s.selectedNodeId)

  const { data: execution } = useExecution(selectedExecutionId)
  const { data: events = [] } = useEvents(selectedExecutionId)

  const [inputJson, setInputJson] = useState('')
  const [inputError, setInputError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [result, setResult] = useState<ReplayResult | null>(null)
  const [originalOutput, setOriginalOutput] = useState<unknown>(undefined)
  const [editOpen, setEditOpen] = useState(false)

  // Derive original node input whenever the selected node changes.
  useEffect(() => {
    if (!selectedNodeId) return
    const raw = extractNodeInput(events, selectedNodeId)
    setInputJson(safeStringify(raw))
    setResult(null)
    setInputError(null)
    setEditOpen(false)

    // Also capture original output from node_completed event if present.
    const completedEv = events.find(
      (ev) =>
        ev.kind.type === 'node_completed' &&
        (ev.kind as NodeCompletedEvent).node_id === selectedNodeId,
    )
    if (completedEv) {
      const kind = completedEv.kind as NodeCompletedEvent & { output?: unknown }
      setOriginalOutput(kind.output)
    } else {
      setOriginalOutput(undefined)
    }
  }, [selectedNodeId, events])

  // Guard: only show for completed nodes in a completed execution.
  if (!selectedExecutionId || !selectedNodeId) return null
  if (!isNodeCompleted(events, selectedNodeId)) return null
  // Only show for executions that have finished (completed or failed).
  if (execution && execution.status === 'running') return null

  // ── Replay handler ──────────────────────────────────────────────────────────

  async function handleReplay() {
    if (!selectedExecutionId || !selectedNodeId) return

    // Validate JSON if user edited it.
    let parsedInput: unknown = undefined
    if (inputJson.trim()) {
      try {
        parsedInput = JSON.parse(inputJson)
      } catch {
        setInputError('Invalid JSON — fix the input before replaying.')
        return
      }
    }
    setInputError(null)
    setLoading(true)
    setResult(null)

    // Capture the node we're replaying for, so we can discard stale responses.
    const replayNodeId = selectedNodeId

    try {
      const res = await fetch(`/api/executions/${encodeURIComponent(selectedExecutionId)}/replay`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          from_node: replayNodeId,
          ...(parsedInput !== undefined ? { input: parsedInput } : {}),
        }),
      })

      // Discard if the user selected a different node while we were waiting.
      if (useInspectorStore.getState().selectedNodeId !== replayNodeId) return

      if (res.status === 404 || res.status === 501) {
        setResult({ output: null, unavailable: true })
        return
      }

      if (!res.ok) {
        const text = await res.text().catch(() => res.statusText)
        setResult({ output: null, error: `Replay failed (${res.status}): ${text}` })
        return
      }

      const data = await res.json()
      setResult({ output: data })
    } catch (err) {
      // Discard if stale.
      if (useInspectorStore.getState().selectedNodeId !== replayNodeId) return

      // Network error — treat as unavailable if we can't reach the server.
      const msg = err instanceof Error ? err.message : String(err)
      if (msg.toLowerCase().includes('fetch') || msg.toLowerCase().includes('network')) {
        setResult({ output: null, unavailable: true })
      } else {
        setResult({ output: null, error: msg })
      }
    } finally {
      setLoading(false)
    }
  }

  // ── Derived display values ──────────────────────────────────────────────────

  const shortNodeId =
    selectedNodeId.length > 20 ? `…${selectedNodeId.slice(-18)}` : selectedNodeId

  const resultOutput = result?.output
  const outputDiffers =
    result && !result.error && !result.unavailable && isDifferent(originalOutput, resultOutput)

  // ── Render ──────────────────────────────────────────────────────────────────

  return (
    <div className="flex flex-wrap items-start gap-2 px-4 py-2 bg-zinc-900 border-b border-zinc-800 text-xs">
      {/* ── Replay button ───────────────────────────────────────────────── */}
      <button
        onClick={handleReplay}
        disabled={loading}
        title={`Replay execution from node ${selectedNodeId}`}
        className={cn(
          'inline-flex items-center gap-1.5 px-3 py-1.5 rounded font-medium transition-colors',
          'bg-indigo-600 text-white hover:bg-indigo-500 active:bg-indigo-700',
          'disabled:opacity-50 disabled:cursor-not-allowed',
        )}
      >
        {loading ? (
          <>
            <svg
              className="animate-spin h-3 w-3"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth={2}
            >
              <path d="M12 2v4M12 18v4M4.93 4.93l2.83 2.83M16.24 16.24l2.83 2.83M2 12h4M18 12h4M4.93 19.07l2.83-2.83M16.24 7.76l2.83-2.83" />
            </svg>
            Replaying…
          </>
        ) : (
          <>
            <span>&#9654;</span>
            Replay from:{' '}
            <code className="font-mono opacity-80">{shortNodeId}</code>
          </>
        )}
      </button>

      {/* ── Edit input toggle ───────────────────────────────────────────── */}
      <button
        onClick={() => setEditOpen((v) => !v)}
        title="Edit input JSON before replay"
        className={cn(
          'inline-flex items-center gap-1 px-3 py-1.5 rounded font-medium transition-colors',
          'border border-zinc-700 text-zinc-300 hover:bg-zinc-800',
          editOpen && 'bg-zinc-800 border-zinc-600',
        )}
      >
        ✏ Edit Input
      </button>

      {/* ── Input editor (inline) ───────────────────────────────────────── */}
      {editOpen && (
        <div className="w-full flex flex-col gap-1 mt-1">
          <textarea
            value={inputJson}
            onChange={(e) => {
              setInputJson(e.target.value)
              setInputError(null)
            }}
            rows={4}
            spellCheck={false}
            placeholder="Enter JSON input (leave empty to use original)"
            className={cn(
              'w-full font-mono text-xs bg-zinc-950 text-zinc-200 border rounded px-2 py-1.5 resize-y',
              'placeholder:text-zinc-600 focus:outline-none focus:ring-1',
              inputError
                ? 'border-red-600 focus:ring-red-600'
                : 'border-zinc-700 focus:ring-indigo-500',
            )}
          />
          {inputError && <p className="text-red-400">{inputError}</p>}
        </div>
      )}

      {/* ── Result area ─────────────────────────────────────────────────── */}
      {result && (
        <div className="w-full mt-1">
          {result.unavailable ? (
            <p className="text-zinc-500 italic">Replay API not available</p>
          ) : result.error ? (
            <p className="text-red-400">{result.error}</p>
          ) : (
            <div
              className={cn(
                'rounded border px-3 py-2 font-mono whitespace-pre-wrap break-all bg-zinc-950 text-zinc-200',
                outputDiffers ? 'border-amber-500' : 'border-zinc-700',
              )}
            >
              <span className="font-sans not-italic text-zinc-500 mr-2">Result:</span>
              {outputDiffers && (
                <span className="font-sans text-amber-400 mr-2">(differs from original)</span>
              )}
              {safeStringify(resultOutput)}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

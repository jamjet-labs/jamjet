import { useInspectorStore } from '@/store/inspector'
import { useEvents } from '@/hooks/useExecution'
import type { Event, EventKind } from '@/api/types'
import { cn } from '@/lib/utils'

// ─── State reconstruction ──────────────────────────────────────────────────────

function deepMerge(target: Record<string, unknown>, source: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = { ...target }
  for (const key of Object.keys(source)) {
    const sv = source[key]
    const tv = target[key]
    if (
      sv !== null &&
      typeof sv === 'object' &&
      !Array.isArray(sv) &&
      tv !== null &&
      typeof tv === 'object' &&
      !Array.isArray(tv)
    ) {
      result[key] = deepMerge(tv as Record<string, unknown>, sv as Record<string, unknown>)
    } else {
      result[key] = sv
    }
  }
  return result
}

function reconstructState(events: Event[], upToIndex: number): Record<string, unknown> {
  let state: Record<string, unknown> = {}

  for (let i = 0; i <= upToIndex && i < events.length; i++) {
    const ev = events[i]
    const kind = ev.kind as Record<string, unknown>

    if (kind['type'] === 'workflow_started') {
      const initial = kind['initial_input']
      if (initial !== null && initial !== undefined && typeof initial === 'object' && !Array.isArray(initial)) {
        state = deepMerge(state, initial as Record<string, unknown>)
      }
    }

    if (kind['type'] === 'node_completed') {
      const patch = kind['state_patch']
      if (patch !== null && patch !== undefined && typeof patch === 'object' && !Array.isArray(patch)) {
        state = deepMerge(state, patch as Record<string, unknown>)
      }
    }
  }

  return state
}

// ─── Component ────────────────────────────────────────────────────────────────

export function StateInspector() {
  const selectedExecutionId = useInspectorStore((s) => s.selectedExecutionId)
  const timelinePosition = useInspectorStore((s) => s.timelinePosition)
  const setTimelinePosition = useInspectorStore((s) => s.setTimelinePosition)

  const { data: events = [] } = useEvents(selectedExecutionId)

  if (!selectedExecutionId) {
    return (
      <div className="flex items-center justify-center h-full">
        <p className="text-zinc-600 text-sm">Select an execution to inspect state</p>
      </div>
    )
  }

  const total = events.length
  const position = timelinePosition !== null ? timelinePosition : total

  // Clamp position within valid range
  const clampedPosition = Math.max(0, Math.min(position, total))

  const currentEvent = clampedPosition > 0 ? events[clampedPosition - 1] : null
  const reconstructedState = reconstructState(events, clampedPosition - 1)

  const hasState = Object.keys(reconstructedState).length > 0

  return (
    <div className="flex flex-col h-full p-4 space-y-3">
      {/* ── Timeline scrubber ───────────────────────────────────────── */}
      <div className="space-y-1.5">
        <div className="flex items-center justify-between text-xs text-zinc-500">
          <span>Timeline</span>
          <span className="font-mono">
            {clampedPosition} / {total}
          </span>
        </div>
        <input
          type="range"
          min={0}
          max={total}
          value={clampedPosition}
          onChange={(e) => setTimelinePosition(Number(e.target.value))}
          className={cn(
            'w-full h-1.5 rounded-full appearance-none cursor-pointer',
            'bg-zinc-800 accent-zinc-400',
            total === 0 && 'opacity-40 cursor-not-allowed'
          )}
          disabled={total === 0}
        />
      </div>

      {/* ── Current event label ─────────────────────────────────────── */}
      {currentEvent ? (
        <div className="flex items-center gap-2 text-xs">
          <span className="text-zinc-500">seq</span>
          <span className="font-mono text-zinc-300">{currentEvent.sequence}</span>
          <span className="text-zinc-600">·</span>
          <span
            className={cn(
              'font-mono px-1.5 py-0.5 rounded text-xs',
              currentEvent.kind.type === 'node_completed'
                ? 'bg-emerald-950 text-emerald-400'
                : currentEvent.kind.type === 'node_failed'
                ? 'bg-red-950 text-red-400'
                : currentEvent.kind.type === 'workflow_started'
                ? 'bg-blue-950 text-blue-400'
                : currentEvent.kind.type === 'workflow_completed'
                ? 'bg-violet-950 text-violet-400'
                : 'bg-zinc-800 text-zinc-400'
            )}
          >
            {currentEvent.kind.type}
          </span>
        </div>
      ) : (
        <div className="text-xs text-zinc-600 italic">
          {total === 0 ? 'No events loaded' : 'Drag slider to inspect state'}
        </div>
      )}

      {/* ── JSON state tree ─────────────────────────────────────────── */}
      <div className="flex-1 overflow-auto">
        {hasState ? (
          <pre className="font-mono text-xs bg-zinc-950 p-3 rounded overflow-auto">
            {JSON.stringify(reconstructedState, null, 2)}
          </pre>
        ) : (
          <div className="font-mono text-xs bg-zinc-950 p-3 rounded text-zinc-600 italic">
            {clampedPosition === 0
              ? '// state is empty before the first event'
              : '// no state patches in events up to this point'}
          </div>
        )}
      </div>
    </div>
  )
}

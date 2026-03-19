import { useState } from 'react'
import { useInspectorStore } from '@/store/inspector'
import { useEvents } from '@/hooks/useExecution'
import type { Event, EventKind } from '@/api/types'
import { cn } from '@/lib/utils'

// ─── Constants ────────────────────────────────────────────────────────────────

const ALL_EVENT_TYPES: string[] = [
  'workflow_started',
  'workflow_completed',
  'workflow_failed',
  'node_scheduled',
  'node_started',
  'node_completed',
  'node_failed',
  'coordinator_discovery',
  'coordinator_scoring',
  'coordinator_decision',
  'agent_tool_invoked',
  'agent_tool_completed',
  'agent_tool_progress',
  'agent_turn',
  'agent_terminated',
  'agent_failed',
]

// ─── Helpers ──────────────────────────────────────────────────────────────────

function badgeClass(type: string): string {
  if (type.startsWith('coordinator_')) {
    return 'bg-purple-900 text-purple-300 border border-purple-700'
  }
  if (type.startsWith('agent_tool')) {
    return 'bg-blue-900 text-blue-300 border border-blue-700'
  }
  if (type === 'node_completed') {
    return 'bg-emerald-900 text-emerald-300 border border-emerald-700'
  }
  if (type === 'node_failed' || type === 'workflow_failed' || type === 'agent_failed') {
    return 'bg-red-900 text-red-300 border border-red-700'
  }
  // workflow_* and node_* (other)
  return 'bg-zinc-800 text-zinc-300 border border-zinc-600'
}

function isCoordinatorEvent(type: string): boolean {
  return type.startsWith('coordinator_')
}

function extractNodeId(kind: EventKind): string | null {
  const k = kind as Record<string, unknown>
  if (typeof k['node_id'] === 'string') return k['node_id']
  return null
}

function relativeTimestamp(baseIso: string, eventIso: string): string {
  const diffMs = new Date(eventIso).getTime() - new Date(baseIso).getTime()
  const sign = diffMs < 0 ? '-' : '+'
  const abs = Math.abs(diffMs)
  if (abs < 1000) return `${sign}${abs}ms`
  return `${sign}${(abs / 1000).toFixed(1)}s`
}

function extractSummary(kind: EventKind): string {
  const k = kind as Record<string, unknown>
  switch (kind.type) {
    case 'node_completed': {
      const dur = k['duration_ms']
      return dur !== undefined ? `completed in ${dur}ms` : 'completed'
    }
    case 'node_failed': {
      const err = typeof k['error'] === 'string' ? k['error'] : 'unknown error'
      return `failed: ${err}`
    }
    case 'coordinator_decision': {
      const selected = typeof k['selected'] === 'string' ? k['selected'] : '?'
      const method = typeof k['method'] === 'string' ? k['method'] : ''
      return method ? `selected ${selected} (${method})` : `selected ${selected}`
    }
    case 'agent_tool_invoked': {
      const mode = typeof k['mode'] === 'string' ? k['mode'] : 'invoke'
      const uri = typeof k['agent_uri'] === 'string' ? k['agent_uri'] : String(k['tool'] ?? '?')
      return `${mode} → ${uri}`
    }
    case 'agent_tool_completed': {
      const lat = k['latency_ms']
      return lat !== undefined ? `done in ${lat}ms` : 'done'
    }
    default:
      return kind.type
  }
}

// ─── Filter dropdown ──────────────────────────────────────────────────────────

function FilterDropdown({
  filter,
  onChange,
}: {
  filter: string[]
  onChange: (f: string[]) => void
}) {
  const [open, setOpen] = useState(false)

  function toggle(type: string) {
    if (filter.includes(type)) {
      onChange(filter.filter((t) => t !== type))
    } else {
      onChange([...filter, type])
    }
  }

  function clearAll() {
    onChange([])
  }

  return (
    <div className="relative">
      <button
        onClick={() => setOpen((v) => !v)}
        className={cn(
          'flex items-center gap-1.5 px-2 py-1 rounded text-xs border',
          filter.length > 0
            ? 'border-purple-700 text-purple-300 bg-purple-950'
            : 'border-zinc-700 text-zinc-400 bg-zinc-900',
          'hover:border-zinc-500 transition-colors'
        )}
      >
        <span>Filter</span>
        {filter.length > 0 && (
          <span className="bg-purple-700 text-white rounded-full px-1.5 leading-4 text-[10px]">
            {filter.length}
          </span>
        )}
        <svg className="w-3 h-3" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.5">
          <path d="M2 4l4 4 4-4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>

      {open && (
        <>
          {/* backdrop */}
          <div className="fixed inset-0 z-10" onClick={() => setOpen(false)} />
          <div className="absolute right-0 top-full mt-1 z-20 w-52 bg-zinc-900 border border-zinc-700 rounded shadow-xl py-1 max-h-72 overflow-y-auto">
            {filter.length > 0 && (
              <button
                onClick={clearAll}
                className="w-full text-left px-3 py-1.5 text-xs text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800"
              >
                Clear all
              </button>
            )}
            {ALL_EVENT_TYPES.map((type) => (
              <label
                key={type}
                className="flex items-center gap-2 px-3 py-1.5 text-xs cursor-pointer hover:bg-zinc-800"
              >
                <input
                  type="checkbox"
                  checked={filter.includes(type)}
                  onChange={() => toggle(type)}
                  className="accent-purple-500"
                />
                <span className="font-mono text-zinc-300">{type}</span>
              </label>
            ))}
          </div>
        </>
      )}
    </div>
  )
}

// ─── Event row ────────────────────────────────────────────────────────────────

function EventRow({
  event,
  baseIso,
  onNodeClick,
}: {
  event: Event
  baseIso: string
  onNodeClick: (nodeId: string) => void
}) {
  const [expanded, setExpanded] = useState(false)
  const payloadId = `event-payload-${event.id}`
  const toggleExpanded = () => setExpanded((v) => !v)
  const type = event.kind.type
  const nodeId = extractNodeId(event.kind)
  const isCoordinator = isCoordinatorEvent(type)

  return (
    <div
      className={cn(
        'border-b border-zinc-800/60 last:border-0',
        isCoordinator && 'border-l-2 border-l-purple-700'
      )}
    >
      {/* Summary row */}
      <div
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        aria-controls={payloadId}
        className="flex items-center gap-3 px-3 py-1.5 cursor-pointer hover:bg-zinc-900/60 transition-colors select-none"
        onClick={toggleExpanded}
        onKeyDown={(e) => {
          if (e.target !== e.currentTarget) return
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault()
            toggleExpanded()
          }
        }}
      >
        {/* Sequence */}
        <span className="font-mono text-[11px] text-zinc-600 w-8 shrink-0 text-right">
          {event.sequence}
        </span>

        {/* Timestamp */}
        <span className="font-mono text-[11px] text-zinc-500 w-14 shrink-0">
          {relativeTimestamp(baseIso, event.created_at)}
        </span>

        {/* Event type badge */}
        <span
          className={cn(
            'font-mono text-[10px] px-1.5 py-0.5 rounded shrink-0 whitespace-nowrap',
            badgeClass(type)
          )}
        >
          {type}
        </span>

        {/* Node ID (clickable) */}
        {nodeId && (
          <button
            className="font-mono text-[11px] text-zinc-400 hover:text-zinc-100 underline underline-offset-2 decoration-dotted shrink-0 truncate max-w-[10rem]"
            onClick={(e) => {
              e.stopPropagation()
              onNodeClick(nodeId)
            }}
            title={nodeId}
          >
            {nodeId}
          </button>
        )}

        {/* Summary */}
        <span className="text-xs text-zinc-400 truncate flex-1 min-w-0">
          {extractSummary(event.kind)}
        </span>

        {/* Expand chevron */}
        <svg
          className={cn('w-3 h-3 text-zinc-600 shrink-0 transition-transform', expanded && 'rotate-180')}
          viewBox="0 0 12 12"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
        >
          <path d="M2 4l4 4 4-4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </div>

      {/* Expanded JSON payload */}
      {expanded && (
        <div id={payloadId} className="px-3 pb-2 pt-1">
          <pre className="text-[11px] font-mono bg-zinc-950 border border-zinc-800 p-2 rounded overflow-auto max-h-48 text-zinc-300">
            {JSON.stringify(event.kind, null, 2)}
          </pre>
        </div>
      )}
    </div>
  )
}

// ─── EventTimeline ────────────────────────────────────────────────────────────

export function EventTimeline() {
  const selectedExecutionId = useInspectorStore((s) => s.selectedExecutionId)
  const eventTypeFilter = useInspectorStore((s) => s.eventTypeFilter)
  const setEventTypeFilter = useInspectorStore((s) => s.setEventTypeFilter)
  const setNode = useInspectorStore((s) => s.setNode)

  const { data: events = [], isLoading, isError, refetch } = useEvents(selectedExecutionId)

  const filtered =
    eventTypeFilter.length === 0
      ? events
      : events.filter((e) => eventTypeFilter.includes(e.kind.type))

  // Use the earliest event timestamp as the relative base
  const baseIso = events.length > 0 ? events[0].created_at : new Date().toISOString()

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-1.5 border-b border-zinc-800 shrink-0">
        <div className="flex items-center gap-2">
          <span className="text-xs font-semibold text-zinc-300 uppercase tracking-wider">Events</span>
          {events.length > 0 && (
            <span className="text-[10px] text-zinc-600">
              {filtered.length}{filtered.length !== events.length && `/${events.length}`}
            </span>
          )}
        </div>
        <FilterDropdown filter={eventTypeFilter} onChange={setEventTypeFilter} />
      </div>

      {/* Body */}
      <div className="flex-1 overflow-y-auto">
        {!selectedExecutionId && (
          <div className="flex items-center justify-center h-full">
            <p className="text-zinc-600 text-xs">Select an execution to view events</p>
          </div>
        )}

        {selectedExecutionId && isLoading && (
          <div className="flex items-center justify-center h-full">
            <p className="text-zinc-600 text-xs">Loading events…</p>
          </div>
        )}

        {selectedExecutionId && isError && (
          <div className="flex flex-col items-center justify-center h-full gap-2">
            <p className="text-red-400 text-xs">Failed to load events</p>
            <button
              className="text-xs px-2 py-1 rounded border border-zinc-700 hover:border-zinc-500 text-zinc-400"
              onClick={() => void refetch()}
            >
              Retry
            </button>
          </div>
        )}

        {selectedExecutionId && !isLoading && !isError && filtered.length === 0 && (
          <div className="flex items-center justify-center h-full">
            <p className="text-zinc-600 text-xs">
              {eventTypeFilter.length > 0 ? 'No events match the current filter' : 'No events'}
            </p>
          </div>
        )}

        {filtered.map((event) => (
          <EventRow
            key={event.id}
            event={event}
            baseIso={baseIso}
            onNodeClick={setNode}
          />
        ))}
      </div>
    </div>
  )
}

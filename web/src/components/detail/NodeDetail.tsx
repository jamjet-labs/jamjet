import { Fragment } from 'react'
import { useInspectorStore } from '@/store/inspector'
import { useEvents } from '@/hooks/useExecution'
import type { Event, NodeStartedEvent, NodeCompletedEvent, NodeFailedEvent, Provenance } from '@/api/types'
import { CoordinatorDetail } from './CoordinatorDetail'
import { AgentToolDetail } from './AgentToolDetail'

// ─── Helpers ──────────────────────────────────────────────────────────────────

function JsonBlock({ value }: { value: unknown }) {
  return (
    <pre className="text-xs font-mono bg-zinc-950 p-2 rounded overflow-auto max-h-48">
      {JSON.stringify(value, null, 2)}
    </pre>
  )
}

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
      {children}
    </summary>
  )
}

// ─── Timing ───────────────────────────────────────────────────────────────────

function TimingSection({
  startedAt,
  completedAt,
  durationMs,
  costUsd,
}: {
  startedAt?: string
  completedAt?: string
  durationMs?: number
  costUsd?: number
}) {
  if (!startedAt && durationMs === undefined && costUsd === undefined) return null

  const computedDuration =
    durationMs !== undefined
      ? durationMs
      : startedAt && completedAt
      ? new Date(completedAt).getTime() - new Date(startedAt).getTime()
      : undefined

  return (
    <div className="space-y-1">
      <div className="text-xs font-medium text-zinc-400 uppercase tracking-wider">Timing</div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
        {computedDuration !== undefined && (
          <>
            <span className="text-zinc-500">Duration</span>
            <span className="font-mono text-zinc-300">{computedDuration} ms</span>
          </>
        )}
        {costUsd !== undefined && (
          <>
            <span className="text-zinc-500">Cost</span>
            <span className="font-mono text-zinc-300">${costUsd.toFixed(6)}</span>
          </>
        )}
        {startedAt && (
          <>
            <span className="text-zinc-500">Started</span>
            <span className="font-mono text-zinc-300">
              {new Date(startedAt).toISOString().slice(11, 23)}
            </span>
          </>
        )}
        {completedAt && (
          <>
            <span className="text-zinc-500">Completed</span>
            <span className="font-mono text-zinc-300">
              {new Date(completedAt).toISOString().slice(11, 23)}
            </span>
          </>
        )}
      </div>
    </div>
  )
}

// ─── Provenance ───────────────────────────────────────────────────────────────

function ProvenanceSection({ provenance }: { provenance: Provenance }) {
  const entries = Object.entries(provenance).filter(([, v]) => v !== undefined && v !== null)
  if (entries.length === 0) return null

  return (
    <div className="space-y-1">
      <div className="text-xs font-medium text-zinc-400 uppercase tracking-wider">Provenance</div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
        {entries.map(([key, val]) => (
          <Fragment key={key}>
            <span className="text-zinc-500 capitalize">{key.replace(/_/g, ' ')}</span>
            <span className="font-mono text-zinc-300 break-all">
              {typeof val === 'number' ? val.toFixed(4) : String(val)}
            </span>
          </Fragment>
        ))}
      </div>
    </div>
  )
}

// ─── NodeDetail ───────────────────────────────────────────────────────────────

function extractProvenance(event: Event): Provenance | undefined {
  const k = event.kind as Record<string, unknown>
  const p = k['provenance'] ?? k['prov']
  if (p && typeof p === 'object') return p as Provenance
  // Also check top-level fields
  const fields: Provenance = {}
  if (typeof k['model_id'] === 'string') fields.model_id = k['model_id']
  if (typeof k['confidence'] === 'number') fields.confidence = k['confidence']
  if (typeof k['source'] === 'string') fields.source = k['source']
  if (typeof k['trust_domain'] === 'string') fields.trust_domain = k['trust_domain']
  if (Object.keys(fields).length > 0) return fields
  return undefined
}

function extractDuration(event: Event): number | undefined {
  const k = event.kind as Record<string, unknown>
  if (typeof k['duration_ms'] === 'number') return k['duration_ms']
  return undefined
}

function extractCost(event: Event): number | undefined {
  const k = event.kind as Record<string, unknown>
  if (typeof k['cost_usd'] === 'number') return k['cost_usd']
  return undefined
}

function extractInputs(event: Event): unknown {
  const k = event.kind as Record<string, unknown>
  return k['payload'] ?? k['input'] ?? k['inputs'] ?? null
}

function extractOutput(event: Event): unknown {
  const k = event.kind as Record<string, unknown>
  return k['output'] ?? k['result'] ?? k['payload'] ?? null
}

function extractNodeType(events: Event[], nodeId: string): string {
  // Try to derive from event patterns
  const nodeEventTypes = new Set(
    events
      .filter((e) => 'node_id' in e.kind && (e.kind as { node_id?: string }).node_id === nodeId)
      .map((e) => e.kind.type)
  )
  if (nodeEventTypes.has('coordinator_decision') || nodeEventTypes.has('coordinator_scoring')) {
    return 'coordinator'
  }
  if (
    nodeEventTypes.has('agent_tool_invoked') ||
    nodeEventTypes.has('agent_tool_completed') ||
    nodeEventTypes.has('agent_turn')
  ) {
    return 'agent_tool'
  }
  return 'node'
}

export function NodeDetail() {
  const selectedNodeId = useInspectorStore((s) => s.selectedNodeId)
  const selectedExecutionId = useInspectorStore((s) => s.selectedExecutionId)

  const { data: events = [] } = useEvents(selectedExecutionId)

  if (!selectedNodeId) {
    return (
      <div className="flex items-center justify-center h-full">
        <p className="text-zinc-600 text-sm">Select a node to inspect</p>
      </div>
    )
  }

  const nodeEvents = events.filter(
    (e) => 'node_id' in e.kind && (e.kind as { node_id?: string }).node_id === selectedNodeId
  )

  const startedEvent = nodeEvents.find((e) => e.kind.type === 'node_started')
  const completedEvent = nodeEvents.find((e) => e.kind.type === 'node_completed')
  const failedEvent = nodeEvents.find((e) => e.kind.type === 'node_failed')

  const inputs = startedEvent ? extractInputs(startedEvent) : null
  const output = completedEvent ? extractOutput(completedEvent) : null

  const provenance =
    completedEvent
      ? extractProvenance(completedEvent)
      : startedEvent
      ? extractProvenance(startedEvent)
      : undefined

  const durationMs =
    completedEvent
      ? extractDuration(completedEvent)
      : failedEvent
      ? extractDuration(failedEvent)
      : undefined

  const costUsd =
    completedEvent ? extractCost(completedEvent) : undefined

  const nodeType = extractNodeType(events, selectedNodeId)

  return (
    <div className="p-4 space-y-4">
      {/* ── Header ─────────────────────────────────────────────────────── */}
      <div className="border-b border-zinc-800 pb-3">
        <div className="font-mono text-sm text-zinc-100 break-all">{selectedNodeId}</div>
        <div className="text-xs text-zinc-500 mt-0.5 capitalize">{nodeType.replace(/_/g, ' ')}</div>
        {failedEvent && (
          <div className="mt-2 text-xs text-red-400 bg-red-950 rounded px-2 py-1">
            {(failedEvent.kind as NodeFailedEvent).error}
          </div>
        )}
      </div>

      {/* ── Inputs ─────────────────────────────────────────────────────── */}
      {inputs !== null && inputs !== undefined && (
        <details open>
          <SectionLabel>Inputs</SectionLabel>
          <div className="mt-1">
            <JsonBlock value={inputs} />
          </div>
        </details>
      )}

      {/* ── Output ─────────────────────────────────────────────────────── */}
      {output !== null && output !== undefined && (
        <details open>
          <SectionLabel>Output</SectionLabel>
          <div className="mt-1">
            <JsonBlock value={output} />
          </div>
        </details>
      )}

      {/* ── Timing ─────────────────────────────────────────────────────── */}
      <TimingSection
        startedAt={startedEvent?.created_at}
        completedAt={completedEvent?.created_at ?? failedEvent?.created_at}
        durationMs={durationMs}
        costUsd={costUsd}
      />

      {/* ── Provenance ─────────────────────────────────────────────────── */}
      {provenance && <ProvenanceSection provenance={provenance} />}

      {/* ── Type-specific detail ───────────────────────────────────────── */}
      {nodeType === 'coordinator' && (
        <div className="border-t border-zinc-800 pt-3">
          <CoordinatorDetail nodeId={selectedNodeId} events={events} />
        </div>
      )}
      {nodeType === 'agent_tool' && (
        <div className="border-t border-zinc-800 pt-3">
          <AgentToolDetail nodeId={selectedNodeId} events={events} />
        </div>
      )}
    </div>
  )
}

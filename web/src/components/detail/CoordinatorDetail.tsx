import type { Event, CoordinatorDiscoveryEvent, CoordinatorScoringEvent, CoordinatorDecisionEvent, ScoringEntry } from '@/api/types'

interface CoordinatorDetailProps {
  nodeId: string
  events: Event[]
}

function Badge({ label, className }: { label: string; className: string }) {
  return (
    <span className={`text-xs font-medium px-2 py-0.5 rounded-full ${className}`}>{label}</span>
  )
}

const methodBadgeClass: Record<string, string> = {
  structured: 'bg-emerald-900 text-emerald-300',
  llm_tiebreaker: 'bg-blue-900 text-blue-300',
  single_candidate: 'bg-zinc-700 text-zinc-400',
}

function DiscoverySection({ event }: { event: CoordinatorDiscoveryEvent }) {
  return (
    <details open>
      <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
        Discovery ({event.candidates.length} candidate{event.candidates.length !== 1 ? 's' : ''})
      </summary>
      <div className="mt-2 space-y-1">
        {event.candidates.length === 0 && (
          <p className="text-zinc-600 text-xs italic">No candidates discovered</p>
        )}
        {event.candidates.map((uri) => (
          <div key={uri} className="flex items-center gap-2 py-1 border-b border-zinc-800 last:border-0">
            <span className="w-2 h-2 rounded-full bg-emerald-500 shrink-0" />
            <span className="font-mono text-xs text-zinc-300 break-all">{uri}</span>
          </div>
        ))}
      </div>
    </details>
  )
}

function ScoringSection({ scores }: { scores: ScoringEntry[] }) {
  const sorted = [...scores].sort((a, b) => b.composite - a.composite)
  const composites = scores.map((s) => s.composite)
  const spread =
    composites.length >= 2
      ? (Math.max(...composites) - Math.min(...composites)).toFixed(3)
      : null

  // Collect all dimension keys across all entries
  const dimKeys = Array.from(
    new Set(sorted.flatMap((s) => Object.keys(s.scores)))
  )

  return (
    <details open>
      <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
        Scoring ({scores.length} agent{scores.length !== 1 ? 's' : ''})
        {spread !== null && (
          <span className="ml-2 text-zinc-600 font-normal">spread {spread}</span>
        )}
      </summary>
      <div className="mt-2 overflow-x-auto">
        {scores.length === 0 ? (
          <p className="text-zinc-600 text-xs italic">No scoring data</p>
        ) : (
          <table className="w-full text-xs font-mono border-collapse">
            <thead>
              <tr className="text-zinc-500 border-b border-zinc-800">
                <th className="text-left py-1 pr-2 font-medium">Agent URI</th>
                {dimKeys.map((k) => (
                  <th key={k} className="text-right py-1 px-1 font-medium capitalize">
                    {k}
                  </th>
                ))}
                <th className="text-right py-1 pl-2 font-medium text-zinc-300">Composite</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((entry, i) => (
                <tr
                  key={entry.uri}
                  className={`border-b border-zinc-800 last:border-0 ${
                    i === 0 ? 'text-zinc-100' : 'text-zinc-400'
                  }`}
                >
                  <td className="py-1 pr-2 max-w-[8rem] truncate" title={entry.uri}>
                    {entry.uri}
                  </td>
                  {dimKeys.map((k) => (
                    <td key={k} className="text-right py-1 px-1">
                      {entry.scores[k] !== undefined ? entry.scores[k].toFixed(3) : '—'}
                    </td>
                  ))}
                  <td
                    className={`text-right py-1 pl-2 font-semibold ${
                      i === 0 ? 'text-emerald-400' : ''
                    }`}
                  >
                    {entry.composite.toFixed(3)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </details>
  )
}

function DecisionSection({
  event,
  scores,
}: {
  event: CoordinatorDecisionEvent
  scores: ScoringEntry[]
}) {
  const rejected = scores.filter((s) => s.uri !== event.selected).map((s) => s.uri)

  // Infer method from scores count
  let method: string
  if (scores.length === 1) {
    method = 'single_candidate'
  } else if (event.reasoning) {
    method = 'llm_tiebreaker'
  } else {
    method = 'structured'
  }

  return (
    <details open>
      <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
        Decision
      </summary>
      <div className="mt-2 space-y-3">
        {/* Selected agent */}
        <div className="flex items-start gap-2">
          <span className="w-2 h-2 mt-1 rounded-full bg-emerald-400 shrink-0" />
          <div>
            <div className="text-[10px] text-zinc-500 mb-0.5">Selected</div>
            <div className="font-mono text-xs text-zinc-100">{event.selected}</div>
          </div>
        </div>

        {/* Method badge */}
        <div className="flex items-center gap-2">
          <span className="text-xs text-zinc-500">Method</span>
          <Badge
            label={method.replace(/_/g, ' ')}
            className={methodBadgeClass[method] ?? methodBadgeClass.structured}
          />
        </div>

        {/* Reasoning */}
        {event.reasoning && (
          <div>
            <div className="text-[10px] text-zinc-500 mb-1">Reasoning</div>
            <p className="text-xs text-zinc-300 leading-relaxed">{event.reasoning}</p>
          </div>
        )}

        {/* Rejected agents */}
        {rejected.length > 0 && (
          <div>
            <div className="text-[10px] text-zinc-500 mb-1">
              Rejected ({rejected.length})
            </div>
            <div className="space-y-1">
              {rejected.map((uri) => (
                <div key={uri} className="flex items-center gap-2">
                  <span className="w-2 h-2 rounded-full bg-red-800 shrink-0" />
                  <span className="font-mono text-xs text-zinc-500 break-all">{uri}</span>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </details>
  )
}

export function CoordinatorDetail({ nodeId, events }: CoordinatorDetailProps) {
  const nodeEvents = events.filter(
    (e) => 'node_id' in e.kind && (e.kind as { node_id?: string }).node_id === nodeId
  )

  const discoveryEvent = nodeEvents.find(
    (e) => e.kind.type === 'coordinator_discovery'
  )?.kind as CoordinatorDiscoveryEvent | undefined

  const scoringEvent = nodeEvents.find(
    (e) => e.kind.type === 'coordinator_scoring'
  )?.kind as CoordinatorScoringEvent | undefined

  const decisionEvent = nodeEvents.find(
    (e) => e.kind.type === 'coordinator_decision'
  )?.kind as CoordinatorDecisionEvent | undefined

  return (
    <div className="space-y-4">
      {!discoveryEvent && !scoringEvent && !decisionEvent && (
        <p className="text-zinc-600 text-xs italic">No coordinator events for this node</p>
      )}

      {discoveryEvent && <DiscoverySection event={discoveryEvent} />}

      {scoringEvent && <ScoringSection scores={scoringEvent.scores} />}

      {decisionEvent && (
        <DecisionSection event={decisionEvent} scores={scoringEvent?.scores ?? []} />
      )}
    </div>
  )
}

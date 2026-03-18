import type { Event, AgentToolInvokedEvent, AgentToolCompletedEvent, AgentToolProgressEvent, AgentTurnEvent } from '@/api/types'

interface AgentToolDetailProps {
  nodeId: string
  events: Event[]
}

type Mode = 'sync' | 'streaming' | 'conversational'

function detectMode(nodeEvents: Event[]): Mode {
  const hasProgress = nodeEvents.some((e) => e.kind.type === 'agent_tool_progress')
  const hasTurn = nodeEvents.some((e) => e.kind.type === 'agent_turn')
  if (hasTurn) return 'conversational'
  if (hasProgress) return 'streaming'
  return 'sync'
}

function detectProtocol(nodeEvents: Event[]): string {
  const invoked = nodeEvents.find((e) => e.kind.type === 'agent_tool_invoked')
  if (!invoked) return 'local'
  const tool = (invoked.kind as AgentToolInvokedEvent).tool ?? ''
  if (tool.startsWith('a2a://')) return 'a2a'
  if (tool.startsWith('mcp://')) return 'mcp'
  return 'local'
}

const modeBadgeClass: Record<Mode, string> = {
  sync: 'bg-blue-900 text-blue-300',
  streaming: 'bg-amber-900 text-amber-300',
  conversational: 'bg-violet-900 text-violet-300',
}

const protocolBadgeClass: Record<string, string> = {
  a2a: 'bg-emerald-900 text-emerald-300',
  mcp: 'bg-sky-900 text-sky-300',
  local: 'bg-zinc-700 text-zinc-300',
}

function Badge({ label, className }: { label: string; className: string }) {
  return (
    <span className={`text-xs font-medium px-2 py-0.5 rounded-full ${className}`}>{label}</span>
  )
}

function JsonBlock({ value }: { value: unknown }) {
  return (
    <pre className="text-xs font-mono bg-zinc-950 p-2 rounded overflow-auto max-h-48">
      {JSON.stringify(value, null, 2)}
    </pre>
  )
}

function SyncView({ nodeEvents }: { nodeEvents: Event[] }) {
  const invoked = nodeEvents.find((e) => e.kind.type === 'agent_tool_invoked')
  const completed = nodeEvents.find((e) => e.kind.type === 'agent_tool_completed')

  return (
    <div className="space-y-3">
      {invoked && (
        <details open>
          <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
            Request
          </summary>
          <div className="mt-1">
            <JsonBlock value={(invoked.kind as AgentToolInvokedEvent).input} />
          </div>
        </details>
      )}
      {completed && (
        <details open>
          <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
            Response
          </summary>
          <div className="mt-1">
            <JsonBlock value={(completed.kind as AgentToolCompletedEvent).output} />
          </div>
        </details>
      )}
    </div>
  )
}

function StreamingView({ nodeEvents }: { nodeEvents: Event[] }) {
  const progressEvents = nodeEvents.filter((e) => e.kind.type === 'agent_tool_progress')
  const completed = nodeEvents.find((e) => e.kind.type === 'agent_tool_completed')

  return (
    <div className="space-y-3">
      <details open>
        <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
          Progress chunks ({progressEvents.length})
        </summary>
        <div className="mt-1 space-y-1">
          {progressEvents.length === 0 && (
            <p className="text-zinc-600 text-xs italic">No progress events</p>
          )}
          {progressEvents.map((e) => (
            <div key={e.id} className="flex gap-2 items-start">
              <span className="text-zinc-600 font-mono text-xs shrink-0">
                {new Date(e.created_at).toISOString().slice(11, 23)}
              </span>
              <JsonBlock value={(e.kind as AgentToolProgressEvent).progress} />
            </div>
          ))}
        </div>
      </details>
      {completed && (
        <details open>
          <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
            Final output
          </summary>
          <div className="mt-1">
            <JsonBlock value={(completed.kind as AgentToolCompletedEvent).output} />
          </div>
        </details>
      )}
    </div>
  )
}

function ConversationalView({ nodeEvents }: { nodeEvents: Event[] }) {
  const turns = nodeEvents
    .filter((e) => e.kind.type === 'agent_turn')
    .sort((a, b) => a.sequence - b.sequence)

  return (
    <details open>
      <summary className="cursor-pointer text-xs font-medium text-zinc-400 uppercase tracking-wider py-1 select-none">
        Turns ({turns.length})
      </summary>
      <div className="mt-2 space-y-2">
        {turns.length === 0 && (
          <p className="text-zinc-600 text-xs italic">No turn events</p>
        )}
        {turns.map((e) => {
          const turn = e.kind as AgentTurnEvent
          const isOutbound = turn.turn % 2 === 1
          return (
            <div
              key={e.id}
              className={`flex ${isOutbound ? 'justify-end' : 'justify-start'}`}
            >
              <div
                className={`max-w-[80%] rounded px-3 py-2 text-xs ${
                  isOutbound
                    ? 'bg-blue-900 text-blue-100'
                    : 'bg-zinc-800 text-zinc-200'
                }`}
              >
                <div className="text-zinc-500 text-[10px] mb-1">
                  {isOutbound ? 'outbound' : 'inbound'} · turn {turn.turn}
                </div>
                <div className="whitespace-pre-wrap">{turn.content}</div>
              </div>
            </div>
          )
        })}
      </div>
    </details>
  )
}

export function AgentToolDetail({ nodeId, events }: AgentToolDetailProps) {
  const nodeEvents = events.filter(
    (e) => 'node_id' in e.kind ? (e.kind as { node_id?: string }).node_id === nodeId : false
  )

  // agent_tool events are keyed by `tool`, not node_id — include all tool events
  // that belong to this node by proximity (events without node_id scoping)
  const allToolEvents = events.filter((e) =>
    [
      'agent_tool_invoked',
      'agent_tool_completed',
      'agent_tool_progress',
      'agent_turn',
      'agent_terminated',
      'agent_failed',
    ].includes(e.kind.type) || nodeEvents.some((ne) => ne.id === e.id)
  )

  const relevantEvents = nodeEvents.length > 0 ? nodeEvents : allToolEvents

  const mode = detectMode(relevantEvents)
  const protocol = detectProtocol(relevantEvents)

  const invoked = relevantEvents.find((e) => e.kind.type === 'agent_tool_invoked')
  const agentUri = invoked ? (invoked.kind as AgentToolInvokedEvent).tool : null

  return (
    <div className="space-y-4">
      {/* Badges */}
      <div className="flex gap-2 flex-wrap">
        <Badge label={mode} className={modeBadgeClass[mode]} />
        <Badge label={protocol} className={protocolBadgeClass[protocol] ?? protocolBadgeClass.local} />
      </div>

      {/* Agent URI */}
      {agentUri && (
        <div>
          <div className="text-xs text-zinc-500 uppercase tracking-wider mb-1">Agent URI</div>
          <div className="font-mono text-xs text-zinc-300 break-all">{agentUri}</div>
        </div>
      )}

      {/* Mode-specific content */}
      {mode === 'sync' && <SyncView nodeEvents={relevantEvents} />}
      {mode === 'streaming' && <StreamingView nodeEvents={relevantEvents} />}
      {mode === 'conversational' && <ConversationalView nodeEvents={relevantEvents} />}
    </div>
  )
}

import type { Execution } from '@/api/types'
import { useExecutions } from '@/hooks/useExecution'
import { useInspectorStore } from '@/store/inspector'

// ─── Status config ────────────────────────────────────────────────────────────

const STATUS_DOT: Record<Execution['status'], string> = {
  completed: 'bg-emerald-500',
  running: 'bg-blue-500',
  failed: 'bg-red-500',
  pending: 'bg-zinc-500',
  cancelled: 'bg-yellow-500',
}

const STATUS_LABEL: Record<Execution['status'], string> = {
  completed: 'Completed',
  running: 'Running',
  failed: 'Failed',
  pending: 'Pending',
  cancelled: 'Cancelled',
}

// ─── Component ────────────────────────────────────────────────────────────────

export function ExecutionList() {
  const { data, isLoading } = useExecutions()
  const executions: Execution[] = data?.executions ?? []
  const { selectedExecutionId, setExecution } = useInspectorStore()

  return (
    <div className="flex flex-col h-full overflow-hidden">
      <div className="h-9 flex items-center px-3 border-b border-zinc-800 shrink-0">
        <span className="text-xs font-medium text-zinc-400 uppercase tracking-wider">
          Executions
        </span>
      </div>

      <div className="flex-1 overflow-y-auto">
        {isLoading && (
          <div className="px-3 py-4 text-xs text-zinc-500">Loading…</div>
        )}

        {!isLoading && executions.length === 0 && (
          <div className="px-3 py-4 text-xs text-zinc-500">No executions</div>
        )}

        {executions.map((ex) => {
          const isSelected = ex.id === selectedExecutionId
          return (
            <button
              key={ex.id}
              onClick={() => setExecution(ex.id)}
              className={[
                'w-full text-left px-3 py-2 flex flex-col gap-0.5 hover:bg-zinc-800/60 transition-colors',
                isSelected ? 'bg-zinc-800' : '',
              ]
                .filter(Boolean)
                .join(' ')}
            >
              {/* Truncated ID in monospace */}
              <span className="font-mono text-xs text-zinc-200 truncate">
                {ex.id.length > 18 ? ex.id.slice(0, 8) + '…' + ex.id.slice(-6) : ex.id}
              </span>

              {/* Status row */}
              <span className="flex items-center gap-1.5">
                <span
                  className={`w-1.5 h-1.5 rounded-full shrink-0 ${STATUS_DOT[ex.status]}`}
                />
                <span className="text-xs text-zinc-400">{STATUS_LABEL[ex.status]}</span>
              </span>
            </button>
          )
        })}
      </div>
    </div>
  )
}

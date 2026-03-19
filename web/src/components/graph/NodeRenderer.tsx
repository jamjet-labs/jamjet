import React from 'react'
import { Handle, Position } from '@xyflow/react'
import { cn } from '@/lib/utils'

// ─── Types ────────────────────────────────────────────────────────────────────

export interface NodeData {
  label: string
  nodeType: string
  status: string
  selected: boolean
  [key: string]: unknown
}

interface NodeRendererProps {
  data: NodeData
  selected: boolean
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

function nodeIcon(nodeType: string): string {
  switch (nodeType) {
    case 'model':
      return '🤖'
    case 'tool':
      return '🔧'
    case 'coordinator':
      return '🎯'
    case 'agent_tool':
      return '🔗'
    case 'condition':
      return '◆'
    case 'eval':
      return '📊'
    case 'human_approval':
      return '✋'
    case 'start':
      return '▶'
    case 'end':
      return '⏹'
    default:
      return '⬡'
  }
}

function statusClasses(status: string): string {
  switch (status) {
    case 'completed':
      return 'bg-emerald-900/60 border-emerald-600 text-emerald-100'
    case 'running':
      return 'bg-blue-900/60 border-blue-500 text-blue-100 animate-pulse'
    case 'failed':
      return 'bg-red-900/60 border-red-600 text-red-100'
    case 'scheduled':
      return 'bg-zinc-700/60 border-zinc-500 text-zinc-200'
    case 'skipped':
      return 'bg-zinc-900/40 border-zinc-700 text-zinc-500'
    case 'pending':
    default:
      return 'bg-zinc-800/60 border-zinc-600 text-zinc-300'
  }
}

// ─── Component ────────────────────────────────────────────────────────────────

function NodeRenderer({ data, selected }: NodeRendererProps) {
  const { label, nodeType, status } = data

  return (
    <div
      className={cn(
        'rounded-lg border px-3 py-2 min-w-[120px] max-w-[160px] text-center text-xs font-medium shadow-md transition-all',
        statusClasses(status),
        selected && 'ring-2 ring-blue-400 ring-offset-1 ring-offset-zinc-950',
      )}
    >
      {/* Target handle — top */}
      <Handle
        type="target"
        position={Position.Top}
        className="!bg-zinc-500 !border-zinc-400 !w-2 !h-2"
      />

      {/* Icon + label */}
      <div className="flex flex-col items-center gap-1">
        <span className="text-base leading-none">{nodeIcon(nodeType)}</span>
        <span className="leading-tight break-words">{label}</span>
        {status !== 'pending' && (
          <span className="text-[10px] opacity-70 capitalize">{status}</span>
        )}
      </div>

      {/* Source handle — bottom */}
      <Handle
        type="source"
        position={Position.Bottom}
        className="!bg-zinc-500 !border-zinc-400 !w-2 !h-2"
      />
    </div>
  )
}

export default React.memo(NodeRenderer)

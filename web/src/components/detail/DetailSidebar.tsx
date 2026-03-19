import { useState } from 'react'
import { cn } from '@/lib/utils'
import { NodeDetail } from './NodeDetail'
import { StateInspector } from '@/components/state/StateInspector'

type Tab = 'detail' | 'state'

export function DetailSidebar() {
  const [activeTab, setActiveTab] = useState<Tab>('detail')

  return (
    <div className="flex flex-col h-full">
      {/* ── Tab toggle ──────────────────────────────────────────────── */}
      <div className="flex shrink-0 border-b border-zinc-800">
        <button
          onClick={() => setActiveTab('detail')}
          className={cn(
            'flex-1 py-2 text-xs font-medium tracking-wide transition-colors',
            activeTab === 'detail'
              ? 'text-zinc-100 border-b-2 border-zinc-400 -mb-px'
              : 'text-zinc-500 hover:text-zinc-300'
          )}
        >
          Detail
        </button>
        <button
          onClick={() => setActiveTab('state')}
          className={cn(
            'flex-1 py-2 text-xs font-medium tracking-wide transition-colors',
            activeTab === 'state'
              ? 'text-zinc-100 border-b-2 border-zinc-400 -mb-px'
              : 'text-zinc-500 hover:text-zinc-300'
          )}
        >
          State
        </button>
      </div>

      {/* ── Panel ───────────────────────────────────────────────────── */}
      <div className="flex-1 overflow-auto">
        {activeTab === 'detail' ? <NodeDetail /> : <StateInspector />}
      </div>
    </div>
  )
}

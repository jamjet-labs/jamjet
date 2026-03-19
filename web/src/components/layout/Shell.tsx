import type { ReactNode } from 'react'
import { ExecutionList } from './ExecutionList'

interface ShellProps {
  graph: ReactNode
  detail: ReactNode
  timeline: ReactNode
  /** Optional toolbar rendered between the header and the main content area. */
  toolbar?: ReactNode
}

export function Shell({ graph, detail, timeline, toolbar }: ShellProps) {
  return (
    <div className="h-screen flex flex-col bg-zinc-950 text-zinc-100 overflow-hidden">
      {/* ── Header bar ─────────────────────────────────────────────────────── */}
      <header className="h-12 shrink-0 border-b border-zinc-800 flex items-center px-4">
        <span className="font-semibold text-sm tracking-wide">JamJet Inspector</span>
      </header>

      {/* ── Toolbar slot (optional) ──────────────────────────────────────────── */}
      {toolbar && <div className="shrink-0">{toolbar}</div>}

      {/* ── Body (below header) ─────────────────────────────────────────────── */}
      <div className="flex flex-1 overflow-hidden">
        {/* Left sidebar — execution list */}
        <aside className="w-56 shrink-0 border-r border-zinc-800 flex flex-col overflow-hidden">
          <ExecutionList />
        </aside>

        {/* Main content area */}
        <div className="flex flex-col flex-1 overflow-hidden">
          {/* Top row: graph canvas + detail sidebar */}
          <div className="flex flex-1 overflow-hidden">
            {/* Graph canvas */}
            <div className="flex-1 overflow-hidden">{graph}</div>

            {/* Right detail sidebar */}
            <aside className="w-96 shrink-0 border-l border-zinc-800 overflow-y-auto">
              {detail}
            </aside>
          </div>

          {/* Bottom event timeline */}
          <div className="h-64 shrink-0 border-t border-zinc-800 overflow-y-auto">
            {timeline}
          </div>
        </div>
      </div>
    </div>
  )
}

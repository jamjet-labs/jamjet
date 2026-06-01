'use client'

import { useDemo } from './lib/useDemo'
import { SpendBar } from './components/SpendBar'
import { ModeBadge } from './components/ModeBadge'
import { ChatPanel } from './components/ChatPanel'
import { CostPanel } from './components/CostPanel'

export default function Page() {
  const {
    messages,
    events,
    totals,
    pendingApprovalId,
    mode,
    sendTurn,
    approve,
    runPreset,
  } = useDemo()

  return (
    <div className="app-shell">
      <header className="app-header">
        <div className="app-header-top">
          <h1 className="app-title">JamJet Cost Intelligence</h1>
          <ModeBadge mode={mode} />
        </div>
        <SpendBar events={events} spentCents={totals.spentCents} />
      </header>

      <main className="app-main">
        <section className="app-chat">
          <ChatPanel
            messages={messages}
            sendTurn={sendTurn}
            runPreset={runPreset}
          />
        </section>

        <aside className="app-cost">
          <CostPanel
            events={events}
            totals={totals}
            pendingApprovalId={pendingApprovalId}
            approve={approve}
          />
        </aside>
      </main>
    </div>
  )
}

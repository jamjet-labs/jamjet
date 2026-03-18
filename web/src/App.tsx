export default function App() {
  return (
    <div className="h-screen flex flex-col bg-zinc-950 text-zinc-100">
      <header className="h-12 border-b border-zinc-800 flex items-center px-4 shrink-0">
        <span className="font-semibold text-sm tracking-wide">JamJet Inspector</span>
      </header>
      <main className="flex-1 flex items-center justify-center text-zinc-500">
        Select an execution to inspect
      </main>
    </div>
  )
}

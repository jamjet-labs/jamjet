import { Shell } from '@/components/layout/Shell'

export default function App() {
  return (
    <Shell
      graph={<div className="flex items-center justify-center h-full text-zinc-600">Graph view</div>}
      detail={<div className="p-4 text-zinc-600 text-sm">Select a node</div>}
      timeline={<div className="p-4 text-zinc-600 text-sm">Event timeline</div>}
    />
  )
}

import { Shell } from '@/components/layout/Shell'
import { WorkflowGraph } from '@/components/graph/WorkflowGraph'

export default function App() {
  return (
    <Shell
      graph={<WorkflowGraph />}
      detail={<div className="p-4 text-zinc-600 text-sm">Select a node</div>}
      timeline={<div className="p-4 text-zinc-600 text-sm">Event timeline</div>}
    />
  )
}

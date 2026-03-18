import { Shell } from '@/components/layout/Shell'
import { WorkflowGraph } from '@/components/graph/WorkflowGraph'
import { NodeDetail } from '@/components/detail/NodeDetail'

export default function App() {
  return (
    <Shell
      graph={<WorkflowGraph />}
      detail={<NodeDetail />}
      timeline={<div className="p-4 text-zinc-600 text-sm">Event timeline</div>}
    />
  )
}

import { Shell } from '@/components/layout/Shell'
import { WorkflowGraph } from '@/components/graph/WorkflowGraph'
import { DetailSidebar } from '@/components/detail/DetailSidebar'

export default function App() {
  return (
    <Shell
      graph={<WorkflowGraph />}
      detail={<DetailSidebar />}
      timeline={<div className="p-4 text-zinc-600 text-sm">Event timeline</div>}
    />
  )
}

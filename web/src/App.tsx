import { Shell } from '@/components/layout/Shell'
import { WorkflowGraph } from '@/components/graph/WorkflowGraph'
import { DetailSidebar } from '@/components/detail/DetailSidebar'
import { EventTimeline } from '@/components/timeline/EventTimeline'
import { ReplayControls } from '@/components/replay/ReplayControls'

export default function App() {
  return (
    <Shell
      graph={<WorkflowGraph />}
      detail={<DetailSidebar />}
      timeline={<EventTimeline />}
      toolbar={<ReplayControls />}
    />
  )
}

import { Shell } from '@/components/layout/Shell'
import { WorkflowGraph } from '@/components/graph/WorkflowGraph'
import { DetailSidebar } from '@/components/detail/DetailSidebar'
import { EventTimeline } from '@/components/timeline/EventTimeline'
import { ReplayControls } from '@/components/replay/ReplayControls'
import { ErrorBoundary } from '@/components/layout/ErrorBoundary'

export default function App() {
  return (
    <Shell
      graph={
        <ErrorBoundary label="workflow graph">
          <WorkflowGraph />
        </ErrorBoundary>
      }
      detail={
        <ErrorBoundary label="detail panel">
          <DetailSidebar />
        </ErrorBoundary>
      }
      timeline={
        <ErrorBoundary label="event timeline">
          <EventTimeline />
        </ErrorBoundary>
      }
      toolbar={
        <ErrorBoundary label="replay toolbar">
          <ReplayControls />
        </ErrorBoundary>
      }
    />
  )
}

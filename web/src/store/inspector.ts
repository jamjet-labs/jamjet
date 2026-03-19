import { create } from 'zustand'

interface InspectorState {
  selectedExecutionId: string | null
  selectedNodeId: string | null
  timelinePosition: number | null
  eventTypeFilter: string[]

  setExecution: (id: string | null) => void
  setNode: (nodeId: string | null) => void
  setTimelinePosition: (position: number | null) => void
  setEventTypeFilter: (filter: string[]) => void
}

export const useInspectorStore = create<InspectorState>((set) => ({
  selectedExecutionId: null,
  selectedNodeId: null,
  timelinePosition: null,
  eventTypeFilter: [],

  setExecution: (id) =>
    set({
      selectedExecutionId: id,
      // Reset node and timeline context when switching executions
      selectedNodeId: null,
      timelinePosition: null,
    }),

  setNode: (nodeId) => set({ selectedNodeId: nodeId }),

  setTimelinePosition: (position) => set({ timelinePosition: position }),

  setEventTypeFilter: (filter) => set({ eventTypeFilter: filter }),
}))

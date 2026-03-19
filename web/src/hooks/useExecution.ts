import { useQuery } from '@tanstack/react-query'
import {
  fetchExecutions,
  fetchExecution,
  fetchEvents,
  fetchCoordinatorDecisions,
} from '../api/client'

// ─── Query key factory ────────────────────────────────────────────────────────

export const executionKeys = {
  all: ['executions'] as const,
  detail: (id: string) => ['executions', id] as const,
  events: (id: string) => ['executions', id, 'events'] as const,
  coordinatorDecisions: (id: string) => ['executions', id, 'coordinator-decisions'] as const,
}

// ─── Hooks ────────────────────────────────────────────────────────────────────

export function useExecutions() {
  return useQuery({
    queryKey: executionKeys.all,
    queryFn: fetchExecutions,
  })
}

export function useExecution(id: string | null) {
  return useQuery({
    queryKey: executionKeys.detail(id ?? ''),
    queryFn: () => fetchExecution(id!),
    enabled: id !== null,
  })
}

export function useEvents(executionId: string | null) {
  return useQuery({
    queryKey: executionKeys.events(executionId ?? ''),
    queryFn: () => fetchEvents(executionId!),
    enabled: executionId !== null,
  })
}

export function useCoordinatorDecisions(executionId: string | null) {
  return useQuery({
    queryKey: executionKeys.coordinatorDecisions(executionId ?? ''),
    queryFn: () => fetchCoordinatorDecisions(executionId!),
    enabled: executionId !== null,
  })
}

import type { Execution, Event, ScoringEntry } from './types'

// In dev, Vite proxies /api → the Rust runtime (see vite.config.ts).
// In production the SPA is served from the same origin as the runtime.
const BASE = '/api'

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`)
  if (!res.ok) {
    throw new Error(`API error ${res.status}: ${res.statusText} (${path})`)
  }
  return res.json() as Promise<T>
}

// ─── Executions ───────────────────────────────────────────────────────────────

export function fetchExecutions(): Promise<Execution[]> {
  return get<Execution[]>('/executions')
}

export function fetchExecution(id: string): Promise<Execution> {
  return get<Execution>(`/executions/${id}`)
}

// ─── Events ───────────────────────────────────────────────────────────────────

export function fetchEvents(executionId: string): Promise<Event[]> {
  return get<Event[]>(`/executions/${executionId}/events`)
}

// ─── Coordinator ──────────────────────────────────────────────────────────────

export function fetchCoordinatorDecisions(executionId: string): Promise<Event[]> {
  return get<Event[]>(`/executions/${executionId}/coordinator-decisions`)
}

// ─── Node scoring / reasoning ─────────────────────────────────────────────────

export function fetchNodeScoring(executionId: string, nodeId: string): Promise<ScoringEntry[]> {
  return get<ScoringEntry[]>(`/executions/${executionId}/nodes/${nodeId}/scoring`)
}

export function fetchNodeReasoning(executionId: string, nodeId: string): Promise<string> {
  return get<string>(`/executions/${executionId}/nodes/${nodeId}/reasoning`)
}

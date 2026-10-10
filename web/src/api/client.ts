import type {
  Event,
  EventsResponse,
  Execution,
  ExecutionsResponse,
  NodeReasoningResponse,
  NodeScoringResponse,
} from './types'

// The runtime serves its API at bare paths (/executions, ...), with no /api
// prefix; an /api path gets a JSON 404. In dev, Vite proxies these paths to the
// Rust runtime (see vite.config.ts). In production the SPA is served from the
// same origin as the runtime.
const BASE = ''

/** Thrown when a response parses as JSON but is not the shape the runtime sends. */
export class UnexpectedResponseError extends Error {
  constructor(path: string, expected: string) {
    super(`Unexpected response from ${path}: expected ${expected}`)
    this.name = 'UnexpectedResponseError'
  }
}

async function get(path: string): Promise<unknown> {
  const res = await fetch(`${BASE}${path}`)
  if (!res.ok) {
    // The runtime answers errors as {"error": "..."}; show that text when present.
    let detail = res.statusText
    try {
      const body = (await res.json()) as { error?: unknown }
      if (typeof body.error === 'string' && body.error.length > 0) detail = body.error
    } catch {
      // Not JSON; keep the status text.
    }
    throw new Error(`API error ${res.status} on ${path}: ${detail}`)
  }
  return res.json()
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

// The checks below are deliberately shallow: they confirm the envelope and the
// fields the UI indexes into, so a runtime change shows up as a query error
// instead of a TypeError thrown during render.

function isEvent(v: unknown): v is Event {
  return (
    isObject(v) &&
    typeof v.id === 'string' &&
    typeof v.sequence === 'number' &&
    typeof v.created_at === 'string' &&
    isObject(v.kind) &&
    typeof v.kind.type === 'string'
  )
}

function isExecution(v: unknown): v is Execution {
  return (
    isObject(v) &&
    typeof v.execution_id === 'string' &&
    typeof v.workflow_id === 'string' &&
    typeof v.status === 'string'
  )
}

function eventArray(path: string, v: unknown, field: string): Event[] {
  const arr = isObject(v) ? v[field] : undefined
  if (!Array.isArray(arr) || !arr.every(isEvent)) {
    throw new UnexpectedResponseError(path, `{"${field}": [event, ...]}`)
  }
  return arr
}

// ─── Executions ───────────────────────────────────────────────────────────────

export async function fetchExecutions(): Promise<ExecutionsResponse> {
  const path = '/executions'
  const body = await get(path)
  const executions = isObject(body) ? body.executions : undefined
  if (!Array.isArray(executions) || !executions.every(isExecution)) {
    throw new UnexpectedResponseError(path, '{"executions": [execution, ...]}')
  }
  return { executions }
}

export async function fetchExecution(id: string): Promise<Execution> {
  const path = `/executions/${encodeURIComponent(id)}`
  const body = await get(path)
  if (!isExecution(body)) {
    throw new UnexpectedResponseError(path, 'an execution object')
  }
  return body
}

// ─── Events ───────────────────────────────────────────────────────────────────

export async function fetchEvents(executionId: string): Promise<Event[]> {
  const path = `/executions/${encodeURIComponent(executionId)}/events`
  return eventArray(path, await get(path), 'events')
}

// ─── Coordinator ──────────────────────────────────────────────────────────────

export async function fetchCoordinatorDecisions(executionId: string): Promise<Event[]> {
  const path = `/executions/${encodeURIComponent(executionId)}/coordinator-decisions`
  return eventArray(path, await get(path), 'events')
}

// ─── Node scoring / reasoning ─────────────────────────────────────────────────
// Both answer 404 when the node has no coordinator events of that kind.

export async function fetchNodeScoring(
  executionId: string,
  nodeId: string,
): Promise<NodeScoringResponse> {
  const path = `/executions/${encodeURIComponent(executionId)}/nodes/${encodeURIComponent(nodeId)}/scoring`
  const body = await get(path)
  return { node_id: nodeId, scoring: eventArray(path, body, 'scoring') }
}

export async function fetchNodeReasoning(
  executionId: string,
  nodeId: string,
): Promise<NodeReasoningResponse> {
  const path = `/executions/${encodeURIComponent(executionId)}/nodes/${encodeURIComponent(nodeId)}/reasoning`
  const body = await get(path)
  return { node_id: nodeId, decisions: eventArray(path, body, 'decisions') }
}

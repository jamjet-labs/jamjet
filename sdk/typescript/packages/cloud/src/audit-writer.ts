import { mkdirSync, appendFileSync } from 'node:fs'
import { join } from 'node:path'
import type { CloudPusher } from './cloud-pusher.js'

export type AdapterName =
  | 'claude-code-hook'
  | 'openai-guardrail'
  | 'mcp-shim'
  | 'python-sdk'
  | 'ts-sdk'

export type HostName =
  | 'claude-code'
  | 'claude-desktop'
  | 'cursor'
  | 'openai-agents-sdk'
  | 'python'
  | 'typescript'
  | 'custom'

export type Decision =
  | 'ALLOWED'
  | 'BLOCKED'
  | 'WAITING_FOR_APPROVAL'
  | 'APPROVED'
  | 'REJECTED'
  | 'BUDGET_EXCEEDED'
  | 'AUDIT'
  | 'ERROR'

export interface AuditEventInput {
  run_id: string
  trace_id?: string
  decision_id?: string
  host: HostName
  server?: string | null
  tool: string
  args: Record<string, unknown>
  decision: Decision
  rule: string | null
  rule_kind: 'allow' | 'block' | 'require_approval' | 'audit' | null
  executed: boolean
  policy_version?: string
}

export interface AuditWriterOptions {
  destination: string
  adapter: AdapterName
  rotateDaily?: boolean
  /**
   * Optional Path B collaborator. If provided, every write() also pushes the
   * event to Cloud's /v1/policy-audit/events inline (fire-and-forget). The
   * local JSONL write is the source of truth — the push is best-effort.
   * Adapters detect Path B via detectPathMode() and pass a CloudPusher here.
   */
  cloudPusher?: CloudPusher | null
}

export class AuditWriter {
  constructor(private options: AuditWriterOptions) {}

  write(event: AuditEventInput): void {
    const ts = new Date().toISOString()
    const rotateDaily = this.options.rotateDaily ?? true
    const dayDir = rotateDaily ? ts.slice(0, 10) : 'all'
    const dir = join(this.options.destination, dayDir)
    mkdirSync(dir, { recursive: true })
    const path = join(dir, `${this.options.adapter}.jsonl`)
    const full = {
      ts,
      run_id: event.run_id,
      trace_id: event.trace_id ?? null,
      decision_id: event.decision_id ?? null,
      adapter: this.options.adapter,
      host: event.host,
      server: event.server ?? null,
      tool: event.tool,
      args: event.args,
      decision: event.decision,
      rule: event.rule,
      rule_kind: event.rule_kind,
      executed: event.executed,
      policy_version: event.policy_version ?? '1',
      schema_version: 1 as const,
    }
    appendFileSync(path, JSON.stringify(full) + '\n', 'utf-8')

    // Path B direct-push. Fire-and-forget — never awaited, never throws.
    // CloudPusher.push already swallows errors and returns false; the extra
    // .catch is belt-and-suspenders so a misconfigured pusher cannot upset
    // the synchronous write() contract.
    if (this.options.cloudPusher) {
      void this.options.cloudPusher.push(full).catch(() => {})
    }
  }
}

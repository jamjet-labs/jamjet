import { mkdirSync, appendFileSync } from 'node:fs'
import { join } from 'node:path'

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
      schema_version: 1,
    }
    appendFileSync(path, JSON.stringify(full) + '\n', 'utf-8')
  }
}

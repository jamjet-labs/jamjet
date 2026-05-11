import {
  writeFileSync,
  unlinkSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
} from 'node:fs'
import { join } from 'node:path'
import { randomBytes } from 'node:crypto'
import type { AdapterName } from './audit-writer.js'

export interface PendingApproval {
  run_id: string
  tool: string
  args: Record<string, unknown>
  adapter: AdapterName
  enqueued_at: string
  status: 'pending'
}

export interface ApprovalResult {
  status: 'approved' | 'rejected'
  reason?: 'timeout' | 'rejected_by_user'
}

export interface ApprovalQueueOptions {
  pendingDir: string
  defaultTimeoutMs?: number
}

interface Waiter {
  resolve: (r: ApprovalResult) => void
  timer: NodeJS.Timeout
}

export class ApprovalQueue {
  private waiters = new Map<string, Waiter>()

  constructor(private options: ApprovalQueueOptions) {
    mkdirSync(options.pendingDir, { recursive: true })
  }

  enqueue(input: { tool: string; args: Record<string, unknown>; adapter: AdapterName }): string {
    const runId = `run_${randomBytes(6).toString('hex')}`
    const pending: PendingApproval = {
      run_id: runId,
      tool: input.tool,
      args: input.args,
      adapter: input.adapter,
      enqueued_at: new Date().toISOString(),
      status: 'pending',
    }
    writeFileSync(this.queuePath(runId), JSON.stringify(pending, null, 2), 'utf-8')
    return runId
  }

  wait(runId: string, timeoutMs?: number): Promise<ApprovalResult> {
    const ms = timeoutMs ?? this.options.defaultTimeoutMs ?? 300_000
    return new Promise<ApprovalResult>((resolve) => {
      const timer = setTimeout(() => {
        this.waiters.delete(runId)
        this.removeQueueFile(runId)
        resolve({ status: 'rejected', reason: 'timeout' })
      }, ms)
      this.waiters.set(runId, { resolve, timer })
    })
  }

  approve(runId: string): boolean {
    return this.complete(runId, { status: 'approved' })
  }

  reject(runId: string): boolean {
    return this.complete(runId, { status: 'rejected', reason: 'rejected_by_user' })
  }

  list(): PendingApproval[] {
    if (!existsSync(this.options.pendingDir)) return []
    const files = readdirSync(this.options.pendingDir).filter((f) => f.endsWith('.json'))
    return files.map((f) => JSON.parse(readFileSync(join(this.options.pendingDir, f), 'utf-8')))
  }

  private complete(runId: string, result: ApprovalResult): boolean {
    const waiter = this.waiters.get(runId)
    if (!waiter) return false
    clearTimeout(waiter.timer)
    this.waiters.delete(runId)
    this.removeQueueFile(runId)
    waiter.resolve(result)
    return true
  }

  private removeQueueFile(runId: string): void {
    const p = this.queuePath(runId)
    if (existsSync(p)) unlinkSync(p)
  }

  private queuePath(runId: string): string {
    return join(this.options.pendingDir, `${runId}.json`)
  }
}

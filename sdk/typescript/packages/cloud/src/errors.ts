export class JamjetBudgetExceeded extends Error {
  readonly spent: number
  readonly limit: number
  constructor(spent: number, limit: number) {
    super(`JamJet budget exceeded: spent $${spent} of $${limit} limit`)
    this.name = 'JamjetBudgetExceeded'
    this.spent = spent
    this.limit = limit
  }
}

export class JamjetPolicyBlocked extends Error {
  readonly toolName: string
  readonly pattern: string
  constructor(toolName: string, pattern: string, opts?: { cause?: unknown }) {
    super(`Tool '${toolName}' blocked by policy '${pattern}'`)
    this.name = 'JamjetPolicyBlocked'
    this.toolName = toolName
    this.pattern = pattern
    if (opts?.cause !== undefined) this.cause = opts.cause
  }
}

export class JamjetApprovalRejected extends Error {
  readonly approvalId: string
  readonly reason?: string
  constructor(approvalId: string, reason?: string) {
    super(`Approval ${approvalId} rejected${reason ? `: ${reason}` : ''}`)
    this.name = 'JamjetApprovalRejected'
    this.approvalId = approvalId
    if (reason !== undefined) this.reason = reason
  }
}

export class JamjetApprovalTimeout extends Error {
  readonly approvalId: string
  readonly timeoutMs: number
  constructor(approvalId: string, timeoutMs: number, opts?: { cause?: unknown }) {
    super(`Approval ${approvalId} did not resolve within ${timeoutMs}ms`)
    this.name = 'JamjetApprovalTimeout'
    this.approvalId = approvalId
    this.timeoutMs = timeoutMs
    if (opts?.cause !== undefined) this.cause = opts.cause
  }
}

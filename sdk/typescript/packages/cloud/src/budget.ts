import { JamjetBudgetExceeded } from './errors.js'

export class BudgetManager {
  private _maxCostUsd: number | null
  private _spent = 0

  constructor(maxCostUsd?: number | null) {
    this._maxCostUsd = maxCostUsd ?? null
  }

  record(costUsd: number): void {
    this._spent += costUsd
  }

  checkOrThrow(opts: { estimatedCost?: number } = {}): void {
    if (this._maxCostUsd === null) return
    const projected = this._spent + (opts.estimatedCost ?? 0)
    if (projected > this._maxCostUsd) {
      throw new JamjetBudgetExceeded(this._spent, this._maxCostUsd)
    }
  }

  setLimit(maxCostUsd: number | null): void {
    this._maxCostUsd = maxCostUsd
  }

  get spent(): number {
    return this._spent
  }

  get remaining(): number | null {
    if (this._maxCostUsd === null) return null
    return Math.max(0, this._maxCostUsd - this._spent)
  }
}

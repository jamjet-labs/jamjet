'use client'

interface Props {
  savedCents: number
}

export function SavingsTicker({ savedCents }: Props) {
  const dollars = (savedCents / 100).toFixed(4)
  return (
    <div className="savings-ticker">
      <span className="savings-ticker-label">cumulative savings</span>
      <span className="savings-ticker-value">${dollars} saved</span>
    </div>
  )
}

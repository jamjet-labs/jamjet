'use client'

import type { Mode } from '../lib/useDemo.js'

interface Props {
  mode: Mode
}

const LABELS: Record<Mode, string> = {
  mock: 'mock',
  live: 'live',
  'live+dashboard': 'live + dashboard',
}

export function ModeBadge({ mode }: Props) {
  return (
    <span className={`mode-badge mode-badge--${mode.replace('+', '-')}`} data-testid="mode-badge">
      {LABELS[mode]}
      {mode === 'live+dashboard' && (
        <a
          href="https://app.jamjet.dev"
          target="_blank"
          rel="noopener noreferrer"
          className="mode-badge-link"
        >
          View in dashboard →
        </a>
      )}
    </span>
  )
}

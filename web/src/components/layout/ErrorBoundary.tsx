import { Component, type ErrorInfo, type ReactNode } from 'react'

interface ErrorBoundaryProps {
  /** Names the region in the message, e.g. "event timeline". */
  label: string
  children: ReactNode
}

interface ErrorBoundaryState {
  error: Error | null
}

/**
 * Catches a render error in its subtree and shows it in place. Without one,
 * React unmounts the whole tree and the page goes blank, which hides the cause.
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: unknown): ErrorBoundaryState {
    return { error: error instanceof Error ? error : new Error(String(error)) }
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error(`[inspector] ${this.props.label} failed to render`, error, info.componentStack)
  }

  render() {
    const { error } = this.state
    if (!error) return this.props.children
    return (
      <div role="alert" className="p-3 text-xs text-red-300 space-y-2">
        <div className="font-medium">The {this.props.label} could not be displayed.</div>
        <pre className="whitespace-pre-wrap break-words font-mono text-red-400">{error.message}</pre>
        <button
          onClick={() => this.setState({ error: null })}
          className="px-2 py-1 rounded border border-red-800 hover:bg-red-950"
        >
          Try again
        </button>
      </div>
    )
  }
}

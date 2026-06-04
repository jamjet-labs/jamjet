'use client'

import { useRef, useEffect, useState } from 'react'
import type { Message, PresetName } from '../lib/useDemo.js'

interface Props {
  messages: Message[]
  sendTurn: (text: string) => Promise<void>
  runPreset: (name: PresetName) => Promise<void>
}

const PRESETS: { name: PresetName; label: string; testId: string }[] = [
  { name: 'ask5', label: 'Ask 5 KB questions', testId: 'preset-ask5' },
  { name: 'enableCache', label: 'Enable cache_inject → re-run', testId: 'preset-enableCache' },
  { name: 'rapidfire', label: 'Rapid-fire (trip budget)', testId: 'preset-rapidfire' },
  { name: 'pii', label: 'Send PII', testId: 'preset-pii' },
  { name: 'refund', label: 'Request refund', testId: 'preset-refund' },
]

export function ChatPanel({ messages, sendTurn, runPreset }: Props) {
  const [inputValue, setInputValue] = useState('')
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  function handleSend() {
    const text = inputValue.trim()
    if (!text) return
    setInputValue('')
    void sendTurn(text)
  }

  function handleKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'Enter') handleSend()
  }

  return (
    <div className="chat-panel">
      <div className="chat-transcript">
        {messages.length === 0 && (
          <p className="chat-empty">Send a message or click a preset to start.</p>
        )}
        {messages.map((msg, i) => (
          <div
            key={i}
            className={`chat-bubble chat-bubble--${msg.role}`}
          >
            <span className="chat-bubble-role">{msg.role === 'user' ? 'You' : 'Agent'}</span>
            <span className="chat-bubble-text">{msg.text}</span>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>

      <div className="chat-input-row">
        <input
          className="chat-input"
          data-testid="chat-input"
          type="text"
          placeholder="Type a message..."
          value={inputValue}
          onChange={(e) => setInputValue(e.target.value)}
          onKeyDown={handleKeyDown}
        />
        <button
          className="chat-send-btn"
          data-testid="send-btn"
          onClick={handleSend}
        >
          Send
        </button>
      </div>

      <div className="chat-presets">
        {PRESETS.map((p) => (
          <button
            key={p.name}
            className="preset-btn"
            data-testid={p.testId}
            onClick={() => void runPreset(p.name)}
          >
            {p.label}
          </button>
        ))}
      </div>
    </div>
  )
}

'use client'

import { useState, useEffect, useCallback, useRef } from 'react'
import type { FeatureEvent } from '../../lib/engine/events.js'

export interface Message {
  role: 'user' | 'assistant'
  text: string
}

export interface Totals {
  spentCents: number
  savedCents: number
}

export type Mode = 'mock' | 'live' | 'live+dashboard'

export interface DemoState {
  messages: Message[]
  events: FeatureEvent[]
  totals: Totals
  cacheInjectOn: boolean
  pendingApprovalId: string | null
  mode: Mode
  sendTurn: (text: string) => Promise<void>
  enableCacheInject: () => Promise<void>
  approve: (id: string, decision: 'approved' | 'rejected') => Promise<void>
  runPreset: (name: PresetName) => Promise<void>
}

export type PresetName = 'ask5' | 'enableCache' | 'rapidfire' | 'pii' | 'refund'

const KB_QUESTIONS = [
  'How do I reset my password?',
  'What is your return policy?',
  'How long does shipping take?',
  'How do I track my order?',
  'Can I change my delivery address after ordering?',
  'What payment methods do you accept?',
  'How do I contact customer support?',
  'What is your refund timeline?',
]

function deriveTotals(events: FeatureEvent[]): Totals {
  let spentCents = 0
  let savedCents = 0
  for (const ev of events) {
    if (ev.kind === 'cost') spentCents += ev.cents
    if (ev.kind === 'cache_saved') savedCents += ev.savedCents
  }
  return { spentCents, savedCents }
}

export function useDemo(): DemoState {
  const [messages, setMessages] = useState<Message[]>([])
  const [events, setEvents] = useState<FeatureEvent[]>([])
  const [cacheInjectOn, setCacheInjectOn] = useState(false)
  const [pendingApprovalId, setPendingApprovalId] = useState<string | null>(null)
  const [mode, setMode] = useState<Mode>('mock')
  const busyRef = useRef(false)

  useEffect(() => {
    fetch('/api/mode')
      .then((r) => r.json())
      .then((data: { mode: Mode }) => setMode(data.mode))
      .catch(() => {})
  }, [])

  const appendEvents = useCallback((newEvents: FeatureEvent[]) => {
    setEvents((prev) => [...prev, ...newEvents])
    // Update pendingApprovalId from new events
    for (const ev of newEvents) {
      if (ev.kind === 'approval_required') {
        setPendingApprovalId(ev.id)
      }
      if (ev.kind === 'approval_resolved') {
        setPendingApprovalId((cur) => (cur === ev.id ? null : cur))
      }
    }
  }, [])

  const sendTurn = useCallback(async (text: string) => {
    setMessages((prev) => [...prev, { role: 'user', text }])
    try {
      const res = await fetch('/api/turn', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text }),
      })
      const data: { reply: string; events: FeatureEvent[] } = await res.json()
      setMessages((prev) => [...prev, { role: 'assistant', text: data.reply }])
      appendEvents(data.events)
    } catch {
      setMessages((prev) => [...prev, { role: 'assistant', text: '(error contacting server)' }])
    }
  }, [appendEvents])

  const enableCacheInject = useCallback(async () => {
    try {
      await fetch('/api/cache-inject', { method: 'POST' })
      setCacheInjectOn(true)
    } catch {
      // ignore
    }
  }, [])

  const approve = useCallback(async (id: string, decision: 'approved' | 'rejected') => {
    try {
      const res = await fetch('/api/approve', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ id, decision }),
      })
      const data: { events: FeatureEvent[] } = await res.json()
      appendEvents(data.events)
      setPendingApprovalId((cur) => (cur === id ? null : cur))
    } catch {
      // ignore
    }
  }, [appendEvents])

  const runPreset = useCallback(async (name: PresetName) => {
    if (busyRef.current) return
    busyRef.current = true
    try {
      if (name === 'ask5') {
        for (let i = 0; i < 5; i++) {
          await sendTurn(KB_QUESTIONS[i % KB_QUESTIONS.length])
        }
      } else if (name === 'enableCache') {
        await enableCacheInject()
        await sendTurn(KB_QUESTIONS[0])
      } else if (name === 'rapidfire') {
        for (let i = 0; i < 8; i++) {
          await sendTurn(KB_QUESTIONS[i % KB_QUESTIONS.length])
        }
      } else if (name === 'pii') {
        await sendTurn('my ssn is 123-45-6789 please help')
      } else if (name === 'refund') {
        await sendTurn('please refund my last order')
      }
    } finally {
      busyRef.current = false
    }
  }, [sendTurn, enableCacheInject])

  const totals = deriveTotals(events)

  return {
    messages,
    events,
    totals,
    cacheInjectOn,
    pendingApprovalId,
    mode,
    sendTurn,
    enableCacheInject,
    approve,
    runPreset,
  }
}

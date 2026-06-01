import type { Metadata } from 'next'
import './globals.css'

export const metadata: Metadata = {
  title: 'JamJet Cost Intelligence — Support Agent',
  description: 'Live demo of JamJet cost intelligence and governance for a support agent.',
}

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  )
}

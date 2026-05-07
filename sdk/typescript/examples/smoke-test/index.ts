import { init } from '@jamjet/cloud/node'
import OpenAI from 'openai'

const apiKey = process.env.JAMJET_API_KEY
const openaiKey = process.env.OPENAI_API_KEY
if (!apiKey || !openaiKey) {
  console.error('Set JAMJET_API_KEY and OPENAI_API_KEY (use jj_local_dev key from seed-local.sh).')
  process.exit(1)
}

await init({
  apiKey,
  project: 'smoke-test',
  apiUrl: process.env.JAMJET_API_URL ?? 'http://localhost:8080',
  debug: true,
})

const openai = new OpenAI({ apiKey: openaiKey })

const res = await openai.chat.completions.create({
  model: 'gpt-4o-mini',
  messages: [{ role: 'user', content: 'say hi in one word' }],
})

console.log('LLM response:', res.choices[0]?.message.content)
console.log('Span emitted — check Supabase studio at http://localhost:54323/project/default/editor')
console.log('SELECT * FROM events ORDER BY created_at DESC LIMIT 5;')

// Give the batcher 3s to flush
await new Promise((r) => setTimeout(r, 3000))

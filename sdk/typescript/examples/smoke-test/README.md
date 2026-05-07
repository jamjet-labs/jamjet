# smoke-test — E2E example against local JamJet Cloud stack

Verifies that `@jamjet/cloud/node` auto-patches the OpenAI SDK and ships spans to a locally running JamJet Cloud API.

## Prerequisites

1. **Local cloud stack** running (from `jamjet-cloud/` repo):
   ```bash
   cd ~/Development/sunil-ws/jamjet-cloud
   cd supabase && supabase start
   cd ../api && cargo run
   ./scripts/seed-local.sh   # creates jj_local_dev key + smoke-test project
   ```

2. **OpenAI API key** in your shell environment.

## Run

```bash
cd sdk/typescript
pnpm install
cd examples/smoke-test
JAMJET_API_KEY=jj_local_dev OPENAI_API_KEY=$OPENAI_API_KEY pnpm start
```

Expected output:

```
LLM response: Hello
Span emitted — check Supabase studio at http://localhost:54323/project/default/editor
```

## Verify in Supabase studio

```sql
SELECT trace_id, span_id, kind, name, model, input_tokens, output_tokens, cost_usd
FROM events
WHERE project_id IN (SELECT id FROM projects WHERE slug='smoke-test')
ORDER BY created_at DESC
LIMIT 5;
```

Expected: one row with `kind='llm_call'`, `name='openai.gpt-4o-mini'`, non-null tokens and cost.

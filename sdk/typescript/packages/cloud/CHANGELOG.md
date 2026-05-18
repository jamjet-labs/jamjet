# Changelog

## 0.4.0-alpha.1 — 2026-05-18 (unreleased)

### Fixed
- CI release workflow now passes `--tag=next` for prerelease versions so they do not clobber the stable `latest` dist-tag. The `0.4.0-alpha.0` tag was created but never published; this `.1` bump pairs with the workflow fix.

## 0.4.0-alpha.0 — 2026-05-17 (unreleased — workflow needed --tag fix; never reached npm)

### Added
- **Cost-waste detection signal:** every Anthropic-patched span now emits an optional `prompt_prefix_hash` (SHA-256 over the normalized first 80% of the input prompt, truncated to 16 hex chars). The Cloud groups spans by this hash to detect repeated uncached prefixes — the foundation of the Phase 8.1 cost-leak detector.
- `Span.setPromptPrefixHash(hash: string | null): void` setter; field surfaces in `SpanEventDict` as optional `prompt_prefix_hash?: string`.
- `runEnforcedCall` accepts a `computePromptPrefixHash` callback (injected by the Node-only Anthropic patcher) — keeps `node:crypto` out of the universal bundle.
- New internal module `src/prefix-hash.ts` (Node-only, behind the `/node` entry).

### Notes
- Alpha. Not yet published to npm. The Cloud-side ingestion of `prompt_prefix_hash` lands in a parallel branch on `jamjet-cloud`. Wait for both before publishing.
- OpenAI patcher is unchanged in this release (TODO marker only); same wiring lands once OpenAI's prompt-shape extractor is written.
- A SpanEventDict with `messages: []` will still receive the empty-prompt sentinel hash `e3b0c44298fc1c14` — Cloud detectors must treat it as a "no prompt" marker and skip it in groupings.

## 0.3.0 — 2026-05-11

### Added
- **`@jamjet/cloud/node` subpath:** `loadPolicy(path?)`, `AuditWriter`, `ApprovalQueue` — Node-only utilities. Import as `import { loadPolicy } from '@jamjet/cloud/node'`. Kept off the universal entry to preserve Cloudflare Workers / Vercel Edge / browser bundle compatibility.
- `loadPolicy(path?)` — load + validate v1 `policy.yaml` from canonical lookup order (`JAMJET_POLICY_FILE` env → cwd `./policy.yaml` → `~/.jamjet/policy.yaml`)
- `AuditWriter` — JSONL append-only writer with daily rotation, v1 schema. Adapter discriminator field in every emitted event.
- `ApprovalQueue` — in-memory queue + filesystem pending dir, default 5-min timeout auto-reject
- New `audit` action on `PolicyAction` for log-only enforcement
- New types: `Policy`, `PolicyRule`, `PolicyBudget`, `AdapterName`, `HostName`, `Decision`, `AuditEventInput`, `PendingApproval`, `ApprovalResult` — exported from BOTH universal and `/node` entries (types are tree-shaken; no runtime cost)

### Fixed
- `PolicyEvaluator.evaluate()` was last-match-wins (no break in the loop). Spec at design §5 says first-match-wins. Fixed; no production policies have overlapping rules so behavior change has zero known impact.

### Changed
- Description rewritten: `@jamjet/cloud` graduates from "Cloud SDK" to the shared engine across all JamJet adapters
- `PolicyAction` union extended from `'allow' | 'block' | 'require_approval'` to include `'audit'`

### Notes
- This release is the foundation for the JamJet Portable Policy Layer (Phase 2). Adapters `@jamjet/claude-code-hook`, `@jamjet/mcp-shim`, and `@jamjet/openai-guardrail` will depend on this engine.

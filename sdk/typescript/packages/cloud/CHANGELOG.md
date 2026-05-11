# Changelog

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

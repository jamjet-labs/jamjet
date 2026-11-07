# Cloud SDK conformance

The TypeScript SDK (`sdk/typescript/packages/cloud`) and the Python SDK
(`sdk/python/jamjet/cloud`) evaluate policy and price calls in-process. These
files are the contract both of them test against, so the two cannot drift.

| File | What it pins | Read by |
|---|---|---|
| `policy-decisions.yaml` | Rule order and decisions. Vendored from `jamjet-labs/jamjet-policy`; change it there first. | `tests/conformance-smoke.test.ts`, `tests/cloud/test_conformance.py` |
| `cloud-sdk-vectors.json` | Tool-name glob matching, the price table, cost and cache-read savings. | `tests/conformance-vectors.test.ts`, `tests/cloud/test_conformance.py` |

## Tool-name globs

A rule's pattern is matched against the whole tool name, after trimming
leading and trailing whitespace from the name (as Cloud does):

- `*` matches any run of characters, including none, `.` and line breaks.
- `?` matches exactly one Unicode code point.
- Every other character is literal. `[`, `]`, `{`, `}`, `!`, `\` and regex
  syntax have no special meaning, so `db.[a-z]*` matches only names that start
  with the literal text `db.[a-z]`.
- Matching is case-sensitive on every platform.

Python's `fnmatch` does not follow this: it treats `[a-z]` as a character class
and is case-insensitive on Windows. The Python SDK therefore uses its own
matcher.

Rules are evaluated in order and the first match wins. No match means allow.

## Prices

`prices.models` lists USD per million tokens. A model ID not in the table is
normalised first (`prices.normalize`; vectors in `normalize`): provider prefix,
Bedrock version, Vertex `@date`, Anthropic `-YYYYMMDD` and OpenAI
`-YYYY-MM-DD` / `-MMDD` dates are removed. A model still unknown is priced at
`prices.fallback`. Cost is `input_tokens * input + output_tokens * output`,
plus, for Anthropic, prompt-cache reads at `cache_read` (0.1x input when none is
listed) and cache writes at 1.25x input, all divided by one million. OpenAI's
`prompt_tokens` already include cached tokens, so nothing is added for it. The cache-read saving for a model is
`(input - cache_read)` per million cache-read tokens. It is zero when no
`cache_read` price is listed. Both SDK tables must equal `prices.models`
exactly; the tests compare them entry by entry.

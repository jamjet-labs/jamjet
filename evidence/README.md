# Evidence

This directory contains reproducible measurements of **JamJet-specific** claims — overhead a policy check adds, time to resume after approval, audit record size. Each subdirectory is a self-contained experiment.

The framing is deliberately not "JamJet vs LangGraph / CrewAI / etc." JamJet sits underneath those frameworks as a safety layer. The right comparison is "JamJet present vs JamJet absent" in the same agent, not "JamJet vs another framework."

## Categories

- `policy-decision-latency/` — overhead a policy check adds to a tool call
- `approval-resume/` — recovery time after a pause for human approval
- `runtime-overhead/` — steady-state overhead vs raw LLM calls
- `audit-export-size/` — audit record size and retention cost

(Older comparison-frame benchmarks live in the separate `jamjet-labs/jamjet-benchmarks` repo, which is being deprecated in favor of this directory.)

# Policy decision latency

Measures the overhead a JamJet policy check adds to a tool call.

**JamJet-specific claim:** a policy check evaluating an N-rule policy against M tool requests resolves in O(N·M) string-pattern operations. This is the cost of agent action control.

Methodology, raw numbers, and reproduction instructions to follow as benchmarks are ported from `jamjet-labs/jamjet-benchmarks`.

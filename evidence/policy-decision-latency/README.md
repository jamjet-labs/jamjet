# Policy decision latency

Measures the overhead a JamJet policy check adds to a tool call.

**JamJet-specific claim:** a policy check evaluating an N-rule policy against M tool requests is expected to scale roughly as O(N·M) under glob-style matching with bounded pattern and tool-name lengths. Exact constants depend on the matcher implementation and will be published with the benchmark methodology.

Methodology, raw numbers, and reproduction instructions to follow as benchmarks are ported from `jamjet-labs/jamjet-benchmarks`.

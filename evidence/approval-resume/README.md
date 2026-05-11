# Approval-resume recovery

Measures the time from a paused (WAITING_FOR_APPROVAL) state to a resumed and completed tool call.

**JamJet-specific claim:** an approval cycle can complete and resume execution within milliseconds once the approval event arrives, regardless of how long the human took to approve.

Methodology and reproduction instructions to follow as benchmarks are ported from `jamjet-labs/jamjet-benchmarks`.

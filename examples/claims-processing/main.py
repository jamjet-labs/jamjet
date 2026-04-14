"""
Insurance Claims Processing Agent — JamJet Example

Demonstrates: durable execution, parallel branches, human-in-the-loop,
eval harness, MCP tools, and full audit trails.

Usage:
    jamjet dev                              # start the runtime
    python examples/claims-processing/main.py  # run the example
    jamjet inspect <exec_id> --events       # inspect execution
"""

import asyncio
from jamjet import JamJetClient


async def main():
    client = JamJetClient()

    # Submit a claim
    result = await client.run(
        "claims-processor",
        input={
            "claim_id": "CLM-4821",
            "customer_id": "CUST-1092",
            "submission": (
                "Severe hail damage to roof on March 28, 2026. "
                "Multiple shingles missing, gutter damage on north side. "
                "Water staining visible on attic ceiling. "
                "Neighbor's property also affected — storm confirmed by NWS."
            ),
            "photo_urls": [
                "s3://claims-bucket/CLM-4821/roof-overview.jpg",
                "s3://claims-bucket/CLM-4821/shingle-damage.jpg",
                "s3://claims-bucket/CLM-4821/gutter-north.jpg",
                "s3://claims-bucket/CLM-4821/attic-staining.jpg",
            ],
        },
    )

    # Print the decision
    print(f"\n{'='*60}")
    print(f"Claim {result.state['claim_id']} — Decision")
    print(f"{'='*60}")
    print(result.state["decision"])

    # Print execution summary
    print(f"\n{'='*60}")
    print("Execution Summary")
    print(f"{'='*60}")
    print(f"Status:    {result.status}")
    print(f"Duration:  {result.duration_ms}ms")
    print(f"Steps:     {len(result.events)}")

    total_tokens = sum(e.tokens for e in result.events if e.tokens)
    total_cost = sum(e.cost for e in result.events if e.cost)
    print(f"Tokens:    {total_tokens:,}")
    print(f"Cost:      ${total_cost:.4f}")

    # Per-step breakdown
    print(f"\n{'='*60}")
    print("Step-by-Step Trace")
    print(f"{'='*60}")
    for event in result.events:
        cost_str = f"${event.cost:.4f}" if event.cost else "—"
        token_str = f"{event.tokens:,}" if event.tokens else "—"
        print(f"  {event.node:20s}  {event.duration_ms:6d}ms  {token_str:>8s} tokens  {cost_str:>8s}")

    print(f"\nInspect: jamjet inspect {result.execution_id} --events")
    print(f"Replay:  jamjet replay {result.execution_id}")


if __name__ == "__main__":
    asyncio.run(main())

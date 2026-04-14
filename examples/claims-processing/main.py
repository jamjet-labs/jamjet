"""
Insurance Claims Processing Agent — JamJet Example
====================================================

Four specialist agents collaborate through a durable workflow to process
an insurance claim end-to-end: intake, assessment, fraud check, and resolution.

Demonstrates:
  - Native Python SDK with Agent, Workflow, @workflow.step
  - Typed Pydantic state flowing through every step
  - Parallel branches (photo analysis + policy lookup run concurrently)
  - Human-in-the-loop approval gate for high-value claims
  - Durable execution — crash at any step, resume from checkpoint
  - Full audit trail with per-step cost/token attribution

Architecture:
  intake → ┬─ analyze_photos ─┬→ assess_damage → check_fraud → approve → resolve
           └─ lookup_policy  ─┘

Run (local in-process):
    python examples/claims-processing/main.py

Run (on JamJet runtime — durable):
    jamjet dev
    python examples/claims-processing/main.py --runtime
    jamjet inspect <exec_id> --events
"""

from __future__ import annotations

import asyncio
import sys

from pydantic import BaseModel

from jamjet import Agent, Workflow, tool


# ═══════════════════════════════════════════════════════════════════════════════
# Tools — structured capabilities the agents can call
# ═══════════════════════════════════════════════════════════════════════════════


@tool
def analyze_photos(photo_urls: list[str]) -> dict:
    """Analyze damage photos using a vision model.

    Returns structured damage assessment: type, severity, affected areas.
    """
    # In production, this calls a vision API or MCP tool server.
    return {
        "damage_type": "hail",
        "severity": 7,
        "affected_areas": ["roof_shingles", "north_gutter", "attic_ceiling"],
        "photo_count": len(photo_urls),
        "consistency": "high — damage pattern consistent across all photos",
    }


@tool
def lookup_policy(customer_id: str) -> dict:
    """Retrieve the customer's insurance policy from the database."""
    # In production, this queries the policy database via MCP.
    return {
        "policy_id": "POL-88431",
        "customer_id": customer_id,
        "coverage_limit": 150_000,
        "deductible": 1_000,
        "covered_perils": ["wind", "hail", "fire", "lightning"],
        "exclusions": ["flood", "earthquake"],
        "effective_date": "2025-06-01",
    }


@tool
def check_fraud(claim_id: str, customer_id: str, estimated_cost: float) -> dict:
    """Cross-reference claim history and flag anomalies."""
    # In production, this calls a fraud detection service.
    return {
        "risk_score": 0.12,
        "flags": [],
        "prior_claims": 1,
        "recommendation": "proceed",
        "details": "Low risk. One prior claim (minor water damage, 2024). No pattern.",
    }


# ═══════════════════════════════════════════════════════════════════════════════
# Agent Definitions
# ═══════════════════════════════════════════════════════════════════════════════

# Intake specialist — extracts structured claim details
intake_agent = Agent(
    name="intake_specialist",
    model="claude-sonnet-4-6",
    tools=[analyze_photos],
    instructions="""You are an insurance claims intake specialist.

Given a claim submission with photos and description, extract:
1. Damage type and severity (1-10 scale)
2. Affected areas of the property
3. Consistency between description and photo evidence
4. Any red flags or inconsistencies

Use analyze_photos to assess the damage photos. Be precise and factual.""",
    strategy="react",
    max_iterations=3,
)

# Damage assessor — estimates repair cost against policy terms
damage_assessor = Agent(
    name="damage_assessor",
    model="claude-sonnet-4-6",
    tools=[lookup_policy],
    instructions="""You are a certified claims assessor.

Given a damage report and claim analysis, you must:
1. Look up the customer's policy using lookup_policy
2. Determine if the damage is covered under the policy terms
3. Estimate the repair cost based on damage severity and affected areas
4. Calculate the net payout (estimate minus deductible)

Be specific with dollar amounts. Justify every line item.""",
    strategy="plan-and-execute",
    max_iterations=5,
)

# Fraud analyst — cross-references history for anomalies
fraud_analyst = Agent(
    name="fraud_analyst",
    model="claude-haiku-4-5-20251001",
    tools=[check_fraud],
    instructions="""You are a fraud detection analyst.

Run the fraud check tool with the claim details. Analyze the results and
provide a clear recommendation: proceed, flag for review, or reject.
Explain your reasoning based on the risk score and any flags.""",
    strategy="react",
    max_iterations=2,
)

# Resolution writer — generates the decision letter
resolution_writer = Agent(
    name="resolution_writer",
    model="claude-sonnet-4-6",
    instructions="""You are a senior claims adjuster writing the final decision letter.

Based on the assessment, fraud check, and any human reviewer notes, write a
professional decision letter that includes:
- Claim decision (approved / denied / partial)
- Authorized amount and deductible applied
- Covered and excluded items
- Payment timeline and next steps for the policyholder
- Appeal process if applicable

The letter must be clear enough for a policyholder with no insurance background.""",
    strategy="critic",
    max_iterations=3,
)


# ═══════════════════════════════════════════════════════════════════════════════
# Workflow Orchestration
# ═══════════════════════════════════════════════════════════════════════════════

workflow = Workflow("claims_processor", version="0.1.0")


@workflow.state
class ClaimsState(BaseModel):
    """Typed state flowing through the claims workflow."""

    claim_id: str
    customer_id: str
    submission: str
    photo_urls: list[str]
    # Populated by agents as they execute
    claim_analysis: str | None = None
    damage_report: str | None = None
    policy: str | None = None
    assessment: str | None = None
    fraud_result: str | None = None
    human_review: str | None = None
    decision: str | None = None


@workflow.step
async def intake(state: ClaimsState) -> ClaimsState:
    """Step 1: Intake specialist analyzes the claim submission and photos."""
    result = await intake_agent.run(
        f"Process this insurance claim:\n\n"
        f"Claim ID: {state.claim_id}\n"
        f"Description: {state.submission}\n"
        f"Photos: {state.photo_urls}"
    )
    return state.model_copy(update={"claim_analysis": result.output})


@workflow.step(parallel=["analyze_photos_step", "lookup_policy_step"])
async def parallel_lookup(state: ClaimsState) -> ClaimsState:
    """Step 2: Photo analysis and policy lookup run concurrently."""
    return state


@workflow.step
async def analyze_photos_step(state: ClaimsState) -> ClaimsState:
    """Step 2a: Deep photo analysis based on intake findings."""
    result = await intake_agent.run(
        f"Perform detailed photo analysis for claim {state.claim_id}.\n"
        f"Photos: {state.photo_urls}\n"
        f"Initial analysis: {state.claim_analysis}\n\n"
        "Focus on damage extent, repair scope, and evidence quality."
    )
    return state.model_copy(update={"damage_report": result.output})


@workflow.step
async def lookup_policy_step(state: ClaimsState) -> ClaimsState:
    """Step 2b: Retrieve policy details from the database."""
    result = await damage_assessor.run(
        f"Look up the insurance policy for customer {state.customer_id}."
    )
    return state.model_copy(update={"policy": result.output})


@workflow.step
async def assess_damage(state: ClaimsState) -> ClaimsState:
    """Step 3: Estimate repair cost against policy terms."""
    result = await damage_assessor.run(
        f"Assess damage and estimate repair cost for claim {state.claim_id}:\n\n"
        f"Damage report:\n{state.damage_report}\n\n"
        f"Policy details:\n{state.policy}\n\n"
        f"Claim analysis:\n{state.claim_analysis}"
    )
    return state.model_copy(update={"assessment": result.output})


@workflow.step
async def check_fraud_step(state: ClaimsState) -> ClaimsState:
    """Step 4: Cross-reference claim history for fraud indicators."""
    result = await fraud_analyst.run(
        f"Run fraud check on claim {state.claim_id} for customer {state.customer_id}. "
        f"Estimated cost from assessment: check the assessment details.\n\n"
        f"Assessment:\n{state.assessment}"
    )
    return state.model_copy(update={"fraud_result": result.output})


@workflow.step(
    name="approve_claim",
    human_approval=True,  # High-value claims pause for human adjuster review
)
async def approve_claim(state: ClaimsState) -> ClaimsState:
    """Step 5: Human approval gate for high-value claims (>$10K).

    The workflow pauses here and waits for a human adjuster to review.
    This pause is durable — it survives process restarts and deployments.
    """
    return state


@workflow.step
async def resolve(state: ClaimsState) -> ClaimsState:
    """Step 6: Generate the final decision letter."""
    human_notes = f"\nHuman reviewer notes:\n{state.human_review}" if state.human_review else ""
    result = await resolution_writer.run(
        f"Write the decision letter for claim {state.claim_id}:\n\n"
        f"Assessment:\n{state.assessment}\n\n"
        f"Fraud check:\n{state.fraud_result}"
        f"{human_notes}"
    )
    return state.model_copy(update={"decision": result.output})


# ═══════════════════════════════════════════════════════════════════════════════
# Runner
# ═══════════════════════════════════════════════════════════════════════════════


async def run_local(claim_id: str = "CLM-4821") -> None:
    """Execute the claims workflow locally (in-process)."""
    print(f"\n{'='*60}")
    print(f"  Claims Processing — {claim_id}")
    print(f"{'='*60}\n")

    initial_state = ClaimsState(
        claim_id=claim_id,
        customer_id="CUST-1092",
        submission=(
            "Severe hail damage to roof on March 28, 2026. "
            "Multiple shingles missing, gutter damage on north side. "
            "Water staining visible on attic ceiling. "
            "Neighbor's property also affected — storm confirmed by NWS."
        ),
        photo_urls=[
            "s3://claims-bucket/CLM-4821/roof-overview.jpg",
            "s3://claims-bucket/CLM-4821/shingle-damage.jpg",
            "s3://claims-bucket/CLM-4821/gutter-north.jpg",
            "s3://claims-bucket/CLM-4821/attic-staining.jpg",
        ],
    )

    result = await workflow.run(initial_state, max_steps=20)

    print(f"\n{'─'*60}")
    print(f"  WORKFLOW COMPLETE")
    print(f"  Steps: {result.steps_executed}  Duration: {result.total_duration_us / 1_000_000:.2f}s")
    print(f"{'─'*60}")

    sections = [
        ("CLAIM ANALYSIS", result.state.claim_analysis),
        ("DAMAGE REPORT", result.state.damage_report),
        ("ASSESSMENT", result.state.assessment),
        ("FRAUD CHECK", result.state.fraud_result),
        ("DECISION LETTER", result.state.decision),
    ]
    for title, content in sections:
        print(f"\n{'═'*60}")
        print(f"  {title}")
        print(f"{'═'*60}")
        print(content or "(not available)")

    print(f"\n  Inspect: jamjet inspect <exec_id> --events")
    print(f"  Replay:  jamjet replay <exec_id>")


async def run_on_runtime(claim_id: str = "CLM-4821") -> None:
    """Submit to JamJet runtime for durable execution."""
    from jamjet import JamjetClient

    async with JamjetClient("http://localhost:7700") as client:
        ir = workflow.compile()
        await client.create_workflow(ir)

        result = await client.start_execution(
            workflow_id="claims_processor",
            input={
                "claim_id": claim_id,
                "customer_id": "CUST-1092",
                "submission": "Severe hail damage to roof...",
                "photo_urls": ["s3://claims-bucket/CLM-4821/roof-overview.jpg"],
            },
        )
        exec_id = result["execution_id"]
        print(f"Execution started: {exec_id}")
        print(f"The workflow will pause at the approval gate for claims >$10K.")
        print(f"To approve: jamjet approve {exec_id} --decision approved")


if __name__ == "__main__":
    claim_id = "CLM-4821"
    runtime_mode = "--runtime" in sys.argv

    for arg in sys.argv[1:]:
        if arg.startswith("CLM-"):
            claim_id = arg

    if runtime_mode:
        asyncio.run(run_on_runtime(claim_id))
    else:
        asyncio.run(run_local(claim_id))

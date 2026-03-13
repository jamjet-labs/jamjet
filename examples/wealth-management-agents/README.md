# Wealth Management Multi-Agent Advisory

A realistic multi-agent system where four specialist agents collaborate to
produce a comprehensive wealth management recommendation for a banking client.

## Scenario

A wealth advisor at a bank needs to produce a portfolio recommendation for a
high-net-worth client. The process requires expertise across four domains that
would typically involve different specialists:

```
Client Request
    │
    ├─▶ Risk Profiler        Assess client risk capacity & willingness
    │       │
    ├─▶ Market Analyst       Research current market conditions
    │       │
    ├─▶ Tax Strategist       Identify tax-optimization opportunities
    │       │
    └─▶ Portfolio Architect  Synthesize all inputs → final recommendation
            │
     [Human Approval Gate]   Senior advisor reviews before delivery
            │
        Final Report
```

## Agents

### 1. Risk Profiler
| Property | Value |
|----------|-------|
| Role | Certified Financial Planner |
| Strategy | `plan-and-execute` (structured multi-step) |
| Tools | `get_client_profile`, `assess_risk_score` |
| Output | Risk score (0-100), category, factor breakdown |

**Why plan-and-execute?** Risk profiling follows a well-defined sequence:
retrieve data → compute metrics → synthesize. Each step builds on the
previous one, making a structured plan optimal.

### 2. Market Analyst
| Property | Value |
|----------|-------|
| Role | CFA charterholder, senior strategist |
| Strategy | `react` (observe-reason-act loop) |
| Tools | `get_market_data` |
| Output | Market outlook, sector analysis, recommendations |

**Why react?** Market analysis is exploratory — the analyst needs to look at
broad data, notice patterns, then drill into specific sectors. The tight
observe-act loop of ReAct is ideal for iterative data exploration.

### 3. Tax Strategist
| Property | Value |
|----------|-------|
| Role | Enrolled Agent (EA) |
| Strategy | `plan-and-execute` |
| Tools | `get_client_profile`, `analyze_tax_implications` |
| Output | Prioritized tax strategies with estimated savings |

**Why plan-and-execute?** Tax strategy is rule-based and systematic. The agent
needs to methodically evaluate all applicable strategies, making a structured
plan the right choice.

### 4. Portfolio Architect
| Property | Value |
|----------|-------|
| Role | Senior portfolio manager (20+ years) |
| Strategy | `critic` (draft-evaluate-refine) |
| Tools | `build_portfolio_allocation`, `check_compliance` |
| Output | Final portfolio recommendation with implementation plan |

**Why critic?** The final recommendation is the most important deliverable.
The critic strategy drafts an initial allocation, evaluates it against all
inputs, and refines it — producing higher-quality output through iteration.

## Setup

```bash
# Install JamJet SDK
cd sdk/python && pip install -e ".[dev]"

# For JamJet implementation (uses OpenAI-compatible API)
export OPENAI_API_KEY="sk-..."

# For Google ADK implementation
pip install google-adk
export GOOGLE_API_KEY="..."
```

## Running

### JamJet — Local In-Process

```bash
cd examples/wealth-management-agents

# Run for default client (Sarah Chen, C-1001)
python jamjet_impl.py

# Run for a different client
python jamjet_impl.py C-1002
```

### JamJet — On Runtime (Durable Execution)

```bash
# Start the JamJet runtime
jamjet dev

# Submit workflow to runtime
python jamjet_impl.py --runtime

# The workflow pauses at the human approval gate.
# Approve to deliver:
jamjet approve <execution_id> --decision approved
```

### Google ADK

```bash
python google_adk_impl.py
python google_adk_impl.py C-1002
```

## File Structure

```
wealth-management-agents/
├── README.md                   This file
├── tools.py                    Shared tool definitions (simulated data sources)
├── jamjet_impl.py              JamJet implementation
├── google_adk_impl.py          Google ADK implementation
└── comparison.md               Detailed comparison of both approaches
```

## How Agent Interactions Work

### JamJet Flow

```
                    Workflow("wealth_management_advisory")
                    ┌────────────────────────────────────┐
                    │                                    │
  AdvisoryState ───▶│  Step 1: assess_risk               │
  {client_id}       │    └─ risk_profiler.run(prompt)     │
                    │       └─ plan-and-execute strategy  │
                    │          └─ get_client_profile()    │
                    │          └─ assess_risk_score()     │
                    │    └─ state.risk_assessment = output │
                    │                                    │
                    │  Step 2: analyze_markets            │
                    │    └─ market_analyst.run(prompt)     │
                    │       └─ react strategy             │
                    │          └─ get_market_data() ×2    │
                    │    └─ state.market_analysis = output │
                    │                                    │
                    │  Step 3: plan_tax_strategy          │
                    │    └─ tax_strategist.run(prompt)    │
                    │       └─ plan-and-execute strategy  │
                    │          └─ get_client_profile()    │
                    │          └─ analyze_tax_impl()      │
                    │    └─ state.tax_strategy = output   │
                    │                                    │
                    │  Step 4: build_recommendation       │
                    │    [HUMAN APPROVAL GATE]            │
                    │    └─ portfolio_architect.run(brief) │
                    │       └─ critic strategy            │
                    │          └─ build_portfolio_alloc() │
                    │          └─ check_compliance()      │
                    │    └─ state.final_recommendation    │
                    │                                    │
                    └────────────────────────────────────┘
                                     │
                               AdvisoryState
                          {risk_assessment, market_analysis,
                           tax_strategy, final_recommendation}
```

**Key points:**
- State is a typed Pydantic model (`AdvisoryState`) — compile-time guarantees
- Each step receives the full state, returns updated state
- Human approval gate is a first-class workflow primitive
- Everything compiles to IR for durable execution on the Rust runtime
- Event sourcing captures every state transition for audit

### Google ADK Flow

```
                    SequentialAgent("wealth_advisor")
                    ┌──────────────────────────────────┐
                    │                                  │
  session.state ───▶│  Sub-agent 1: risk_profiler      │
  {client_id}       │    └─ model call + tool use      │
                    │    └─ writes state['risk_...']    │
                    │                                  │
                    │  Sub-agent 2: market_analyst      │
                    │    └─ model call + tool use       │
                    │    └─ writes state['market_...']  │
                    │                                  │
                    │  Sub-agent 3: tax_strategist      │
                    │    └─ model call + tool use       │
                    │    └─ writes state['tax_...']     │
                    │                                  │
                    │  Sub-agent 4: portfolio_architect │
                    │    └─ model call + tool use       │
                    │    └─ writes state['final_...']   │
                    │                                  │
                    └──────────────────────────────────┘
                                    │
                            session.state
                    {risk_assessment, market_analysis,
                     tax_strategy, final_recommendation}
```

**Key points:**
- State is a plain `dict` — no type safety, no compile-time checks
- Sub-agents write to `session.state` directly (mutable shared state)
- No human approval gate — would need custom implementation
- In-memory only — no durable execution, no replay
- No IR compilation — agents run directly

## Sample Client Profiles

### C-1001: Sarah Chen
- **Age:** 42 | **Income:** $320K | **Net worth:** $2.8M
- **Risk tolerance:** Moderate | **Horizon:** 23 years
- **Goals:** Retirement at 65, college fund (2 kids), vacation home
- **Concern:** Heavy concentration in company RSUs ($450K single stock)

### C-1002: James Rodriguez
- **Age:** 58 | **Income:** $185K | **Net worth:** $4.2M
- **Risk tolerance:** Conservative | **Horizon:** 7 years
- **Goals:** Retire in 7 years, maintain lifestyle, leave inheritance
- **Concern:** Near-retirement, needs income focus and capital preservation

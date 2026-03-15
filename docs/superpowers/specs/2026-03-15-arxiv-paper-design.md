# Design Spec: arXiv Paper — "JamJet: A Durable Runtime for Reliable and Reproducible Multi-Agent Systems"

## Overview

- **Type:** Standalone arXiv systems paper (hybrid systems + research infrastructure)
- **Length:** ~12 pages (double-column, LaTeX)
- **Author:** Solo (affiliated with JamJet Labs)
- **Timeline:** 4-6 weeks, thorough with benchmarks
- **Core thesis:** Durability is not only an operational concern for agent systems, but also a key enabler of reproducible experimental methodology.

## Decisions Made

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Venue | Standalone arXiv, adaptable to workshops later | Establish priority, no page pressure |
| Framing | Hybrid (systems + research infrastructure) | Matches JamJet's dual value proposition |
| Thesis | Durability enables both production reliability AND research reproducibility | Unique insight, connects two audiences |
| Experiments | A (strategy ablation) + B (crash recovery) + C (benchmarks) + E (DCI/LDP case studies) | Covers both runtime and research validation |
| Existing papers (LDP, DCI) | Used as case study experiments (option B) | Grounds paper in real research patterns |
| Authorship | Solo | Independent open-source project |
| Timeline | 4-6 weeks, thorough | Solid benchmarks and polished writing |

---

## Title

**JamJet: A Durable Runtime for Reliable and Reproducible Multi-Agent Systems**

## Abstract

Multi-agent AI systems are increasingly used for complex reasoning and tool-use tasks, but most current frameworks treat execution as ephemeral: failures lose state, runs are difficult to replay, and experimental results are hard to reproduce exactly. We present JamJet, an open-source runtime that makes durable execution a first-class primitive for agent workflows. JamJet represents agent execution as an event-sourced computation over an explicit workflow intermediate representation, enabling crash-safe recovery, provenance-preserving replay, and execution forking for counterfactual analysis. This same systems design supports both production reliability and research rigor: workflows can be resumed after failure, replayed under recorded conditions, and systematically compared across reasoning strategies such as ReAct, reflection, plan-and-execute, debate, and consensus. We implement JamJet as a Rust-based runtime for multi-agent orchestration and evaluate it along four axes: recovery correctness under worker failures, runtime overhead and replay cost, reproducible ablation across reasoning strategies, and end-to-end case studies reproducing recent multi-agent research patterns. Our results suggest that durability is not only an operational concern for agent systems, but also a key enabler of reproducible experimental methodology.

---

## Section Design

### 1. Introduction (~1.5 pages)

**Purpose:** Establish the problem, state the thesis, list contributions.

**Structure:**

**Opening hook (1 paragraph):**
Multi-agent systems are transitioning from research prototypes to deployed applications, but the execution infrastructure has not kept pace. Current frameworks treat agent execution as ephemeral in-memory computation. Worker failures lose accumulated state. Completed runs cannot be exactly replayed. Moving from a research notebook to a production deployment requires substantial re-engineering.

**Problem statement (1 paragraph):**
This architectural gap creates two related problems. For production use: agent workflows that cannot survive failures are unsuitable for long-running, high-stakes, or cost-sensitive tasks. For research: agent experiments that cannot be exactly reproduced — including model calls, tool invocations, and intermediate reasoning — undermine the scientific methodology that multi-agent research depends on. Both problems trace to the same root cause: the absence of durable execution as a first-class runtime primitive.

**Thesis (1 paragraph):**
We argue that durability is not merely an operational concern but a foundational property that simultaneously enables production reliability and reproducible research. An event-sourced execution model — where every state transition is recorded as an immutable event with provenance metadata — provides crash recovery and deterministic replay as two facets of the same mechanism. This observation motivates the design of JamJet: a runtime where durability is the substrate from which both reliability and reproducibility emerge.

**Contributions (enumerated list):**
1. A durable runtime architecture for multi-agent workflows based on event-sourced execution over a canonical intermediate representation (§3)
2. A strategy compilation model that transforms high-level reasoning patterns (ReAct, reflection, consensus, debate, plan-and-execute, critic) into explicit DAGs with enforced resource bounds (§3.4)
3. Research infrastructure primitives — replay, fork, provenance tracking, ExperimentGrid with statistical significance testing — derived directly from the durability model (§4)
4. Native integration of emerging agent interoperability protocols (MCP, A2A) as first-class workflow node types (§3.5)
5. Empirical evaluation across four axes: crash recovery correctness, runtime performance, strategy ablation with statistical analysis, and reproduction of published multi-agent research patterns (§5)

**Figures:**
- Figure 1: High-level architecture diagram showing the relationship between SDK authoring → IR compilation → Rust runtime → event log → research primitives (replay, fork, eval)

---

### 2. Background & Motivation (~1 page)

**Purpose:** Position JamJet in context of existing work and establish the specific gap.

**Structure:**

**2.1 Agent Frameworks Today**
Brief survey of LangGraph, CrewAI, AutoGen, OpenAI Agents SDK, Google ADK. Characterize them along two axes: (a) agent-native primitives (strategies, tools, multi-agent coordination) and (b) execution durability. Key observation: all have strong primitives but ephemeral execution. State lives in Python process memory. No crash recovery. No replay.

**2.2 Durable Execution in Distributed Systems**
Event sourcing as established pattern (Temporal, Azure Durable Functions). These provide strong durability but are not agent-native — no concept of reasoning strategies, tool calling semantics, model provider abstraction, or agent identity. Using Temporal for agents requires encoding all agent semantics in application code.

**2.3 The Gap**
No existing system occupies the intersection: durable execution + agent-native primitives. This gap has concrete consequences:
- Production: a 2-hour agent workflow that crashes at minute 90 must restart from scratch
- Research: a multi-agent experiment cannot be replayed to verify results or forked to test variations
- Protocol integration: MCP and A2A define interoperability but no framework natively supports both as durable workflow steps

**Figures:**
- Figure 2: 2x2 positioning matrix (x-axis: agent-native, y-axis: durable execution). Quadrants: (empty, LangGraph/CrewAI/AutoGen, Temporal/Durable Functions, JamJet)

---

### 3. System Design (~3 pages)

**Purpose:** Present JamJet's architecture as a systems contribution. This is "Act 1."

**Structure:**

**3.1 Architecture Overview**
Six-layer architecture:
1. Authoring layer — YAML definitions, Python SDK (`@task`, `Agent`, `Workflow`), Java SDK
2. Compilation — Schema validation, graph validation, tool contract verification, strategy expansion
3. Execution runtime — Rust-based scheduler, stateless workers, event-sourced state, lease-based work items
4. Protocol layer — MCP client/server, A2A client/server, ProtocolAdapter framework
5. Runtime services — Model providers, tool execution, policy engine, observability (OTel)
6. Control plane — REST/gRPC APIs, CLI, inspection, replay/fork/cancel

**3.2 Canonical Intermediate Representation**
All authoring modes compile to a single IR (WorkflowIr). Describe the IR structure: metadata, state schema, nodes (23 types), edges with conditions, retry/timeout policies, model/tool references, policy bindings, cost/token budgets. Key design decision: the IR is the contract between polyglot SDKs and the Rust runtime. Show how `@task` (3 lines), `Agent` (5 lines), and `Workflow` (full graph control) all produce the same IR.

**3.3 Event-Sourced Execution Model**
Core durability mechanism. Describe:
- Append-only event log with sequence numbers and timestamps
- Event types: WorkflowStarted, NodeScheduled, NodeStarted, NodeCompleted, NodeFailed, RetryScheduled, StrategyLimitHit, etc.
- ProvenanceMetadata per event: model_id, model_version, confidence, verified, source
- State materialization: latest_snapshot + Σ(events since snapshot) → current state
- JSON merge patches (RFC 7396) for incremental state updates
- Snapshot intervals for bounded replay cost
- Correctness property: for any execution, materializing from the initial event log produces identical state

**3.4 Strategy Compilation**
How reasoning strategies are compiled to explicit DAGs rather than implemented as control flow:
- Six strategies: react, plan-and-execute, critic, reflection, consensus, debate
- Each strategy expands into a sub-DAG with internal nodes (prefixed `__` to avoid collision)
- Resource bounds (max_iterations, max_cost_usd, timeout_seconds) wired as guard condition nodes at compile time
- This is a key design insight: strategies are data (graph structure), not code (control flow). This makes them inspectable, comparable, and reproducible.

**3.5 Protocol Integration**
Native MCP and A2A support as first-class node types:
- McpTool nodes: invoke remote tools via MCP protocol (stdio, HTTP-SSE, WebSocket transports)
- A2aTask nodes: delegate to remote agents via A2A protocol with lifecycle management
- Agent Cards: machine-readable agent identity (capabilities, constraints, autonomy level, auth)
- Key insight: protocol interactions are durable events — a crashed workflow resumes after an MCP tool call without re-invoking

**3.6 Scheduler and Worker Model**
- Scheduler tick loop: reclaim expired leases → detect runnable nodes (all predecessors complete) → dispatch to worker queues with backpressure
- Stateless workers: pull work items, claim with leases, emit events, heartbeat for liveness
- Crash recovery: expired leases trigger automatic re-dispatch with exponential backoff
- Concurrency control: max_concurrent_nodes_per_execution, max_dispatch_per_tick

**Figures:**
- Figure 3: IR compilation pipeline (YAML/Python → IR → event-sourced execution)
- Figure 4: Event log and materialization (showing snapshot + event replay → state)
- Figure 5: Strategy compilation example (reflection strategy → explicit DAG with guard nodes)

---

### 4. Research Infrastructure (~1.5 pages)

**Purpose:** Show how durability primitives become research tools. This is "Act 2."

**Key framing:** Each subsection maps a durability mechanism to a research capability. The contribution is not the research tools themselves, but the insight that they are direct consequences of the durability model.

**Structure:**

**4.1 Replay and Reproduction**
- Durability mechanism: append-only event log with full provenance
- Research capability: exact replay of any completed execution
- `jamjet replay <execution_id>` reproduces from event log
- ProvenanceMetadata ensures model version, parameters, and confidence are recorded per node
- Contrast with current practice: researchers manually set seeds and hope for reproducibility

**4.2 Execution Forking**
- Durability mechanism: checkpoint-based materialization at any sequence number
- Research capability: fork an execution from any intermediate state, change inputs or strategy, compare outcomes
- `jamjet fork <execution_id> --from-checkpoint <seq> --override-input`
- Enables counterfactual analysis: "what if agent B had used debate instead of reflection at step 5?"

**4.3 Strategy Comparison via ExperimentGrid**
- Durability mechanism: strategy compilation to explicit DAGs + identical IR execution
- Research capability: systematic ablation across strategies, models, and parameters
- ExperimentGrid API: define conditions (strategies × models), seeds, dataset, scorers → run as durable executions → collect results with statistical tests
- Built-in significance testing: Welch's t-test, Mann-Whitney U, Cohen's d effect sizes, 95% confidence intervals
- Publication-ready export: LaTeX (booktabs), CSV, JSON

**4.4 Inline Evaluation**
- Durability mechanism: eval nodes as first-class workflow nodes (not post-hoc)
- Research capability: quality gates during execution with feedback loops
- EvalNode: scorers (LLM judge with rubric, assertions, latency, cost, custom `@scorer`), on_fail actions (RetryWithFeedback, Escalate, Halt, LogAndContinue)
- Key difference from external eval tools (LangSmith, Braintrust): evaluation is part of the workflow DAG, not a separate pipeline

**4.5 Provenance and Audit**
- Durability mechanism: ProvenanceMetadata on every event
- Research capability: full lineage of every output — which model, which version, what confidence, what source
- Supports accountability requirements for multi-agent research papers
- Event timeline exportable for post-hoc analysis

**Figures:**
- Figure 6: Mapping table — durability mechanism → research primitive (visual summary of §4's argument)
- Figure 7: ExperimentGrid workflow (conditions × seeds → parallel durable executions → statistical comparison → LaTeX)

---

### 5. Evaluation (~3 pages)

**Purpose:** Empirical validation across both runtime (production) and research claims.

**Structure:**

**5.1 Experimental Setup**
- Hardware: specify machine (CPU, RAM, OS)
- JamJet version: v0.2.x (whatever is current at time of experiments)
- Baselines: LangGraph (latest stable) for framework comparison; raw Python asyncio for overhead measurement
- Models: Claude Sonnet 4 and GPT-4o for strategy experiments (specify exact versions)
- Statistical methodology: all experiments run with multiple seeds, significance at α = 0.05

**5.2 Experiment A: Strategy Ablation Study**
- **Goal:** Demonstrate ExperimentGrid for systematic strategy comparison
- **Setup:** Select a benchmark task set (suggest: a subset of tasks from the DCI paper's 45-task corpus, or a standard reasoning benchmark). Run all 6 strategies (react, plan-and-execute, critic, reflection, consensus, debate) × 2 models × 3+ seeds.
- **Metrics:** Task success rate, output quality (LLM judge score 1-5), total cost (tokens), latency, number of iterations
- **Analysis:** Pairwise comparison with Welch's t-test, Cohen's d effect sizes, 95% CI. Show results as LaTeX table generated by ExperimentGrid.to_latex()
- **Expected contribution:** Show that strategy choice significantly affects outcomes and that systematic comparison (not anecdotal selection) is feasible with proper infrastructure

**5.3 Experiment B: Crash Recovery**
- **Goal:** Validate durability guarantees under worker failures
- **Setup:** Run a multi-step workflow (10+ nodes). Kill worker processes at random points during execution (25%, 50%, 75% progress). Measure:
  - Recovery correctness: does the resumed execution produce identical final state?
  - Recovery latency: time from crash to resumed execution
  - Event log integrity: no duplicate or missing events after recovery
- **Baseline comparison:** Run same workflow on LangGraph — show that crash = total state loss, must restart from scratch
- **Methodology:** Repeat each crash scenario N times (e.g., 50), report success rate, mean/p95 recovery time
- **Expected contribution:** Quantitative evidence that event-sourced durability provides correct, fast crash recovery

**5.4 Experiment C: Runtime Performance**
- **Goal:** Quantify JamJet's overhead and demonstrate Rust runtime benefits
- **Metrics:**
  - Dispatch latency: time from node becoming runnable to worker receiving work item (measure p50, p95, p99)
  - Throughput: maximum workflows/second with N concurrent executions
  - Event logging overhead: cost of persisting events vs. ephemeral execution
  - Replay cost: time to materialize state from event log at various event counts (100, 1K, 10K events), with and without snapshots
  - Snapshot effectiveness: replay time as function of snapshot interval
- **Baselines:**
  - Raw Python asyncio (same workflow logic, no framework) — measures framework overhead
  - LangGraph (same workflow) — head-to-head framework comparison
- **Expected contribution:** Show that Rust runtime adds minimal overhead over raw execution while providing durability, and is competitive or faster than Python-only frameworks

**5.5 Experiment E: Case Studies — DCI and LDP Reproduction**
- **Goal:** Demonstrate end-to-end research workflow using JamJet
- **Case Study 1 — DCI Deliberation:**
  - Reproduce the Deliberative Collective Intelligence pattern (arXiv:2603.11781)
  - 4 agents (Framer, Explorer, Challenger, Integrator) with different strategies
  - Run on a subset of the paper's task corpus
  - Use ExperimentGrid to compare DCI vs. single-agent baselines
  - Show: JamJet reproduces the pattern, enables systematic evaluation, exports publication-ready results
- **Case Study 2 — LDP Routing:**
  - Reproduce the Language-agent Dispatch Protocol routing pattern (arXiv:2603.08852)
  - Three-tier agent routing (simple/moderate/complex) based on complexity classification
  - Compare routing-based delegation vs. single-agent execution
  - Show: JamJet enables identity-aware agent delegation as a composable primitive
- **For both case studies:**
  - Full pipeline: define workflow → run ExperimentGrid → statistical comparison → export LaTeX table
  - Demonstrate replay: re-run an execution, verify identical output
  - Demonstrate fork: take a completed execution, change one agent's strategy, compare outcomes
- **Expected contribution:** Evidence that JamJet is practical research infrastructure, not theoretical — real patterns from real papers run end-to-end with reproducibility guarantees

**Figures:**
- Figure 8: Strategy ablation results table (LaTeX, generated by ExperimentGrid)
- Figure 9: Crash recovery timeline (showing crash point, lease expiry, re-dispatch, correct completion)
- Figure 10: Dispatch latency distribution (CDF plot, JamJet vs. LangGraph vs. raw Python)
- Figure 11: Replay cost vs. event count (with/without snapshots)
- Figure 12: DCI case study — workflow DAG visualization + results summary
- Figure 13: LDP case study — routing decision distribution + quality comparison

---

### 6. Related Work (~1 page)

**Purpose:** Position JamJet precisely in the landscape.

**Structure organized by category:**

**6.1 Agent Frameworks**
- LangGraph/LangChain: extensive integrations, large community, but ephemeral execution, proprietary observability (LangSmith)
- CrewAI: role-based multi-agent, but Python-only, no durability, limited eval
- AutoGen (Microsoft): conversational multi-agent patterns, GroupChat, but no persistent state, research-oriented but lacks infrastructure primitives
- OpenAI Agents SDK: tight OpenAI integration, but vendor-locked, no durability
- Google ADK: Vertex AI integration, recent A2A support, but tied to Google Cloud

**6.2 Durable Execution Systems**
- Temporal: gold standard for durable workflows, but not agent-native (no strategies, tool calling, model abstraction). Using Temporal for agents requires encoding all agent semantics in application code.
- Azure Durable Functions: serverless durability, similar gap
- Event sourcing literature: cite foundational work (Fowler, CQRS pattern)

**6.3 Agent Evaluation**
- LangSmith: external eval platform, post-hoc, proprietary
- Braintrust, PromptFoo: eval-focused tools, but separate from execution
- JamJet's difference: eval is a workflow primitive, not a separate pipeline

**6.4 Agent Interoperability Protocols**
- MCP (Model Context Protocol): tool/resource interoperability, growing adoption
- A2A (Agent-to-Agent Protocol): agent delegation, Google-led
- JamJet is the first framework to natively support both as durable workflow node types

**Key positioning claim:** JamJet occupies the intersection of agent-native primitives and durable execution — a space that no existing system addresses.

---

### 7. Discussion & Limitations (~0.5 pages)

**Honest limitations to discuss:**

- **Single-node storage:** Current SQLite backend is suitable for local and single-server deployments. Distributed storage (Postgres, CockroachDB) is planned but not yet production-tested at scale.
- **Benchmark scope:** Performance comparisons are on single-machine workloads. Claims about scaling to thousands of concurrent workflows are architectural projections, not measured.
- **Model dependency:** Strategy ablation results are specific to the models tested (Claude, GPT-4o). Different model families may exhibit different strategy preferences.
- **Replay determinism:** Exact replay requires the same model API to return the same response given the same inputs, which is not guaranteed by model providers. JamJet replays from recorded events (reproducing the execution trace), not by re-invoking models. This distinction should be clearly stated.
- **Protocol maturity:** MCP and A2A are evolving specifications. JamJet's implementations track current spec versions but may require updates.
- **Adoption stage:** JamJet is early-stage open source. Claims about research infrastructure value are supported by case studies but not yet by large-scale community adoption.

---

### 8. Conclusion (~0.5 pages)

**Restate thesis:** Durable execution, implemented through event sourcing over a canonical intermediate representation, provides a foundation that simultaneously addresses production reliability and research reproducibility for multi-agent systems.

**Summary of evidence:** Crash recovery experiments confirm correct and fast resumption. Performance benchmarks show acceptable overhead. Strategy ablation demonstrates systematic comparison infrastructure. Case studies show end-to-end reproduction of published research patterns.

**Future work:**
- Distributed storage backends for horizontal scaling
- Go and TypeScript SDKs for broader polyglot support
- Trust domains and governed delegation for enterprise multi-agent deployments
- Integration with emerging agent protocol extensions
- Community-driven strategy library and evaluation benchmarks

---

## Implementation Plan Summary

### Phase 1: Paper Infrastructure (Week 1)
- Set up LaTeX project (ACM or IEEE double-column template)
- Create figure drafts (architecture diagram, positioning matrix, strategy DAG)
- Write §1 Introduction and §2 Background

### Phase 2: Core Sections (Weeks 1-2)
- Write §3 System Design (architecture, IR, event model, strategies, protocols, scheduler)
- Write §4 Research Infrastructure (replay, fork, ExperimentGrid, eval, provenance)
- Write §6 Related Work

### Phase 3: Experiments (Weeks 2-4)
- Experiment A: Strategy ablation — implement benchmark task set, run ExperimentGrid, collect results
- Experiment B: Crash recovery — build fault injection harness, run scenarios, measure recovery
- Experiment C: Performance benchmarks — measure dispatch latency, throughput, replay cost, compare baselines
- Experiment E: Case studies — run DCI and LDP examples with full eval pipeline, generate LaTeX tables

### Phase 4: Writing & Polish (Weeks 4-5)
- Write §5 Evaluation with actual results and figures
- Write §7 Discussion & Limitations
- Write §8 Conclusion
- Revise abstract with concrete numbers from experiments
- Internal review and revision pass

### Phase 5: Submission (Week 5-6)
- Final proofreading
- Upload to arXiv
- Adapt shorter versions for AAMAS / NeurIPS workshop submissions if desired

---

## Key Risks

| Risk | Mitigation |
|------|------------|
| LangGraph benchmark comparison rejected as unfair | Be transparent about what's compared (durability, not features). Show raw numbers. |
| Strategy ablation shows no significant differences | This is still a valid result — report it honestly. The contribution is the infrastructure, not the outcome. |
| Replay determinism confusion | Clearly distinguish "replay from recorded events" vs. "re-invoke models." JamJet does the former. |
| Paper too long / unfocused | Strict page budget per section. The two-act structure (§3 systems, §4 research) keeps the narrative tight. |
| Missing benchmarks | 4-6 week timeline specifically allocates 2 weeks for experiments. Start early. |

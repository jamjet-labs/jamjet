"""
Built-in project templates for `jamjet init --template <name>`.

Each template is a dict mapping relative file paths to their content.
"""

from __future__ import annotations

# ── Template definitions ───────────────────────────────────────────────────────

_TEMPLATES: dict[str, dict[str, str]] = {
    # ── hello-agent ───────────────────────────────────────────────────────────
    "hello-agent": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Minimal question-answering workflow — the simplest possible JamJet agent.
#
# Usage:
#   jamjet dev                                       # terminal 1 — start runtime
#   jamjet run workflow.yaml --input '{{"query": "What is JamJet?"}}'  # terminal 2

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    query: str
    answer: str
  start: think

nodes:
  think:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: |
      Answer this question clearly and concisely:

      {{{{ state.query }}}}
    next: end

  end:
    type: end
""",
        "evals/dataset.jsonl": """\
{{"id": "q1", "input": {{"query": "What is JamJet?"}}, "expected": {{}}}}
{{"id": "q2", "input": {{"query": "What is a workflow node?"}}, "expected": {{}}}}
{{"id": "q3", "input": {{"query": "How do I connect an MCP server?"}}, "expected": {{}}}}
""",
    },
    # ── research-agent ────────────────────────────────────────────────────────
    "research-agent": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Web search + synthesis workflow (requires Brave Search MCP server).
#
# Prerequisites:
#   export BRAVE_API_KEY=...
#   Configure brave-search MCP server in jamjet.toml
#
# Usage:
#   jamjet run workflow.yaml --input '{{"query": "Latest AI agent frameworks"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    query: str
    search_results: list
    report: str
  start: search

nodes:
  search:
    type: mcp_tool
    server: brave-search
    tool: web_search
    input:
      query: "{{{{ state.query }}}}"
      count: "10"
    retry_policy: io_default
    next: synthesize

  synthesize:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are a research assistant that synthesizes information from web search results
      into clear, structured reports. Always cite your sources.
    prompt: |
      Based on these search results:

      {{{{ state.search_results | join('\\n\\n') }}}}

      Write a comprehensive research report answering:
      {{{{ state.query }}}}

      Include:
      - Key findings
      - Different perspectives where applicable
      - Source citations
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[[mcp_servers]]
name = "brave-search"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
env = {{ BRAVE_API_KEY = "${{BRAVE_API_KEY}}" }}
""",
        "evals/dataset.jsonl": """\
{{"id": "q1", "input": {{"query": "What is event sourcing?"}}, "expected": {{}}}}
{{"id": "q2", "input": {{"query": "How does the A2A protocol work?"}}, "expected": {{}}}}
{{"id": "q3", "input": {{"query": "Best practices for prompt engineering"}}, "expected": {{}}}}
""",
    },
    # ── code-reviewer ─────────────────────────────────────────────────────────
    "code-reviewer": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# GitHub PR review with quality scoring and auto-retry.
#
# Prerequisites:
#   export GITHUB_TOKEN=ghp_...
#   Configure github MCP server in jamjet.toml
#
# Usage:
#   jamjet run workflow.yaml --input '{{"repo": "owner/repo", "pr_number": 42}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    repo: str
    pr_number: int
    pr_data: dict
    review: str
    comment_url: str
  start: fetch-pr

nodes:
  fetch-pr:
    type: mcp_tool
    server: github
    tool: get_pull_request
    input:
      owner: "{{{{ state.repo.split('/')[0] }}}}"
      repo: "{{{{ state.repo.split('/')[1] }}}}"
      pull_number: "{{{{ state.pr_number }}}}"
    next: review

  review:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are an expert code reviewer. Provide thorough, constructive feedback.
      Focus on: correctness, performance, security, readability, and test coverage.
    prompt: |
      Review this pull request:

      Title: {{{{ state.pr_data.title }}}}
      Description: {{{{ state.pr_data.body }}}}

      Diff:
      {{{{ state.pr_data.diff }}}}

      Provide a structured review with:
      1. Summary (1-2 sentences)
      2. Issues found (critical → minor)
      3. Suggestions for improvement
      4. Overall verdict (approve / request changes / comment)
    retry_policy: llm_default
    next: check-quality

  check-quality:
    type: eval
    scorers:
      - type: llm_judge
        rubric: |
          Is this code review thorough, constructive, specific, and well-structured?
        min_score: 4
        model: claude-haiku-4-5-20251001
      - type: assertion
        checks:
          - "len(output['review']) >= 200"
    on_fail: retry_with_feedback
    max_retries: 2
    next: post-comment

  post-comment:
    type: mcp_tool
    server: github
    tool: create_pull_request_review
    input:
      owner: "{{{{ state.repo.split('/')[0] }}}}"
      repo: "{{{{ state.repo.split('/')[1] }}}}"
      pull_number: "{{{{ state.pr_number }}}}"
      body: "{{{{ state.review }}}}"
      event: COMMENT
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[[mcp_servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = {{ GITHUB_TOKEN = "${{GITHUB_TOKEN}}" }}
""",
    },
    # ── single-agent-tool-flow ────────────────────────────────────────────────
    "single-agent-tool-flow": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Single agent that calls one tool then generates a response.
# The simplest useful pattern: tool → model → done.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"path": "README.md", "question": "What does this project do?"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    path: str
    question: str
    file_content: str
    answer: str
  start: read-file

nodes:
  read-file:
    type: mcp_tool
    server: filesystem
    tool: read_file
    input:
      path: "{{{{ state.path }}}}"
    retry_policy: io_default
    next: answer

  answer:
    type: model
    model: claude-haiku-4-5-20251001
    system_prompt: |
      You are a helpful assistant. Answer questions about files clearly and concisely.
    prompt: |
      File: {{{{ state.path }}}}

      Content:
      {{{{ state.file_content }}}}

      Question: {{{{ state.question }}}}
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
""",
        "evals/dataset.jsonl": """\
{{"id": "q1", "input": {{"path": "README.md", "question": "What does this project do?"}}, "expected": {{}}}}
{{"id": "q2", "input": {{"path": "jamjet.toml", "question": "What MCP servers are configured?"}}, "expected": {{}}}}
""",
    },
    # ── hitl-approval ─────────────────────────────────────────────────────────
    "hitl-approval": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Human-in-the-loop review: the agent drafts a reply, a person reviews it, and
# only an approved reply is finalized.
#
# Approving continues the run; rejecting fails it. If the reviewer does not
# decide within 8 hours, the review goes to a second reviewer; if that review
# also expires, the run fails. Nothing is approved automatically.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"request": "Refund order #1234 for $99"}}'
#
# Decide on the paused review:
#   jamjet approvals <exec-id>
#   jamjet approve <exec-id> --decision approved
#   jamjet approve <exec-id> --decision rejected --comment "Too large, check policy first"

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    request: str
  start: draft

nodes:
  draft:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are a support agent. Draft a clear, professional response to customer requests.
      Be specific about what action will be taken.
    prompt: |
      Customer request: {{{{ state.request }}}}

      Draft a response that explains what action you will take and why.
    next: review

  review:
    type: human_approval
    description: Review the drafted response before it is sent
    timeout: 8h
    fallback: second-review
    next: finalize

  second-review:
    type: human_approval
    description: "Escalated: the first reviewer did not decide within 8 hours"
    timeout: 8h
    next: finalize

  finalize:
    type: model
    model: claude-sonnet-4-6
    prompt: |
      A reviewer approved this draft:

      {{{{ state.last_model_output }}}}

      Write the final customer reply, keeping the action and the reasoning.
    next: end

  end:
    type: end
""",
    },
    # ── multi-agent-review ────────────────────────────────────────────────────
    "multi-agent-review": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Writer and critic: a model drafts, an LLM judge scores the draft, and a draft
# below the bar goes back to the writer with the judge's feedback, up to two
# more times. If it still falls short, the run fails.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"topic": "Write a product announcement for JamJet 1.0"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    topic: str
  start: write

nodes:
  write:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are an expert writer. Produce clear, engaging, well-structured content.
      If you have received critique, address every point in this revision.
    prompt: |
      Topic: {{{{ state.topic }}}}
    next: critique

  critique:
    type: eval
    scorers:
      - type: llm_judge
        model: claude-sonnet-4-6
        rubric: |
          You are a strict editor. Is the draft accurate, specific, well structured
          and right for the topic? Score 1-5.
        min_score: 4
    on_fail: retry_with_feedback
    max_retries: 2
    next: end

  end:
    type: end
""",
        "evals/dataset.jsonl": """\
{{"id": "t1", "input": {{"topic": "Write a product announcement for JamJet 1.0"}}, "expected": {{}}}}
{{"id": "t2", "input": {{"topic": "Explain event sourcing to a junior developer"}}, "expected": {{}}}}
""",
    },
    # ── a2a-server ────────────────────────────────────────────────────────────
    "a2a-server": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# A JamJet agent that serves A2A protocol requests.
# External agents can discover this agent via /.well-known/agent.json
# and delegate tasks to it using the A2A standard.
#
# Usage:
#   jamjet dev                     # starts runtime + A2A server
#   curl http://localhost:7700/.well-known/agent.json   # view Agent Card
#
# From another JamJet agent:
#   type: a2a_task
#   remote_agent: "http://localhost:7700"
#   skill: summarize

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    message: str
    result: str
  start: handle

nodes:
  handle:
    type: model
    model: claude-haiku-4-5-20251001
    system_prompt: |
      You are a helpful agent that processes delegated tasks.
      Complete the task clearly and concisely.
    prompt: "{{{{ state.message }}}}"
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[agent]
id = "{name}"
name = "{name}"
version = "0.1.0"
description = "A JamJet agent that accepts A2A task delegations"
url = "http://localhost:7700"

[[agent.skills]]
id = "default"
name = "Default task handler"
description = "Handles general delegated tasks"
input_schema = {{ message = "str" }}
output_schema = {{ result = "str" }}

[[agent.skills]]
id = "summarize"
name = "Summarizer"
description = "Summarizes provided text"
input_schema = {{ message = "str" }}
output_schema = {{ result = "str" }}
""",
    },
    # ── rag-assistant ─────────────────────────────────────────────────────────
    "rag-assistant": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# RAG (Retrieval-Augmented Generation) assistant.
# Reads local files via MCP filesystem, retrieves relevant context, answers questions.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"question": "What does the README say about setup?"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    question: str
    context: str
    answer: str
  start: retrieve

nodes:
  retrieve:
    type: mcp_tool
    server: filesystem
    tool: read_file
    input:
      path: "{{{{ state.question | extract_path }}}}"
    retry_policy: io_default
    next: answer

  answer:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are a helpful assistant that answers questions using the provided context.
      Always ground your answer in the context. If the context does not contain
      enough information, say so clearly.
    prompt: |
      Context:
      {{{{ state.context }}}}

      Question: {{{{ state.question }}}}

      Answer based only on the context above.
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
""",
        "evals/dataset.jsonl": """\
{{"id": "q1", "input": {{"question": "Summarise the project README"}}, "expected": {{}}}}
{{"id": "q2", "input": {{"question": "What are the setup steps?"}}, "expected": {{}}}}
""",
    },
    # ── mcp-tool-consumer ─────────────────────────────────────────────────────
    "mcp-tool-consumer": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Connects to an MCP server and uses its tools inside a workflow.
# Shows how JamJet integrates with any MCP-compatible tool server.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"query": "Search for JamJet on GitHub"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    query: str
    tool_result: str
    summary: str
  start: call-tool

nodes:
  call-tool:
    type: mcp_tool
    server: brave-search
    tool: web_search
    input:
      query: "{{{{ state.query }}}}"
      count: "5"
    retry_policy: io_default
    next: summarize

  summarize:
    type: model
    model: claude-haiku-4-5-20251001
    system_prompt: You are a concise summarizer. Summarize search results in 3-5 sentences.
    prompt: |
      Search results for "{{{{ state.query }}}}":

      {{{{ state.tool_result }}}}

      Write a clear summary.
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

# Connect any MCP server here — brave-search, github, postgres, filesystem, etc.
[[mcp_servers]]
name = "brave-search"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
env = {{ BRAVE_API_KEY = "${{BRAVE_API_KEY}}" }}
""",
        "evals/dataset.jsonl": """\
{{"id": "q1", "input": {{"query": "JamJet AI workflow runtime"}}, "expected": {{}}}}
{{"id": "q2", "input": {{"query": "Model Context Protocol MCP spec"}}, "expected": {{}}}}
""",
    },
    # ── mcp-tool-provider ─────────────────────────────────────────────────────
    "mcp-tool-provider": {
        "server.py": """\
# {name}/server.py
# Expose Python functions as MCP tools — any MCP client can call them.
#
# Usage:
#   python server.py          # starts the MCP server on stdio
#
# Then configure in jamjet.toml:
#   [[mcp_servers]]
#   name = "{name}"
#   command = "python"
#   args = ["server.py"]

from jamjet.protocols.mcp import MCPServer, tool

server = MCPServer("{name}")


@tool(description="Add two numbers together")
def add(a: float, b: float) -> float:
    \"\"\"Add two numbers.\"\"\"
    return a + b


@tool(description="Convert text to uppercase")
def to_upper(text: str) -> str:
    \"\"\"Convert text to uppercase.\"\"\"
    return text.upper()


@tool(description="Get the current UTC timestamp")
def now() -> str:
    \"\"\"Return current UTC time as ISO 8601 string.\"\"\"
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).isoformat()


if __name__ == "__main__":
    server.run_stdio()
""",
        "workflow.yaml": """\
# {name}/workflow.yaml
# Uses the local MCP server defined in server.py.

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    a: float
    b: float
    result: float
    message: str
  start: calculate

nodes:
  calculate:
    type: mcp_tool
    server: {name}
    tool: add
    input:
      a: "{{{{ state.a }}}}"
      b: "{{{{ state.b }}}}"
    next: respond

  respond:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: |
      {{{{ state.a }}}} + {{{{ state.b }}}} = {{{{ state.result }}}}
      Write one sentence confirming this calculation.
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[[mcp_servers]]
name = "{name}"
command = "python"
args = ["server.py"]
""",
    },
    # ── a2a-delegator ─────────────────────────────────────────────────────────
    "a2a-delegator": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Delegates a task to a remote agent via the A2A protocol.
# The remote agent does the work; this workflow collects and presents the result.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"task": "Summarise Q3 sales report", "agent_url": "https://agent.example.com"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    task: str
    agent_url: str
    agent_result: str
    final_summary: str
  start: delegate

nodes:
  delegate:
    type: a2a_task
    remote_agent: "{{{{ state.agent_url }}}}"
    skill: default
    input:
      message: "{{{{ state.task }}}}"
    timeout: 120s
    next: summarize

  summarize:
    type: model
    model: claude-haiku-4-5-20251001
    system_prompt: You are a coordinator agent. Present the delegated result clearly.
    prompt: |
      Task delegated: {{{{ state.task }}}}
      Remote agent result: {{{{ state.agent_result }}}}

      Write a one-paragraph summary of what was accomplished.
    next: end

  end:
    type: end
""",
        "jamjet.toml": """\
[project]
name = "{name}"
version = "0.1.0"

[agent]
id = "{name}-coordinator"
name = "{name} Coordinator"
version = "0.1.0"
""",
        "evals/dataset.jsonl": """\
{{"id": "t1", "input": {{"task": "Summarise this document", "agent_url": "http://localhost:8080"}}, "expected": {{}}}}
""",
    },
    # ── research ───────────────────────────────────────────────────────────────
    "research": {
        "agents/researcher.py": """\
# {name}/agents/researcher.py
# Research agent with tools for paper search and data analysis.
#
# Usage:
#   from agents.researcher import researcher
#   result = await researcher.run("Find papers on transformer architectures")

from jamjet import Agent, tool


@tool
async def search_papers(query: str) -> str:
    \"\"\"Search academic papers by keyword.\"\"\"
    # Replace with a real API (Semantic Scholar, arXiv, etc.)
    return f"Found papers about: {{query}}"


@tool
async def analyze_data(dataset: str) -> str:
    \"\"\"Analyze a dataset and return key findings.\"\"\"
    # Replace with real analysis logic
    return f"Analysis of {{dataset}}: significant results found"


researcher = Agent(
    name="{name}-researcher",
    model="claude-haiku-4-5-20251001",
    tools=[search_papers, analyze_data],
    instructions=(
        "You are a research assistant. Search for papers, analyze data, "
        "and provide clear, well-cited summaries."
    ),
    strategy="react",
    max_iterations=6,
)
""",
        "baselines/baseline.py": """\
# {name}/baselines/baseline.py
# Abstract baseline class and a simple single-agent baseline for comparison.
#
# Usage:
#   from baselines.baseline import SingleAgentBaseline
#   result = await SingleAgentBaseline().run("Summarise recent LLM research")

from abc import ABC, abstractmethod
from dataclasses import dataclass, field


@dataclass
class BaselineResult:
    \"\"\"Container for baseline run outputs.\"\"\"
    output: str
    tokens: int = 0
    latency_ms: float = 0.0


class Baseline(ABC):
    \"\"\"Override `run` to implement a new baseline.\"\"\"

    @abstractmethod
    async def run(self, task: str) -> BaselineResult:
        ...


class SingleAgentBaseline(Baseline):
    \"\"\"Simple single-prompt baseline — no tools, no iteration.\"\"\"

    async def run(self, task: str) -> BaselineResult:
        # Replace with a real model call to measure baseline quality
        return BaselineResult(
            output=f"Baseline response for: {{task}}",
            tokens=0,
            latency_ms=0.0,
        )
""",
        "experiments/config.yaml": """\
# {name}/experiments/config.yaml
# Experiment configuration — models, seeds, and evaluation settings.
#
# Usage:
#   python experiments/runner.py              # run all conditions
#   python experiments/runner.py --seed 42    # run a single seed

experiment:
  name: {name}
  seeds: [42, 123, 456]

  conditions:
    - name: react
      strategy: react
      model: claude-haiku-4-5-20251001
    - name: plan-and-execute
      strategy: plan-and-execute
      model: claude-haiku-4-5-20251001

  eval:
    judge_model: claude-haiku-4-5-20251001
    rubric: "Rate the research quality 1-5: depth, accuracy, clarity"
    min_score: 3

  output_dir: results/
""",
        "experiments/runner.py": """\
# {name}/experiments/runner.py
# Run experiments across conditions and seeds, collect eval results.
#
# Usage:
#   python experiments/runner.py
#   python experiments/runner.py --seed 42

import json
import pathlib

import yaml

from jamjet.eval import EvalRunner


def load_config(path: str = "experiments/config.yaml") -> dict:
    \"\"\"Load experiment configuration from YAML.\"\"\"
    with open(path) as f:
        return yaml.safe_load(f)["experiment"]


async def run_experiments() -> None:
    config = load_config()
    results_dir = pathlib.Path(config["output_dir"])
    results_dir.mkdir(parents=True, exist_ok=True)

    runner = EvalRunner(
        dataset="evals/dataset.jsonl",
        scorers_module="evals.scorers",
        judge_model=config["eval"]["judge_model"],
    )

    all_results = []

    for condition in config["conditions"]:
        for seed in config["seeds"]:
            print(f"Running condition={{condition['name']}} seed={{seed}}")
            result = await runner.run(
                workflow="workflow.yaml",
                strategy=condition["strategy"],
                model=condition["model"],
                seed=seed,
            )
            entry = {{
                "condition": condition["name"],
                "seed": seed,
                "scores": result.scores,
                "pass_rate": result.pass_rate,
            }}
            all_results.append(entry)

    output_path = results_dir / "experiment_results.json"
    output_path.write_text(json.dumps(all_results, indent=2))
    print(f"Results saved to {{output_path}}")


if __name__ == "__main__":
    import asyncio
    asyncio.run(run_experiments())
""",
        "evals/dataset.jsonl": """\
{{"id": "r1", "input": {{"query": "What are the latest advances in transformer architectures?"}}, "expected": {{}}}}
{{"id": "r2", "input": {{"query": "Compare retrieval-augmented generation approaches"}}, "expected": {{}}}}
{{"id": "r3", "input": {{"query": "Summarise key findings on chain-of-thought prompting"}}, "expected": {{}}}}
{{"id": "r4", "input": {{"query": "What are effective evaluation methods for LLM agents?"}}, "expected": {{}}}}
{{"id": "r5", "input": {{"query": "How does RLHF improve language model alignment?"}}, "expected": {{}}}}
""",
        "evals/scorers.py": """\
# {name}/evals/scorers.py
# Custom scorer for research quality evaluation.
#
# Scorers are auto-discovered by EvalRunner when pointed at this module.

from jamjet.eval import Scorer, Score


class ResearchDepthScorer(Scorer):
    \"\"\"Score whether the research output demonstrates sufficient depth.\"\"\"

    name = "research_depth"

    async def score(self, output: str, expected: dict | None = None) -> Score:
        # Simple heuristic — replace with model-graded or custom logic
        word_count = len(output.split())
        if word_count >= 200:
            return Score(value=5, reason=f"Thorough ({{word_count}} words)")
        if word_count >= 100:
            return Score(value=3, reason=f"Adequate ({{word_count}} words)")
        return Score(value=1, reason=f"Too brief ({{word_count}} words)")
""",
        "results/.gitkeep": "",
        "workflow.yaml": """\
# {name}/workflow.yaml
# Multi-step research workflow: search, analyse, evaluate quality.
#
# Usage:
#   jamjet dev
#   jamjet run workflow.yaml --input '{{"query": "Latest advances in LLM agents"}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    query: str
    search_results: str
    analysis: str
    report: str
  start: research

nodes:
  research:
    type: model
    model: claude-haiku-4-5-20251001
    system_prompt: |
      You are a research assistant. Search for relevant papers and information,
      then provide a detailed, well-cited summary.
    prompt: |
      Research the following topic thoroughly:

      {{{{ state.query }}}}

      Provide:
      1. Key findings from recent literature
      2. Different perspectives or approaches
      3. Open questions and future directions
    next: evaluate

  evaluate:
    type: eval
    scorers:
      - type: llm_judge
        rubric: "Is the research thorough and accurate? Rate depth, accuracy, and clarity 1-5."
        min_score: 3
        model: claude-haiku-4-5-20251001
    on_fail: retry_with_feedback
    max_retries: 2
    next: end

  end:
    type: end
""",
        "README.md": """\
# {name}

A research workflow built with [JamJet](https://jamjet.dev).

## Quick start

```bash
# Start the JamJet runtime
jamjet dev

# Run the research workflow
jamjet run workflow.yaml --input '{{"query": "Latest advances in LLM agents"}}'
```

## Run experiments

```bash
# Execute all experiment conditions and seeds
python experiments/runner.py

# Results are saved to results/experiment_results.json
```

## Project structure

```
agents/researcher.py      # Research agent with tools
baselines/baseline.py     # Baseline for comparison
experiments/config.yaml   # Experiment configuration
experiments/runner.py     # Experiment runner
evals/dataset.jsonl       # Sample evaluation dataset
evals/scorers.py          # Custom scorers
workflow.yaml             # Research workflow (YAML)
results/                  # Experiment output
```

## Adding a new condition

Edit `experiments/config.yaml` and add an entry under `conditions`:

```yaml
conditions:
  - name: my-condition
    strategy: react
    model: claude-sonnet-4-6
```

Then re-run `python experiments/runner.py`.

## Exporting results

Results are written as JSON to `results/experiment_results.json`. Load them
in a notebook or script for further analysis:

```python
import json, pathlib
results = json.loads(pathlib.Path("results/experiment_results.json").read_text())
```
""",
    },
    # ── cloud-agent ───────────────────────────────────────────────────────────
    "cloud-agent": {
        "agent.py": """\
# {name}/agent.py
# A JamJet agent with cloud governance — policy enforcement, approval gates,
# cost tracking, and full trace visibility in the JamJet Cloud dashboard.
#
# Prerequisites:
#   pip install jamjet[openai]        # or jamjet[anthropic]
#   export JAMJET_API_KEY=jj_...      # from https://jamjet.cloud/dashboard/settings
#
# Usage:
#   python agent.py

import os

import jamjet.cloud as cloud
from jamjet import tool

# ── 1. Connect to JamJet Cloud ────────────────────────────────────────────────
cloud.init(
    api_key=os.environ["JAMJET_API_KEY"],
    agent_name="{name}",          # appears in the dashboard agent list
    environment="dev",            # dev | staging | prod
)

# Block any tool whose name matches these patterns — enforced server-side.
# Violations appear on the Audit page in the dashboard.
cloud.policy("block", "payments.*")
cloud.policy("block", "*.delete")

# ── 2. Define tools ───────────────────────────────────────────────────────────

@tool
async def summarize_text(text: str) -> str:
    \\"\\"\\"Summarize the provided text in one paragraph.\\"\\"\\"
    # Replace with real logic — e.g. call an LLM or a library
    return f"Summary of {{len(text)}} chars: {{text[:120]}}..."


@tool
async def send_notification(message: str, channel: str = "general") -> str:
    \\"\\"\\"Send a notification to the specified channel.\\"\\"\\"
    # Replace with a real integration (Slack, email, etc.)
    print(f"[notification -> #{{channel}}] {{message}}")
    return "sent"


# ── 3. Run the agent ──────────────────────────────────────────────────────────

async def main() -> None:
    from openai import AsyncOpenAI
    import json

    client = AsyncOpenAI()

    messages = [
        {{
            "role": "system",
            "content": (
                "You are a helpful assistant with access to tools. "
                "Use tools to complete tasks. Never call blocked tools."
            ),
        }},
        {{
            "role": "user",
            "content": "Summarize this paragraph and send it to the #updates channel: "
                       "JamJet is a governance layer for AI agents that enforces policies, "
                       "tracks costs, and gives teams full visibility into what their agents are doing.",
        }},
    ]

    tools_schema = [
        {{
            "type": "function",
            "function": {{
                "name": "summarize_text",
                "description": "Summarize text in one paragraph",
                "parameters": {{
                    "type": "object",
                    "properties": {{"text": {{"type": "string"}}}},
                    "required": ["text"],
                }},
            }},
        }},
        {{
            "type": "function",
            "function": {{
                "name": "send_notification",
                "description": "Send a notification to a channel",
                "parameters": {{
                    "type": "object",
                    "properties": {{
                        "message": {{"type": "string"}},
                        "channel": {{"type": "string", "default": "general"}},
                    }},
                    "required": ["message"],
                }},
            }},
        }},
    ]

    # Agentic loop
    while True:
        response = await client.chat.completions.create(
            model="gpt-4o-mini",
            messages=messages,
            tools=tools_schema,
            tool_choice="auto",
        )
        msg = response.choices[0].message
        messages.append(msg)

        if not msg.tool_calls:
            print("Agent:", msg.content)
            break

        for call in msg.tool_calls:
            name = call.function.name
            args = json.loads(call.function.arguments)

            if name == "summarize_text":
                result = await summarize_text(**args)
            elif name == "send_notification":
                result = await send_notification(**args)
            else:
                result = f"Unknown tool: {{name}}"

            messages.append({{
                "role": "tool",
                "tool_call_id": call.id,
                "content": str(result),
            }})

    # View the trace in your dashboard:
    # https://jamjet.cloud/dashboard/traces


if __name__ == "__main__":
    import asyncio
    asyncio.run(main())
""",
        "README.md": """\
# {name}

A governed AI agent built with [JamJet Cloud](https://jamjet.cloud).

## Setup

```bash
pip install jamjet[openai]
export JAMJET_API_KEY=jj_...   # https://jamjet.cloud/dashboard/settings
```

## Run

```bash
python agent.py
```

Then open the [JamJet Cloud dashboard](https://jamjet.cloud/dashboard) to see:

- **Traces** — full event timeline for every run
- **Audit** — any policy violations (e.g. blocked tool calls)
- **Costs** — per-model spend with recommendations
- **Approvals** — human-in-the-loop gates for sensitive actions

## What's wired up

| Feature | How |
|---|---|
| Policy enforcement | `cloud.policy("block", "payments.*")` — server-side, no bypass |
| Trace visibility | Every LLM call and tool call appears in the dashboard |
| Cost tracking | Automatic per-model attribution |
| Agent identity | `agent_name="{name}"` — shows in the Agents view |
| Environment | `environment="dev"` — filter traces by env in the dashboard |

## Add an approval gate

Wrap any sensitive action in a cloud approval:

```python
from jamjet.cloud import require_approval

approval = await require_approval(
    action="send_notification",
    context={{"message": message, "channel": channel}},
)
if approval.approved:
    await send_notification(message, channel)
```

Approvals surface in `/dashboard/approvals` for your team to review.
""",
    },
    # ── approval-workflow ─────────────────────────────────────────────────────
    "approval-workflow": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Human approval gate: execution pauses until a person approves the plan.
#
# Approving continues the run; rejecting fails it. If nobody decides within
# 24 hours, the approval goes to a second approver; if that one also expires,
# the run fails. Nothing is approved automatically.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"task": "Deploy v2.0 to production"}}'
#   jamjet approvals <exec-id>
#   jamjet approve <exec-id> --decision approved

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    task: str
  start: propose

nodes:
  propose:
    type: model
    model: claude-sonnet-4-6
    system_prompt: |
      You are an operations agent. Propose a safe, detailed execution plan.
      Be specific about what actions will be taken and their risks.
    prompt: |
      Propose an execution plan for this task:

      {{{{ state.task }}}}

      Include:
      1. Steps to execute
      2. Estimated impact
      3. Rollback plan if something goes wrong
      4. Risk assessment (low / medium / high)
    next: await-approval

  await-approval:
    type: human_approval
    description: Approve the proposed execution plan
    timeout: 24h
    fallback: escalate
    next: announce

  escalate:
    type: human_approval
    description: "Escalated: the first approver did not decide within 24 hours"
    timeout: 24h
    next: announce

  announce:
    type: model
    model: claude-sonnet-4-6
    prompt: |
      This execution plan was approved:

      {{{{ state.last_model_output }}}}

      Write a short change announcement for the team: what will happen,
      when, and how to roll it back.
    next: end

  end:
    type: end
""",
    },
    # ── quickstart ────────────────────────────────────────────────────────────
    "quickstart": {
        "agent.py": """\
\"\"\"
{name} -- a minimal JamJet agent.

Run in-process (no dev server needed):
    python agent.py
\"\"\"

from __future__ import annotations

import asyncio

from jamjet import Agent, tool


@tool
async def add(a: float, b: float) -> str:
    \"\"\"Add two numbers and return the sum.\"\"\"
    return f"{{a + b:g}}"


agent = Agent(
    "{name}",
    model="anthropic/claude-sonnet-4-6",
    tools=[add],
    instructions="You are a helpful assistant. Use the add tool when asked to sum numbers.",
    strategy="react",
)


async def main() -> None:
    result = await agent.run("What is 7 plus 35?")
    print(result.output)


if __name__ == "__main__":
    asyncio.run(main())
""",
        "pyproject.toml": """\
[project]
name = "{name}"
version = "0.1.0"
requires-python = ">=3.11"
# The [model] extra pulls in litellm, which the in-process model seam uses to
# call the agent's "anthropic/claude-sonnet-4-6" model. Without it `agent.run()`
# raises at the first model call.
dependencies = ["jamjet[model]>=0.10.0"]

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
""",
        "README.md": """\
# {name}

A minimal JamJet agent built with the `quickstart` template.

## Prerequisites

```bash
pip install 'jamjet[model]'
export ANTHROPIC_API_KEY=sk-ant-...
```

## Run in-process

```bash
python agent.py
```

Runs the agent via `LocalRuntime` -- no server needed.

## Run with the durable engine

Start the local stack:

```bash
jamjet dev
```

Then in a second terminal:

```bash
python agent.py
```

The durable engine records every turn to SQLite so you can inspect, replay, and fork runs.

```bash
jamjet inspect <execution-id>
jamjet events  <execution-id>
```
""",
    },
}

# ── Public API ────────────────────────────────────────────────────────────────

# Templates whose workflow needs a node kind the engine does not run, so a
# project made from them could not run. `jamjet init` refuses them with this
# reason; they come back when the engine runs those nodes.
HELD_BACK_TEMPLATES: dict[str, str] = {
    "a2a-delegator": "it delegates with an a2a_task node, which the engine does not run",
    "a2a-server": "it relies on jamjet.toml to publish an Agent Card, and nothing reads jamjet.toml",
    "code-reviewer": "it calls GitHub through mcp_tool nodes, which the engine does not run",
    "mcp-tool-consumer": "it calls an MCP server through an mcp_tool node, which the engine does not run",
    "mcp-tool-provider": "its workflow calls the server through an mcp_tool node, which the engine does not run",
    "rag-assistant": "it retrieves files through an mcp_tool node, which the engine does not run",
    "research-agent": "it searches through an mcp_tool node, which the engine does not run",
    "single-agent-tool-flow": "it reads files through an mcp_tool node, which the engine does not run",
}

AVAILABLE_TEMPLATES: list[str] = sorted(name for name in _TEMPLATES if name not in HELD_BACK_TEMPLATES)


def get_template(name: str) -> dict[str, str]:
    """Return the file map for a template, with {name} placeholders un-substituted.

    Raises KeyError if the template does not exist.
    """
    if name in HELD_BACK_TEMPLATES:
        raise KeyError(f"Template '{name}' is not available in this release: {HELD_BACK_TEMPLATES[name]}.")
    if name not in _TEMPLATES:
        raise KeyError(f"Unknown template '{name}'. Available: {', '.join(AVAILABLE_TEMPLATES)}")
    return _TEMPLATES[name]


def render_template(name: str, project_name: str) -> dict[str, str]:
    """Return a file map for *name* with ``{name}`` replaced by *project_name*."""
    raw = get_template(name)
    return {path: content.format(name=project_name) for path, content in raw.items()}

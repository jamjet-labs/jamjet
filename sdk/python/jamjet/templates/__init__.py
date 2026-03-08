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
    output_key: answer
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
    type: tool
    server: brave-search
    tool: web_search
    arguments:
      query: "{{{{ state.query }}}}"
      count: 10
    output_key: search_results
    retry:
      max_attempts: 3
      backoff: exponential
      delay_ms: 1000
    next: synthesize

  synthesize:
    type: model
    model: claude-sonnet-4-6
    system: |
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
    output_key: report
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
    type: tool
    server: github
    tool: get_pull_request
    arguments:
      owner: "{{{{ state.repo.split('/')[0] }}}}"
      repo: "{{{{ state.repo.split('/')[1] }}}}"
      pull_number: "{{{{ state.pr_number }}}}"
    output_key: pr_data
    next: review

  review:
    type: model
    model: claude-sonnet-4-6
    system: |
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
    output_key: review
    retry:
      max_attempts: 2
      backoff: constant
      delay_ms: 2000
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
        check: "len(output['review']) >= 200"
    on_fail: retry_with_feedback
    max_retries: 2
    next: post-comment

  post-comment:
    type: tool
    server: github
    tool: create_pull_request_review
    arguments:
      owner: "{{{{ state.repo.split('/')[0] }}}}"
      repo: "{{{{ state.repo.split('/')[1] }}}}"
      pull_number: "{{{{ state.pr_number }}}}"
      body: "{{{{ state.review }}}}"
      event: COMMENT
    output_key: comment_url
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
    # ── approval-workflow ─────────────────────────────────────────────────────
    "approval-workflow": {
        "workflow.yaml": """\
# {name}/workflow.yaml
# Human-in-the-loop approval gate — execution pauses until a human approves.
#
# Usage:
#   jamjet run workflow.yaml --input '{{"task": "Deploy v2.0 to production"}}'
#   jamjet resume <exec-id> --event human_approved --data '{{"approved": true}}'

workflow:
  id: {name}
  version: 0.1.0
  state_schema:
    task: str
    proposal: str
    approved: bool
    result: str
    rejection_reason: str
  start: propose

nodes:
  propose:
    type: model
    model: claude-sonnet-4-6
    system: |
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
    output_key: proposal
    next: await-approval

  await-approval:
    type: wait
    event: human_approved
    timeout_hours: 24
    on_timeout: escalate
    next: check-approved

  check-approved:
    type: branch
    conditions:
      - if: "state.approved == true"
        next: execute
    default: rejected

  execute:
    type: model
    model: claude-sonnet-4-6
    prompt: |
      Execute this approved plan step by step:

      Task: {{{{ state.task }}}}
      Plan: {{{{ state.proposal }}}}

      Report what was done and confirm completion.
    output_key: result
    next: end

  rejected:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: |
      The following plan was rejected: {{{{ state.proposal }}}}
      Reason: {{{{ state.rejection_reason }}}}
      Acknowledge the rejection and suggest alternatives.
    output_key: result
    next: end

  escalate:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: |
      The approval for the following task timed out after 24 hours:
      {{{{ state.task }}}}
      Draft an escalation notice for the on-call team.
    output_key: result
    next: end

  end:
    type: end
""",
    },
}

# ── Public API ────────────────────────────────────────────────────────────────

AVAILABLE_TEMPLATES: list[str] = sorted(_TEMPLATES.keys())


def get_template(name: str) -> dict[str, str]:
    """Return the file map for a template, with {name} placeholders un-substituted.

    Raises KeyError if the template does not exist.
    """
    if name not in _TEMPLATES:
        raise KeyError(f"Unknown template '{name}'. Available: {', '.join(AVAILABLE_TEMPLATES)}")
    return _TEMPLATES[name]


def render_template(name: str, project_name: str) -> dict[str, str]:
    """Return a file map for *name* with ``{name}`` replaced by *project_name*."""
    raw = get_template(name)
    return {path: content.format(name=project_name) for path, content in raw.items()}

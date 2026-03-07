use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type NodeId = String;

/// The lifecycle status of a single node within an execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Scheduled,
    Running,
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

impl NodeStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Skipped | Self::Cancelled
        )
    }
}

/// All node kinds supported by the JamJet runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// LLM call with a prompt and structured output.
    Model {
        model_ref: String,
        prompt_ref: String,
        output_schema: String,
        system_prompt: Option<String>,
    },

    /// Python function, HTTP endpoint, or gRPC tool.
    Tool {
        tool_ref: String,
        input_mapping: HashMap<String, String>,
        output_schema: String,
    },

    /// Arbitrary Python function executed by a Python worker.
    PythonFn {
        module: String,
        function: String,
        output_schema: String,
    },

    /// Router — evaluates expressions and branches.
    Condition { branches: Vec<ConditionalBranch> },

    /// Fan-out to multiple branches concurrently.
    Parallel { branches: Vec<NodeId> },

    /// Waits for all parallel branches to complete.
    Join {
        wait_for: Vec<NodeId>,
        merge_strategy: MergeStrategy,
    },

    /// Pauses workflow for human decision.
    HumanApproval {
        description: String,
        timeout_secs: Option<u64>,
        fallback_node: Option<NodeId>,
    },

    /// Suspends until a timer fires or external event arrives.
    Wait {
        condition: WaitCondition,
        correlation_key: Option<String>,
        timeout_secs: Option<u64>,
    },

    /// Executes a child workflow.
    Subgraph {
        workflow_ref: String,
        workflow_version: Option<String>,
        input_mapping: HashMap<String, String>,
        output_mapping: HashMap<String, String>,
    },

    /// Retrieves context from a memory/retrieval connector.
    MemoryRetrieval {
        connector_ref: String,
        query_expr: String,
        output_schema: String,
    },

    /// Evaluates policy rules; can block or branch on violation.
    Policy {
        policy_ref: String,
        on_violation: ViolationAction,
    },

    /// Side-effect node (notifications, writes).
    Finalizer {
        tool_ref: String,
        run_on: FinalizerTrigger,
    },

    // ── Protocol nodes ──────────────────────────────────────────────────
    /// Delegates to a local JamJet agent.
    Agent {
        agent_ref: String,
        input_mapping: HashMap<String, String>,
        output_schema: String,
    },

    /// Invokes a tool from an external MCP server.
    McpTool {
        server: String,
        tool: String,
        input_mapping: HashMap<String, String>,
        output_schema: String,
    },

    /// Delegates a task to an external A2A agent.
    A2aTask {
        remote_agent: String,
        skill: String,
        input_mapping: HashMap<String, String>,
        output_schema: String,
        stream: bool,
        on_input_required: Option<NodeId>,
        timeout_secs: Option<u64>,
    },

    /// Dynamically discovers and selects an agent at runtime.
    AgentDiscovery {
        skill: String,
        protocol: Option<String>,
        output_binding: String,
    },

    /// Evaluates the preceding node's output using configurable scorers.
    ///
    /// Supports LLM-judge, deterministic assertions, latency/cost thresholds,
    /// and custom Python scorer plugins.
    Eval {
        /// Ordered list of scorer configurations.
        scorers: Vec<EvalScorer>,
        /// Action on overall failure (any scorer below threshold).
        on_fail: EvalOnFail,
        /// Maximum retry attempts before propagating failure.
        #[serde(default)]
        max_retries: u32,
        /// Input expression — which state field to evaluate (default: last node output).
        input_expr: Option<String>,
    },
}

impl NodeKind {
    /// Returns the queue type this node should be dispatched to.
    pub fn queue_type(&self) -> QueueType {
        match self {
            Self::Model { .. } => QueueType::Model,
            Self::Tool { .. } | Self::Finalizer { .. } => QueueType::Tool,
            Self::PythonFn { .. } => QueueType::PythonTool,
            Self::MemoryRetrieval { .. } => QueueType::Retrieval,
            Self::McpTool { .. } | Self::A2aTask { .. } => QueueType::Tool,
            Self::Agent { .. } => QueueType::General,
            Self::HumanApproval { .. } | Self::Wait { .. } => QueueType::General,
            Self::Eval { .. } => QueueType::General,
            _ => QueueType::General,
        }
    }

    /// Returns true if this node requires durable tracking across crashes.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Self::Condition { .. } | Self::AgentDiscovery { .. })
    }
}

/// Which queue a node's work item is dispatched to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueType {
    Model,
    Tool,
    PythonTool,
    Retrieval,
    Privileged,
    General,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalBranch {
    pub condition: Option<String>, // None = default/else branch
    pub target: NodeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Merge all branch outputs into a list.
    Collect,
    /// Take the first completed branch output.
    First,
    /// Custom merge function.
    Custom { function_ref: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitCondition {
    Timer,
    ExternalEvent,
    Either,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationAction {
    Fail,
    Branch { target: NodeId },
    Warn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizerTrigger {
    Success,
    Failure,
    Always,
}

// ── Eval node types ──────────────────────────────────────────────────────────

/// A scorer within an `Eval` node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvalScorer {
    /// LLM-as-judge: sends output to a model with a rubric, expects a score 1-5.
    LlmJudge {
        model: String,
        rubric: String,
        /// Minimum acceptable score (1-5). Scores below this fail.
        #[serde(default = "default_min_score")]
        min_score: u8,
    },
    /// Deterministic Python expressions evaluated against the output.
    Assertion {
        /// Each check is a Python expression that must evaluate to truthy.
        checks: Vec<String>,
    },
    /// Ensures node execution completed within a latency threshold.
    Latency {
        /// Maximum allowed duration in milliseconds.
        threshold_ms: u64,
    },
    /// Ensures the execution cost is within budget.
    Cost {
        /// Maximum allowed cost in USD.
        threshold_usd: f64,
    },
    /// Custom Python scorer loaded via entry point or module path.
    Custom {
        /// Python dotted path: "my_package.scorers:MyScorer"
        module: String,
        /// Optional keyword arguments passed to the scorer.
        #[serde(default)]
        kwargs: serde_json::Value,
    },
}

fn default_min_score() -> u8 {
    3
}

/// What the eval node does when one or more scorers fail.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalOnFail {
    /// Feed scorer feedback back to the previous node and retry.
    RetryWithFeedback,
    /// Escalate to human (triggers HumanApproval fallback node).
    Escalate,
    /// Fail the workflow immediately.
    #[default]
    Halt,
    /// Record the failure but continue the workflow.
    LogAndContinue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_node_dispatches_to_model_queue() {
        let node = NodeKind::Model {
            model_ref: "openai.gpt4".into(),
            prompt_ref: "prompts/summarize.md".into(),
            output_schema: "schemas.Summary".into(),
            system_prompt: None,
        };
        assert_eq!(node.queue_type(), QueueType::Model);
        assert!(node.is_durable());
    }

    #[test]
    fn condition_node_is_not_durable() {
        let node = NodeKind::Condition { branches: vec![] };
        assert!(!node.is_durable());
    }
}

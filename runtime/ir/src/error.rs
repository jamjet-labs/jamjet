use thiserror::Error;

#[derive(Debug, Error)]
pub enum IrError {
    #[error("node '{0}' references unknown tool '{1}'")]
    UnknownToolRef(String, String),

    #[error("node '{0}' references unknown model '{1}'")]
    UnknownModelRef(String, String),

    #[error("node '{0}' references unknown agent '{1}'")]
    UnknownAgentRef(String, String),

    #[error("node '{0}' references unknown MCP server '{1}'")]
    UnknownMcpServer(String, String),

    #[error("node '{0}' references unknown remote agent '{1}'")]
    UnknownRemoteAgent(String, String),

    #[error("edge from '{from}' to '{to}': target node does not exist")]
    UnknownEdgeTarget { from: String, to: String },

    #[error("node '{0}' is unreachable from the start node")]
    UnreachableNode(String),

    #[error("workflow has no start node defined")]
    NoStartNode,

    #[error("workflow has no terminal (end) path from node '{0}'")]
    NoTerminalPath(String),

    #[error("parallel node '{0}' and its join node '{1}' are mismatched")]
    MismatchedParallelJoin(String, String),

    #[error("schema incompatibility: node '{from}' output schema '{from_schema}' is incompatible with node '{to}' input schema '{to_schema}'")]
    SchemaIncompatibility {
        from: String,
        from_schema: String,
        to: String,
        to_schema: String,
    },

    #[error("workflow version '{0}' is not valid semver")]
    InvalidVersion(String),

    #[error("duplicate node id: '{0}'")]
    DuplicateNodeId(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub type IrResult<T> = Result<T, IrError>;

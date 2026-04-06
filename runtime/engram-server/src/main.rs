use clap::Parser;
use engram::embedding::MockEmbeddingProvider;
use engram::memory::Memory;
use engram_server::handlers;
use engram_server::mcp::{McpServer, McpToolDef};
use serde_json::json;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "engram", about = "Engram — memory layer for AI agents")]
enum Cli {
    /// Start the MCP stdio server
    Serve {
        /// Path to SQLite database file
        #[arg(long, default_value = "engram.db")]
        db: String,

        /// Embedding dimensions (for mock provider)
        #[arg(long, default_value = "64")]
        dims: usize,
    },
}

fn tool_defs() -> Vec<McpToolDef> {
    vec![
        McpToolDef {
            name: "memory_add".into(),
            description: "Extract and store facts from conversation messages".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string" },
                                "content": { "type": "string" }
                            },
                            "required": ["role", "content"]
                        },
                        "description": "Conversation messages to extract facts from"
                    },
                    "user_id": { "type": "string", "description": "User identifier" },
                    "org_id": { "type": "string", "description": "Organization identifier" },
                    "session_id": { "type": "string", "description": "Session identifier" }
                },
                "required": ["messages"]
            }),
        },
        McpToolDef {
            name: "memory_recall".into(),
            description: "Semantic search over stored facts".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" },
                    "max_results": { "type": "integer", "default": 10 }
                },
                "required": ["query"]
            }),
        },
        McpToolDef {
            name: "memory_context".into(),
            description: "Assemble a token-budgeted context block for LLM prompts".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Query to build context for" },
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" },
                    "token_budget": { "type": "integer", "default": 2000 },
                    "format": { "type": "string", "enum": ["system_prompt", "markdown", "raw"], "default": "system_prompt" }
                },
                "required": ["query"]
            }),
        },
        McpToolDef {
            name: "memory_forget".into(),
            description: "Soft-delete a fact by ID".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "fact_id": { "type": "string", "description": "UUID of the fact to forget" },
                    "reason": { "type": "string", "description": "Reason for forgetting" }
                },
                "required": ["fact_id"]
            }),
        },
        McpToolDef {
            name: "memory_search".into(),
            description: "Keyword search over stored facts (FTS5)".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keyword search query" },
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" },
                    "top_k": { "type": "integer", "default": 10 }
                },
                "required": ["query"]
            }),
        },
        McpToolDef {
            name: "memory_stats".into(),
            description: "Return aggregate memory statistics".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        McpToolDef {
            name: "memory_consolidate".into(),
            description: "Run a memory consolidation cycle (decay, promote, dedup)".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" }
                }
            }),
        },
    ]
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("engram=info")
        .init();

    let cli = Cli::parse();
    match cli {
        Cli::Serve { db, dims } => {
            // Ensure parent directory exists
            if let Some(parent) = std::path::Path::new(&db).parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }

            let db_url = format!("sqlite:{db}?mode=rwc");

            let embedding = Box::new(MockEmbeddingProvider::new(dims));
            let memory = Memory::open(&db_url, embedding)
                .await
                .expect("failed to open memory database");
            let memory = Arc::new(memory);

            eprintln!("engram: MCP server ready (db={db})");

            let defs = tool_defs();
            let m = memory.clone();
            let server = McpServer::new()
                .tool(defs[0].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_add(m, args).await }
                    }
                })
                .tool(defs[1].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_recall(m, args).await }
                    }
                })
                .tool(defs[2].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_context(m, args).await }
                    }
                })
                .tool(defs[3].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_forget(m, args).await }
                    }
                })
                .tool(defs[4].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_search(m, args).await }
                    }
                })
                .tool(defs[5].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_stats(m, args).await }
                    }
                })
                .tool(defs[6].clone(), {
                    let m = m.clone();
                    move |args| {
                        let m = m.clone();
                        async move { handlers::handle_consolidate(m, args).await }
                    }
                });

            if let Err(e) = server.run().await {
                eprintln!("engram: server error: {e}");
                std::process::exit(1);
            }
        }
    }
}

use clap::Parser;
use engram::memory::Memory;
use engram_server::config::{BackendConfig, EmbeddingBackend, LlmBackend};
use engram_server::handlers;
use engram_server::mcp::{McpServer, McpToolDef};
use engram_server::rest::{self, AppState};
use serde_json::json;
use std::sync::Arc;

/// Supported LLM providers.
///
/// `openai-compatible` is the umbrella for OpenAI itself, Azure OpenAI, Groq,
/// Together, Mistral, DeepSeek, Perplexity, OpenRouter, Fireworks, vLLM,
/// LM Studio, LocalAI, and Ollama's `/v1` compat layer — any endpoint that
/// speaks OpenAI's chat-completions protocol. Switch providers by changing
/// the `--openai-base-url` alone. `openai` is accepted as an alias for
/// backwards compatibility.
///
/// `command` runs a user-supplied shell command per extraction call — see
/// `engram::llm_command` for the stdin/stdout contract.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum LlmProvider {
    Ollama,
    #[value(name = "openai-compatible", alias = "openai")]
    OpenAiCompatible,
    Anthropic,
    Google,
    Command,
    Mock,
}

/// Supported embedding providers.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum EmbeddingProvider {
    Ollama,
    Mock,
}

#[derive(Parser)]
#[command(name = "engram", about = "Engram — memory layer for AI agents")]
enum Cli {
    /// Start the server (MCP stdio or REST HTTP)
    Serve {
        /// Path to SQLite database file
        #[arg(long, env = "ENGRAM_DB_PATH", default_value = "engram.db")]
        db: String,

        /// Server mode: `mcp` (stdio, default) or `rest` (HTTP)
        #[arg(long, env = "ENGRAM_MODE", default_value = "mcp")]
        mode: String,

        /// HTTP port for REST mode
        #[arg(long, env = "ENGRAM_PORT", default_value = "9090")]
        port: u16,

        // ── LLM provider ──────────────────────────────────────────────
        /// LLM provider used for fact extraction
        #[arg(
            long,
            value_enum,
            env = "ENGRAM_LLM_PROVIDER",
            default_value = "ollama"
        )]
        llm_provider: LlmProvider,

        // Ollama LLM
        #[arg(
            long,
            env = "ENGRAM_OLLAMA_URL",
            default_value = "http://localhost:11434"
        )]
        ollama_url: String,
        #[arg(long, env = "ENGRAM_OLLAMA_LLM_MODEL", default_value = "llama3.2")]
        ollama_llm_model: String,

        // OpenAI
        #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
        openai_api_key: Option<String>,
        #[arg(
            long,
            env = "ENGRAM_OPENAI_BASE_URL",
            default_value = "https://api.openai.com/v1"
        )]
        openai_base_url: String,
        #[arg(long, env = "ENGRAM_OPENAI_MODEL", default_value = "gpt-4o-mini")]
        openai_model: String,

        // Anthropic
        #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
        anthropic_api_key: Option<String>,
        #[arg(
            long,
            env = "ENGRAM_ANTHROPIC_BASE_URL",
            default_value = "https://api.anthropic.com"
        )]
        anthropic_base_url: String,
        #[arg(
            long,
            env = "ENGRAM_ANTHROPIC_MODEL",
            default_value = "claude-haiku-4-5-20251001"
        )]
        anthropic_model: String,

        // Google
        #[arg(long, env = "GOOGLE_API_KEY", hide_env_values = true)]
        google_api_key: Option<String>,
        #[arg(
            long,
            env = "ENGRAM_GOOGLE_BASE_URL",
            default_value = "https://generativelanguage.googleapis.com/v1beta"
        )]
        google_base_url: String,
        #[arg(
            long,
            env = "ENGRAM_GOOGLE_MODEL",
            default_value = "gemini-flash-latest"
        )]
        google_model: String,

        // Command (shell-out extensibility)
        /// Shell command for the `command` LLM provider. Runs via `sh -c`.
        /// Reads a JSON request from stdin and writes a JSON response to
        /// stdout. See engram::llm_command for the full contract.
        #[arg(long, env = "ENGRAM_LLM_COMMAND")]
        llm_command: Option<String>,
        /// Timeout in seconds for each `command` provider invocation.
        #[arg(long, env = "ENGRAM_LLM_COMMAND_TIMEOUT", default_value = "120")]
        llm_command_timeout: u64,

        // ── Embedding provider ────────────────────────────────────────
        /// Embedding provider used for vector search
        #[arg(
            long,
            value_enum,
            env = "ENGRAM_EMBEDDING_PROVIDER",
            default_value = "ollama"
        )]
        embedding_provider: EmbeddingProvider,

        #[arg(
            long,
            env = "ENGRAM_EMBEDDING_MODEL",
            default_value = "nomic-embed-text"
        )]
        embedding_model: String,

        /// Embedding dimensions. Must match the embedding model's output.
        /// `nomic-embed-text` is 768. Mock backend accepts any value.
        #[arg(long, env = "ENGRAM_EMBEDDING_DIMS", default_value = "768")]
        embedding_dims: usize,

        /// Enable fact extraction when saving chat messages via the message store.
        /// When true, POST /v1/memory/messages and the messages_save MCP tool
        /// will also run the LLM extraction pipeline to extract facts.
        #[arg(long, env = "ENGRAM_EXTRACT_ON_SAVE", default_value = "true")]
        extract_on_save: bool,
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
        McpToolDef {
            name: "messages_save".into(),
            description: "Save chat messages to a conversation, optionally extracting facts".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string", "description": "Conversation identifier" },
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string" },
                                "content": { "type": "string" },
                                "metadata": { "type": "object" }
                            },
                            "required": ["role", "content"]
                        },
                        "description": "Chat messages to save"
                    },
                    "user_id": { "type": "string", "description": "User identifier" },
                    "org_id": { "type": "string", "description": "Organization identifier" }
                },
                "required": ["conversation_id", "messages"]
            }),
        },
        McpToolDef {
            name: "messages_get".into(),
            description: "Retrieve chat messages from a conversation".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string", "description": "Conversation identifier" },
                    "last_n": { "type": "integer", "description": "Only return the last N messages" },
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" }
                },
                "required": ["conversation_id"]
            }),
        },
        McpToolDef {
            name: "messages_list".into(),
            description: "List all conversation IDs visible to the given scope".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" }
                }
            }),
        },
        McpToolDef {
            name: "messages_delete".into(),
            description: "Delete all messages in a conversation".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string", "description": "Conversation identifier" },
                    "user_id": { "type": "string" },
                    "org_id": { "type": "string" }
                },
                "required": ["conversation_id"]
            }),
        },
    ]
}

#[allow(clippy::too_many_arguments)]
fn build_backend_config(
    llm_provider: LlmProvider,
    ollama_url: String,
    ollama_llm_model: String,
    openai_api_key: Option<String>,
    openai_base_url: String,
    openai_model: String,
    anthropic_api_key: Option<String>,
    anthropic_base_url: String,
    anthropic_model: String,
    google_api_key: Option<String>,
    google_base_url: String,
    google_model: String,
    llm_command: Option<String>,
    llm_command_timeout: u64,
    embedding_provider: EmbeddingProvider,
    embedding_model: String,
    embedding_dims: usize,
) -> Result<BackendConfig, String> {
    let llm = match llm_provider {
        LlmProvider::Mock => LlmBackend::Mock,
        LlmProvider::Ollama => LlmBackend::Ollama {
            base_url: ollama_url.clone(),
            model: ollama_llm_model,
        },
        LlmProvider::OpenAiCompatible => LlmBackend::OpenAiCompatible {
            base_url: openai_base_url,
            api_key: openai_api_key
                .ok_or("openai-compatible LLM provider selected but OPENAI_API_KEY is not set")?,
            model: openai_model,
        },
        LlmProvider::Anthropic => LlmBackend::Anthropic {
            base_url: anthropic_base_url,
            api_key: anthropic_api_key
                .ok_or("Anthropic LLM provider selected but ANTHROPIC_API_KEY is not set")?,
            model: anthropic_model,
        },
        LlmProvider::Google => LlmBackend::Google {
            base_url: google_base_url,
            api_key: google_api_key
                .ok_or("Google LLM provider selected but GOOGLE_API_KEY is not set")?,
            model: google_model,
        },
        LlmProvider::Command => LlmBackend::Command {
            command: llm_command.ok_or(
                "command LLM provider selected but --llm-command / ENGRAM_LLM_COMMAND is not set",
            )?,
            timeout_secs: llm_command_timeout,
        },
    };

    let embedding = match embedding_provider {
        EmbeddingProvider::Mock => EmbeddingBackend::Mock {
            dims: embedding_dims,
        },
        EmbeddingProvider::Ollama => EmbeddingBackend::Ollama {
            base_url: ollama_url,
            model: embedding_model,
            dims: embedding_dims,
        },
    };

    Ok(BackendConfig { llm, embedding })
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("engram=info")
        .init();

    let cli = Cli::parse();
    match cli {
        Cli::Serve {
            db,
            mode,
            port,
            llm_provider,
            ollama_url,
            ollama_llm_model,
            openai_api_key,
            openai_base_url,
            openai_model,
            anthropic_api_key,
            anthropic_base_url,
            anthropic_model,
            google_api_key,
            google_base_url,
            google_model,
            llm_command,
            llm_command_timeout,
            embedding_provider,
            embedding_model,
            embedding_dims,
            extract_on_save,
        } => {
            // Ensure parent directory exists
            if let Some(parent) = std::path::Path::new(&db).parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }

            let config = match build_backend_config(
                llm_provider,
                ollama_url,
                ollama_llm_model,
                openai_api_key,
                openai_base_url,
                openai_model,
                anthropic_api_key,
                anthropic_base_url,
                anthropic_model,
                google_api_key,
                google_base_url,
                google_model,
                llm_command,
                llm_command_timeout,
                embedding_provider,
                embedding_model,
                embedding_dims,
            ) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("engram: configuration error — {e}");
                    std::process::exit(2);
                }
            };

            eprintln!("engram: db = {db}");
            eprintln!("engram: llm = {}", config.llm.describe());
            eprintln!("engram: embedding = {}", config.embedding.describe());
            if config.is_mock() {
                eprintln!(
                    "engram: WARNING — mock backend in use; memory_add will return zero facts."
                );
                eprintln!(
                    "engram: WARNING — set ENGRAM_LLM_PROVIDER=ollama|openai|anthropic|google for real extraction."
                );
            }
            eprintln!("engram: extract_on_save = {extract_on_save}");

            let db_url = format!("sqlite:{db}?mode=rwc");

            let memory = Memory::open(&db_url, config.embedding.build())
                .await
                .expect("failed to open memory database");
            let memory = Arc::new(memory);

            match mode.as_str() {
                "rest" => {
                    let state = AppState {
                        memory: memory.clone(),
                        llm_backend: config.llm.clone(),
                        extract_on_save,
                    };
                    let app = rest::build_router(state);
                    let addr = format!("0.0.0.0:{port}");
                    eprintln!("engram: REST server listening on {addr}");
                    let listener = tokio::net::TcpListener::bind(&addr)
                        .await
                        .expect("failed to bind");
                    axum::serve(listener, app).await.expect("server error");
                }
                _ => {
                    eprintln!("engram: MCP server ready");

                    let defs = tool_defs();
                    let m = memory.clone();
                    let llm_backend = config.llm.clone();
                    let server = McpServer::new()
                        .tool(defs[0].clone(), {
                            let m = m.clone();
                            let lb = llm_backend.clone();
                            move |args| {
                                let m = m.clone();
                                let lb = lb.clone();
                                async move { handlers::handle_add(m, lb, args).await }
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
                        })
                        .tool(defs[7].clone(), {
                            let m = m.clone();
                            let lb = llm_backend.clone();
                            let eos = extract_on_save;
                            move |args| {
                                let m = m.clone();
                                let lb = lb.clone();
                                async move {
                                    handlers::handle_messages_save(m, lb, eos, args).await
                                }
                            }
                        })
                        .tool(defs[8].clone(), {
                            let m = m.clone();
                            move |args| {
                                let m = m.clone();
                                async move { handlers::handle_messages_get(m, args).await }
                            }
                        })
                        .tool(defs[9].clone(), {
                            let m = m.clone();
                            move |args| {
                                let m = m.clone();
                                async move { handlers::handle_messages_list(m, args).await }
                            }
                        })
                        .tool(defs[10].clone(), {
                            let m = m.clone();
                            move |args| {
                                let m = m.clone();
                                async move { handlers::handle_messages_delete(m, args).await }
                            }
                        });

                    if let Err(e) = server.run().await {
                        eprintln!("engram: server error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

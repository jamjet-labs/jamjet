//! Server backend configuration — chooses LLM and embedding providers at startup.
//!
//! The Engram library ships with multiple LLM backends (`MockLlmClient`,
//! `OllamaLlmClient`, `OpenAiLlmClient`, `AnthropicLlmClient`, `GoogleLlmClient`)
//! and two embedding backends (`MockEmbeddingProvider`, `OllamaEmbeddingProvider`).
//! This module lets the server binary pick between them from CLI flags or
//! environment variables, so one Docker image can run in any mode without a
//! rebuild.
//!
//! Defaults favour real providers. `--llm-provider ollama` (the default) uses
//! Ollama at `http://localhost:11434` with `llama3.2` for completions.
//! Pick `openai`, `anthropic`, or `google` and supply an API key to route
//! extraction through a hosted model.

use engram::embedding::{EmbeddingProvider, MockEmbeddingProvider};
use engram::embedding_ollama::OllamaEmbeddingProvider;
use engram::llm::{LlmClient, MockLlmClient};
use engram::llm_anthropic::AnthropicLlmClient;
use engram::llm_command::CommandLlmClient;
use engram::llm_google::GoogleLlmClient;
use engram::llm_ollama::OllamaLlmClient;
use engram::llm_openai::OpenAiLlmClient;

/// Which LLM backend the server uses for fact extraction.
#[derive(Clone, Debug)]
pub enum LlmBackend {
    /// Deterministic mock that always returns `{"facts": []}`.
    /// Use only for tests — `memory_add` will produce zero facts.
    Mock,
    /// Ollama-backed chat-completion client (local, free) — uses Ollama's
    /// native `/api/chat`.
    Ollama { base_url: String, model: String },
    /// Any OpenAI chat-completions-compatible endpoint — OpenAI itself,
    /// Azure OpenAI, Groq, Together, Mistral, DeepSeek, Perplexity, OpenRouter,
    /// Fireworks, vLLM, LM Studio, LocalAI, or Ollama's `/v1` compat layer.
    /// Switch providers by changing `base_url` alone.
    OpenAiCompatible {
        base_url: String,
        api_key: String,
        model: String,
    },
    /// Anthropic Claude via the Messages API.
    Anthropic {
        base_url: String,
        api_key: String,
        model: String,
    },
    /// Google Gemini via the `generateContent` API.
    Google {
        base_url: String,
        api_key: String,
        model: String,
    },
    /// Shell-out extensibility escape hatch. Runs a user-supplied command
    /// per extraction call, writes a JSON request to stdin, reads JSON from
    /// stdout. See `engram::llm_command` for the stdin/stdout contract.
    Command { command: String, timeout_secs: u64 },
}

impl LlmBackend {
    /// Build a fresh boxed LLM client. `Memory::add_messages` takes
    /// `Box<dyn LlmClient>` by value, so each extraction call needs its own.
    pub fn build(&self) -> Box<dyn LlmClient> {
        match self {
            Self::Mock => Box::new(MockLlmClient::new(vec![serde_json::json!({"facts": []})])),
            Self::Ollama { base_url, model } => Box::new(OllamaLlmClient::with_config(
                base_url.clone(),
                model.clone(),
            )),
            Self::OpenAiCompatible {
                base_url,
                api_key,
                model,
            } => Box::new(OpenAiLlmClient::with_config(
                base_url.clone(),
                api_key.clone(),
                model.clone(),
            )),
            Self::Anthropic {
                base_url,
                api_key,
                model,
            } => Box::new(AnthropicLlmClient::with_config(
                base_url.clone(),
                api_key.clone(),
                model.clone(),
            )),
            Self::Google {
                base_url,
                api_key,
                model,
            } => Box::new(GoogleLlmClient::with_config(
                base_url.clone(),
                api_key.clone(),
                model.clone(),
            )),
            Self::Command {
                command,
                timeout_secs,
            } => Box::new(CommandLlmClient::new(command.clone()).with_timeout(*timeout_secs)),
        }
    }

    /// Human-readable description for startup logs. Never includes API keys
    /// or command contents longer than 60 characters.
    pub fn describe(&self) -> String {
        match self {
            Self::Mock => "mock (returns empty facts)".to_string(),
            Self::Ollama { base_url, model } => format!("ollama {model} at {base_url}"),
            Self::OpenAiCompatible {
                base_url, model, ..
            } => format!("openai-compatible {model} at {base_url}"),
            Self::Anthropic {
                base_url, model, ..
            } => format!("anthropic {model} at {base_url}"),
            Self::Google {
                base_url, model, ..
            } => format!("google {model} at {base_url}"),
            Self::Command {
                command,
                timeout_secs,
            } => {
                let shown: String = command.chars().take(60).collect();
                format!("command `{shown}` (timeout={timeout_secs}s)")
            }
        }
    }
}

/// Which embedding backend the server uses for vector search.
#[derive(Clone, Debug)]
pub enum EmbeddingBackend {
    /// Deterministic byte-cycled mock. Not semantically meaningful.
    Mock { dims: usize },
    /// Ollama-backed embedding provider.
    Ollama {
        base_url: String,
        model: String,
        dims: usize,
    },
}

impl EmbeddingBackend {
    /// Build the embedding provider used by `Memory::open`.
    pub fn build(&self) -> Box<dyn EmbeddingProvider> {
        match self {
            Self::Mock { dims } => Box::new(MockEmbeddingProvider::new(*dims)),
            Self::Ollama {
                base_url,
                model,
                dims,
            } => Box::new(OllamaEmbeddingProvider::with_config(base_url, model, *dims)),
        }
    }

    pub fn dimensions(&self) -> usize {
        match self {
            Self::Mock { dims } => *dims,
            Self::Ollama { dims, .. } => *dims,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Mock { dims } => format!("mock ({dims}d)"),
            Self::Ollama {
                base_url,
                model,
                dims,
            } => format!("ollama {model} at {base_url} ({dims}d)"),
        }
    }
}

/// Full backend configuration. Parsed once at startup and cloned into the
/// MCP handler closures and REST `AppState`.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    pub llm: LlmBackend,
    pub embedding: EmbeddingBackend,
}

impl BackendConfig {
    /// Convenience for tests: both backends mocked at 64 dimensions.
    pub fn mock() -> Self {
        Self {
            llm: LlmBackend::Mock,
            embedding: EmbeddingBackend::Mock { dims: 64 },
        }
    }

    /// True if either backend is a mock. Used to print a startup warning.
    pub fn is_mock(&self) -> bool {
        matches!(self.llm, LlmBackend::Mock)
            || matches!(self.embedding, EmbeddingBackend::Mock { .. })
    }
}

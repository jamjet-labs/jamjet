//! MCP tool handlers — one function per tool, each calling Memory methods.

use crate::config::LlmBackend;
use engram::context::{ContextConfig, OutputFormat};
use engram::extract::{ExtractionConfig, Message};
use engram::memory::{Memory, RecallQuery};
use engram::message::ChatMessage;
use engram::scope::Scope;
use serde_json::Value;
use std::sync::Arc;

fn parse_scope(args: &Value) -> Scope {
    let org_id = args["org_id"].as_str().unwrap_or("default");
    match args["user_id"].as_str() {
        Some(user_id) => match args["session_id"].as_str() {
            Some(session_id) => Scope::session(org_id, user_id, session_id),
            None => Scope::user(org_id, user_id),
        },
        None => Scope::org(org_id),
    }
}

/// memory_add — extract facts from conversation messages.
///
/// The LLM used for extraction is chosen by the server's `LlmBackend`
/// (configured at startup via CLI flags or env vars — see `config.rs`).
/// A fresh boxed client is built per call because `Memory::add_messages`
/// takes ownership of `Box<dyn LlmClient>`.
pub async fn handle_add(
    memory: Arc<Memory>,
    llm_backend: LlmBackend,
    args: Value,
) -> Result<String, String> {
    let messages: Vec<Message> = args["messages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| {
            let role = m["role"].as_str()?;
            let content = m["content"].as_str()?;
            Some(Message {
                role: role.to_string(),
                content: content.to_string(),
            })
        })
        .collect();

    if messages.is_empty() {
        return Err("messages array is required and must not be empty".to_string());
    }

    let scope = parse_scope(&args);

    let ids = memory
        .add_messages(
            &messages,
            scope,
            llm_backend.build(),
            ExtractionConfig::default(),
        )
        .await
        .map_err(|e| format!("add_messages failed: {e}"))?;

    let result = serde_json::json!({
        "success": true,
        "fact_count": ids.len(),
        "fact_ids": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_recall — semantic search over stored facts.
pub async fn handle_recall(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let query = args["query"]
        .as_str()
        .ok_or("query is required")?
        .to_string();

    let scope = parse_scope(&args);
    let max_results = args["max_results"].as_u64().unwrap_or(10) as usize;

    let recall_query = RecallQuery {
        query,
        scope: Some(scope),
        max_results,
        as_of: None,
        min_score: None,
    };

    let facts = memory
        .recall(&recall_query)
        .await
        .map_err(|e| format!("recall failed: {e}"))?;

    let results: Vec<Value> = facts
        .iter()
        .map(|f| {
            serde_json::json!({
                "fact_id": f.id.to_string(),
                "text": f.text,
                "tier": f.tier,
                "category": f.category,
                "confidence": f.confidence,
            })
        })
        .collect();

    let result = serde_json::json!({
        "results": results,
        "total": results.len(),
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_context — assemble a token-budgeted context block.
pub async fn handle_context(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let query = args["query"]
        .as_str()
        .ok_or("query is required")?
        .to_string();

    let scope = parse_scope(&args);
    let token_budget = args["token_budget"].as_u64().unwrap_or(2000) as usize;

    let format = match args["format"].as_str().unwrap_or("system_prompt") {
        "markdown" => OutputFormat::Markdown,
        "raw" => OutputFormat::Raw,
        _ => OutputFormat::SystemPrompt,
    };

    let config = ContextConfig {
        token_budget,
        format,
        ..Default::default()
    };

    let block = memory
        .context(&query, &scope, config)
        .await
        .map_err(|e| format!("context failed: {e}"))?;

    let result = serde_json::json!({
        "text": block.text,
        "token_count": block.token_count,
        "facts_included": block.facts_included,
        "facts_omitted": block.facts_omitted,
        "tier_breakdown": block.tier_breakdown,
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_forget — soft-delete a fact by ID.
pub async fn handle_forget(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let fact_id_str = args["fact_id"].as_str().ok_or("fact_id is required")?;
    let fact_id =
        uuid::Uuid::parse_str(fact_id_str).map_err(|e| format!("invalid fact_id: {e}"))?;
    let reason = args["reason"].as_str().map(String::from);

    memory
        .forget(fact_id, reason.as_deref())
        .await
        .map_err(|e| format!("forget failed: {e}"))?;

    let result = serde_json::json!({
        "success": true,
        "deleted_fact_id": fact_id_str,
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_search — keyword search over facts.
pub async fn handle_search(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let query = args["query"].as_str().ok_or("query is required")?;
    let scope = parse_scope(&args);
    let top_k = args["top_k"].as_u64().unwrap_or(10) as usize;

    let facts = memory
        .fact_store()
        .keyword_search(query, &scope, top_k)
        .await
        .map_err(|e| format!("search failed: {e}"))?;

    let results: Vec<Value> = facts
        .iter()
        .map(|f| {
            serde_json::json!({
                "fact_id": f.id.to_string(),
                "text": f.text,
                "tier": f.tier,
                "category": f.category,
            })
        })
        .collect();

    let result = serde_json::json!({
        "results": results,
        "total": results.len(),
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_stats — return aggregate statistics.
pub async fn handle_stats(memory: Arc<Memory>, _args: Value) -> Result<String, String> {
    let stats = memory
        .stats(None)
        .await
        .map_err(|e| format!("stats failed: {e}"))?;

    let result = serde_json::json!({
        "total_facts": stats.total_facts,
        "valid_facts": stats.valid_facts,
        "invalidated_facts": stats.invalidated_facts,
        "total_entities": stats.total_entities,
        "total_relationships": stats.total_relationships,
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// memory_consolidate — run a consolidation cycle.
pub async fn handle_consolidate(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let scope = parse_scope(&args);

    let config = engram::consolidation::ConsolidationConfig::default();
    let result = memory
        .consolidate(&scope, None, config)
        .await
        .map_err(|e| format!("consolidate failed: {e}"))?;

    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// messages_save — save chat messages to a conversation, optionally extracting facts.
pub async fn handle_messages_save(
    memory: Arc<Memory>,
    llm_backend: LlmBackend,
    extract_on_save: bool,
    args: Value,
) -> Result<String, String> {
    let conversation_id = args["conversation_id"]
        .as_str()
        .ok_or("conversation_id is required")?
        .to_string();

    let raw_messages = args["messages"]
        .as_array()
        .ok_or("messages array is required")?;

    if raw_messages.is_empty() {
        return Err("messages must not be empty".to_string());
    }

    let scope = parse_scope(&args);

    let chat_messages: Vec<ChatMessage> = raw_messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let role = m["role"].as_str()?;
            let content = m["content"].as_str()?;
            let mut msg =
                ChatMessage::new(&conversation_id, role, content, scope.clone(), i as i32);
            if let Some(meta) = m["metadata"].as_object() {
                msg.metadata = meta.clone();
            }
            Some(msg)
        })
        .collect();

    if chat_messages.is_empty() {
        return Err("messages must contain valid {role, content} objects".to_string());
    }

    let message_ids = memory
        .save_chat_messages(&conversation_id, &chat_messages, &scope)
        .await
        .map_err(|e| format!("save_chat_messages failed: {e}"))?;

    let message_id_strs: Vec<String> = message_ids.iter().map(|id| id.to_string()).collect();

    let fact_ids = if extract_on_save {
        let extract_messages: Vec<Message> = raw_messages
            .iter()
            .filter_map(|m| {
                let role = m["role"].as_str()?;
                let content = m["content"].as_str()?;
                Some(Message {
                    role: role.to_string(),
                    content: content.to_string(),
                })
            })
            .collect();

        match memory
            .add_messages(
                &extract_messages,
                scope,
                llm_backend.build(),
                ExtractionConfig::default(),
            )
            .await
        {
            Ok(ids) => Some(ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()),
            Err(e) => {
                tracing::warn!("fact extraction failed (messages saved): {e}");
                None
            }
        }
    } else {
        None
    };

    let result = serde_json::json!({
        "success": true,
        "message_ids": message_id_strs,
        "fact_ids": fact_ids,
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// messages_get — retrieve messages from a conversation.
pub async fn handle_messages_get(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let conversation_id = args["conversation_id"]
        .as_str()
        .ok_or("conversation_id is required")?
        .to_string();

    let scope = parse_scope(&args);
    let last_n = args["last_n"].as_u64().map(|n| n as usize);

    let messages = memory
        .get_chat_messages(&conversation_id, last_n, &scope)
        .await
        .map_err(|e| format!("get_chat_messages failed: {e}"))?;

    let results: Vec<Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id.to_string(),
                "conversation_id": m.conversation_id,
                "role": m.role,
                "content": m.content,
                "seq": m.seq,
                "created_at": m.created_at.to_rfc3339(),
                "metadata": m.metadata,
            })
        })
        .collect();

    let result = serde_json::json!({
        "messages": results,
        "total": results.len(),
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// messages_list — list all conversation IDs visible to the given scope.
pub async fn handle_messages_list(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let scope = parse_scope(&args);

    let ids = memory
        .list_conversations(&scope)
        .await
        .map_err(|e| format!("list_conversations failed: {e}"))?;

    let result = serde_json::json!({
        "conversation_ids": ids,
        "total": ids.len(),
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

/// messages_delete — delete all messages in a conversation.
pub async fn handle_messages_delete(memory: Arc<Memory>, args: Value) -> Result<String, String> {
    let conversation_id = args["conversation_id"]
        .as_str()
        .ok_or("conversation_id is required")?
        .to_string();

    let scope = parse_scope(&args);

    let count = memory
        .delete_chat_messages(&conversation_id, &scope)
        .await
        .map_err(|e| format!("delete_chat_messages failed: {e}"))?;

    let result = serde_json::json!({
        "success": true,
        "deleted_count": count,
    });
    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())
}

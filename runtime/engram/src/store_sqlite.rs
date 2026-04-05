//! SQLite-backed `FactStore` implementation.
//!
//! All `DateTime<Utc>` values are stored as RFC 3339 strings.
//! UUIDs are stored as TEXT. `entity_refs` is a JSON array of UUID strings.
//! `metadata` is a JSON object string (or `"null"` when empty).

use crate::fact::{Fact, FactFilter, FactId, FactPatch, MemoryTier};
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError, StoreStats};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

pub const FACT_STORE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS facts (
    id              TEXT PRIMARY KEY,
    text            TEXT NOT NULL,
    org_id          TEXT NOT NULL DEFAULT 'default',
    agent_id        TEXT,
    user_id         TEXT,
    session_id      TEXT,
    tier            TEXT NOT NULL DEFAULT 'conversation',
    category        TEXT,
    source          TEXT,
    confidence      REAL,
    valid_from      TEXT NOT NULL,
    invalid_at      TEXT,
    created_at      TEXT NOT NULL,
    entity_refs     TEXT NOT NULL DEFAULT '[]',
    supersedes      TEXT,
    superseded_by   TEXT,
    access_count    INTEGER NOT NULL DEFAULT 0,
    last_accessed   TEXT,
    metadata        TEXT NOT NULL DEFAULT 'null'
);
CREATE INDEX IF NOT EXISTS idx_facts_org_id     ON facts (org_id);
CREATE INDEX IF NOT EXISTS idx_facts_user_id    ON facts (user_id);
CREATE INDEX IF NOT EXISTS idx_facts_agent_id   ON facts (agent_id);
CREATE INDEX IF NOT EXISTS idx_facts_session_id ON facts (session_id);
CREATE INDEX IF NOT EXISTS idx_facts_tier       ON facts (tier);
CREATE INDEX IF NOT EXISTS idx_facts_category   ON facts (category);
CREATE INDEX IF NOT EXISTS idx_facts_valid_from ON facts (valid_from);
CREATE INDEX IF NOT EXISTS idx_facts_invalid_at ON facts (invalid_at);
"#;

// ---------------------------------------------------------------------------
// SqliteFactStore
// ---------------------------------------------------------------------------

pub struct SqliteFactStore {
    pool: SqlitePool,
}

impl SqliteFactStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a connection pool from a database URL and return a store.
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Apply the DDL. Safe to call multiple times (uses `IF NOT EXISTS`).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        for stmt in FACT_STORE_DDL.split(';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal row type
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct FactRow {
    id: String,
    text: String,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    tier: String,
    category: Option<String>,
    source: Option<String>,
    confidence: Option<f64>,
    valid_from: String,
    invalid_at: Option<String>,
    created_at: String,
    entity_refs: String,
    supersedes: Option<String>,
    superseded_by: Option<String>,
    access_count: i64,
    last_accessed: Option<String>,
    metadata: String,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn parse_dt(s: &str) -> Result<DateTime<Utc>, MemoryError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| MemoryError::Serialization(e.to_string()))
}

fn parse_opt_dt(s: &Option<String>) -> Result<Option<DateTime<Utc>>, MemoryError> {
    match s {
        None => Ok(None),
        Some(s) => parse_dt(s).map(Some),
    }
}

fn tier_from_str(s: &str) -> MemoryTier {
    match s {
        "working" => MemoryTier::Working,
        "knowledge" => MemoryTier::Knowledge,
        _ => MemoryTier::Conversation,
    }
}

fn tier_to_str(t: &MemoryTier) -> &'static str {
    match t {
        MemoryTier::Working => "working",
        MemoryTier::Conversation => "conversation",
        MemoryTier::Knowledge => "knowledge",
    }
}

fn row_to_fact(row: FactRow) -> Result<Fact, MemoryError> {
    let id = Uuid::parse_str(&row.id).map_err(|e| MemoryError::Serialization(e.to_string()))?;

    let entity_refs: Vec<Uuid> = {
        let strings: Vec<String> = serde_json::from_str(&row.entity_refs)
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;
        strings
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| MemoryError::Serialization(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?
    };

    let metadata: serde_json::Map<String, serde_json::Value> =
        if row.metadata == "null" || row.metadata.is_empty() {
            serde_json::Map::new()
        } else {
            serde_json::from_str(&row.metadata)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

    let supersedes = row
        .supersedes
        .as_deref()
        .map(|s| Uuid::parse_str(s).map_err(|e| MemoryError::Serialization(e.to_string())))
        .transpose()?;

    let superseded_by = row
        .superseded_by
        .as_deref()
        .map(|s| Uuid::parse_str(s).map_err(|e| MemoryError::Serialization(e.to_string())))
        .transpose()?;

    Ok(Fact {
        id,
        text: row.text,
        scope: Scope {
            org_id: row.org_id,
            agent_id: row.agent_id,
            user_id: row.user_id,
            session_id: row.session_id,
        },
        tier: tier_from_str(&row.tier),
        category: row.category,
        source: row.source,
        confidence: row.confidence.map(|c| c as f32),
        valid_from: parse_dt(&row.valid_from)?,
        invalid_at: parse_opt_dt(&row.invalid_at)?,
        created_at: parse_dt(&row.created_at)?,
        // embeddings are not persisted in the facts table
        embedding: Vec::new(),
        entity_refs,
        supersedes,
        superseded_by,
        access_count: row.access_count as u64,
        last_accessed: parse_opt_dt(&row.last_accessed)?,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// FactStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl FactStore for SqliteFactStore {
    async fn insert_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
        let entity_refs_json = {
            let strs: Vec<String> = fact.entity_refs.iter().map(|u| u.to_string()).collect();
            serde_json::to_string(&strs).map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        let metadata_json = if fact.metadata.is_empty() {
            "null".to_string()
        } else {
            serde_json::to_string(&fact.metadata)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO facts
                (id, text, org_id, agent_id, user_id, session_id,
                 tier, category, source, confidence,
                 valid_from, invalid_at, created_at,
                 entity_refs, supersedes, superseded_by,
                 access_count, last_accessed, metadata)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(fact.id.to_string())
        .bind(&fact.text)
        .bind(&fact.scope.org_id)
        .bind(fact.scope.agent_id.as_deref())
        .bind(fact.scope.user_id.as_deref())
        .bind(fact.scope.session_id.as_deref())
        .bind(tier_to_str(&fact.tier))
        .bind(fact.category.as_deref())
        .bind(fact.source.as_deref())
        .bind(fact.confidence.map(|c| c as f64))
        .bind(fact.valid_from.to_rfc3339())
        .bind(fact.invalid_at.map(|dt| dt.to_rfc3339()))
        .bind(fact.created_at.to_rfc3339())
        .bind(entity_refs_json)
        .bind(fact.supersedes.map(|u| u.to_string()))
        .bind(fact.superseded_by.map(|u| u.to_string()))
        .bind(fact.access_count as i64)
        .bind(fact.last_accessed.map(|dt| dt.to_rfc3339()))
        .bind(metadata_json)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(fact.id)
    }

    async fn get_fact(&self, id: FactId) -> Result<Fact, MemoryError> {
        let row = sqlx::query_as::<_, FactRow>("SELECT * FROM facts WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;

        row_to_fact(row)
    }

    async fn update_fact(&self, id: FactId, patch: FactPatch) -> Result<Fact, MemoryError> {
        // Build a (column = ?, value_string) list for each non-None patch field.
        // We bind all values as strings; SQLite will coerce them.
        let mut cols: Vec<&'static str> = Vec::new();
        let mut vals: Vec<String> = Vec::new();

        if let Some(ref text) = patch.text {
            cols.push("text = ?");
            vals.push(text.clone());
        }
        if let Some(ref tier) = patch.tier {
            cols.push("tier = ?");
            vals.push(tier_to_str(tier).to_string());
        }
        if let Some(ref category) = patch.category {
            cols.push("category = ?");
            vals.push(category.clone());
        }
        if let Some(ref source) = patch.source {
            cols.push("source = ?");
            vals.push(source.clone());
        }
        if let Some(confidence) = patch.confidence {
            cols.push("confidence = ?");
            vals.push((confidence as f64).to_string());
        }
        if let Some(invalid_at) = patch.invalid_at {
            cols.push("invalid_at = ?");
            vals.push(invalid_at.to_rfc3339());
        }
        if let Some(superseded_by) = patch.superseded_by {
            cols.push("superseded_by = ?");
            vals.push(superseded_by.to_string());
        }
        if !patch.metadata.is_empty() {
            let json = serde_json::to_string(&patch.metadata)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?;
            cols.push("metadata = ?");
            vals.push(json);
        }

        if !cols.is_empty() {
            let sql = format!("UPDATE facts SET {} WHERE id = ?", cols.join(", "));
            let mut q = sqlx::query(&sql);
            for v in &vals {
                q = q.bind(v.as_str());
            }
            q = q.bind(id.to_string());
            q.execute(&self.pool)
                .await
                .map_err(|e| MemoryError::Database(e.to_string()))?;
        }

        self.get_fact(id).await
    }

    async fn list_facts(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError> {
        let mut wheres: Vec<String> = vec!["1=1".to_string()];

        if let Some(ref scope) = filter.scope {
            wheres.push(format!("org_id = '{}'", scope.org_id.replace('\'', "''")));
            if let Some(ref user_id) = scope.user_id {
                wheres.push(format!("user_id = '{}'", user_id.replace('\'', "''")));
            }
            if let Some(ref agent_id) = scope.agent_id {
                wheres.push(format!("agent_id = '{}'", agent_id.replace('\'', "''")));
            }
            if let Some(ref session_id) = scope.session_id {
                wheres.push(format!("session_id = '{}'", session_id.replace('\'', "''")));
            }
        }

        if let Some(ref tier) = filter.tier {
            wheres.push(format!("tier = '{}'", tier_to_str(tier)));
        }

        if let Some(ref category) = filter.category {
            wheres.push(format!("category = '{}'", category.replace('\'', "''")));
        }

        if let Some(as_of) = filter.as_of {
            let s = as_of.to_rfc3339();
            wheres.push(format!("valid_from <= '{s}'"));
            wheres.push(format!("(invalid_at IS NULL OR invalid_at > '{s}')"));
        } else if filter.valid_only {
            wheres.push("invalid_at IS NULL".to_string());
        }

        if let Some(ref text_contains) = filter.text_contains {
            let escaped = text_contains.replace('\'', "''");
            wheres.push(format!("text LIKE '%{escaped}%'"));
        }

        let where_clause = wheres.join(" AND ");
        let sql = format!(
            "SELECT * FROM facts WHERE {where_clause} ORDER BY created_at DESC LIMIT {} OFFSET {}",
            filter.limit, filter.offset
        );

        let rows = sqlx::query_as::<_, FactRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_fact).collect()
    }

    async fn invalidate_fact(&self, id: FactId) -> Result<(), MemoryError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE facts SET invalid_at = ? WHERE id = ?")
            .bind(&now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn delete_scope_data(&self, scope: &Scope) -> Result<u64, MemoryError> {
        let mut wheres = vec![format!("org_id = '{}'", scope.org_id.replace('\'', "''"))];

        if let Some(ref user_id) = scope.user_id {
            wheres.push(format!("user_id = '{}'", user_id.replace('\'', "''")));
        }
        if let Some(ref agent_id) = scope.agent_id {
            wheres.push(format!("agent_id = '{}'", agent_id.replace('\'', "''")));
        }
        if let Some(ref session_id) = scope.session_id {
            wheres.push(format!("session_id = '{}'", session_id.replace('\'', "''")));
        }

        let where_clause = wheres.join(" AND ");
        let sql = format!("DELETE FROM facts WHERE {where_clause}");

        let result = sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(result.rows_affected())
    }

    async fn export(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError> {
        self.list_facts(filter).await
    }

    async fn import(&self, facts: Vec<Fact>) -> Result<u64, MemoryError> {
        let mut imported: u64 = 0;
        for fact in facts {
            let result = sqlx::query(
                r#"
                INSERT OR IGNORE INTO facts
                    (id, text, org_id, agent_id, user_id, session_id,
                     tier, category, source, confidence,
                     valid_from, invalid_at, created_at,
                     entity_refs, supersedes, superseded_by,
                     access_count, last_accessed, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(fact.id.to_string())
            .bind(&fact.text)
            .bind(&fact.scope.org_id)
            .bind(fact.scope.agent_id.as_deref())
            .bind(fact.scope.user_id.as_deref())
            .bind(fact.scope.session_id.as_deref())
            .bind(tier_to_str(&fact.tier))
            .bind(fact.category.as_deref())
            .bind(fact.source.as_deref())
            .bind(fact.confidence.map(|c| c as f64))
            .bind(fact.valid_from.to_rfc3339())
            .bind(fact.invalid_at.map(|dt| dt.to_rfc3339()))
            .bind(fact.created_at.to_rfc3339())
            .bind({
                let strs: Vec<String> = fact.entity_refs.iter().map(|u| u.to_string()).collect();
                serde_json::to_string(&strs)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            })
            .bind(fact.supersedes.map(|u| u.to_string()))
            .bind(fact.superseded_by.map(|u| u.to_string()))
            .bind(fact.access_count as i64)
            .bind(fact.last_accessed.map(|dt| dt.to_rfc3339()))
            .bind(if fact.metadata.is_empty() {
                "null".to_string()
            } else {
                serde_json::to_string(&fact.metadata)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            })
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

            imported += result.rows_affected();
        }
        Ok(imported)
    }

    async fn stats(&self) -> Result<StoreStats, MemoryError> {
        let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM facts")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let (valid,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM facts WHERE invalid_at IS NULL")
                .fetch_one(&self.pool)
                .await
                .map_err(|e| MemoryError::Database(e.to_string()))?;

        let (invalidated,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM facts WHERE invalid_at IS NOT NULL")
                .fetch_one(&self.pool)
                .await
                .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(StoreStats {
            total_facts: total as u64,
            valid_facts: valid as u64,
            invalidated_facts: invalidated as u64,
            total_entities: 0,
            total_relationships: 0,
        })
    }

    async fn record_access(&self, id: FactId) -> Result<(), MemoryError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE facts SET access_count = access_count + 1, last_accessed = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }
}

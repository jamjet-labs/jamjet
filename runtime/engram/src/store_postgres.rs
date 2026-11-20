//! PostgreSQL-backed `FactStore` implementation.
//!
//! Uses native Postgres types: `UUID` for ids, `TIMESTAMPTZ` for timestamps,
//! `JSONB` for metadata and entity_refs, and a generated `tsvector` column
//! for full-text search. Placeholder syntax uses `$1, $2, …`.

use crate::fact::{Fact, FactFilter, FactId, FactPatch, MemoryTier};
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError, StoreStats};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL statements for the Postgres facts table. Each element is a single
/// statement to be executed independently (no splitting on `;` needed).
const PG_FACT_STORE_DDL: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS facts (
        id              UUID PRIMARY KEY,
        text            TEXT NOT NULL,
        org_id          TEXT NOT NULL DEFAULT 'default',
        agent_id        TEXT,
        user_id         TEXT,
        session_id      TEXT,
        tier            TEXT NOT NULL DEFAULT 'conversation',
        category        TEXT,
        source          TEXT,
        confidence      DOUBLE PRECISION,
        valid_from      TIMESTAMPTZ NOT NULL,
        invalid_at      TIMESTAMPTZ,
        created_at      TIMESTAMPTZ NOT NULL,
        entity_refs     JSONB NOT NULL DEFAULT '[]',
        supersedes      UUID,
        superseded_by   UUID,
        access_count    BIGINT NOT NULL DEFAULT 0,
        last_accessed   TIMESTAMPTZ,
        metadata        JSONB NOT NULL DEFAULT 'null',
        search_vector   tsvector GENERATED ALWAYS AS (to_tsvector('english', text)) STORED
    )
    "#,
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_org_id     ON facts (org_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_user_id    ON facts (user_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_agent_id   ON facts (agent_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_session_id ON facts (session_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_tier       ON facts (tier)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_category   ON facts (category)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_valid_from ON facts (valid_from)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_invalid_at ON facts (invalid_at)",
    "CREATE INDEX IF NOT EXISTS idx_pg_facts_fts        ON facts USING GIN (search_vector)",
];

// ---------------------------------------------------------------------------
// PostgresFactStore
// ---------------------------------------------------------------------------

pub struct PostgresFactStore {
    pool: PgPool,
}

impl PostgresFactStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a connection pool from a database URL and return a store.
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Apply the DDL. Safe to call multiple times (uses `IF NOT EXISTS`).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        for stmt in PG_FACT_STORE_DDL {
            sqlx::query(*stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal row type
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct FactRow {
    id: Uuid,
    text: String,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    tier: String,
    category: Option<String>,
    source: Option<String>,
    confidence: Option<f64>,
    valid_from: DateTime<Utc>,
    invalid_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    entity_refs: serde_json::Value,
    supersedes: Option<Uuid>,
    superseded_by: Option<Uuid>,
    access_count: i64,
    last_accessed: Option<DateTime<Utc>>,
    metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

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
    let entity_refs: Vec<Uuid> = {
        let strings: Vec<String> = serde_json::from_value(row.entity_refs.clone())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;
        strings
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| MemoryError::Serialization(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?
    };

    let metadata: serde_json::Map<String, serde_json::Value> = match &row.metadata {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map.clone(),
        other => serde_json::from_value(other.clone())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?,
    };

    Ok(Fact {
        id: row.id,
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
        valid_from: row.valid_from,
        invalid_at: row.invalid_at,
        created_at: row.created_at,
        embedding: Vec::new(),
        entity_refs,
        supersedes: row.supersedes,
        superseded_by: row.superseded_by,
        access_count: row.access_count as u64,
        last_accessed: row.last_accessed,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// FactStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl FactStore for PostgresFactStore {
    async fn insert_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
        let entity_refs_json = {
            let strs: Vec<String> = fact.entity_refs.iter().map(|u| u.to_string()).collect();
            serde_json::to_value(&strs).map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        let metadata_json = if fact.metadata.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&fact.metadata)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        sqlx::query(
            r#"
            INSERT INTO facts
                (id, text, org_id, agent_id, user_id, session_id,
                 tier, category, source, confidence,
                 valid_from, invalid_at, created_at,
                 entity_refs, supersedes, superseded_by,
                 access_count, last_accessed, metadata)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(fact.id)
        .bind(&fact.text)
        .bind(&fact.scope.org_id)
        .bind(fact.scope.agent_id.as_deref())
        .bind(fact.scope.user_id.as_deref())
        .bind(fact.scope.session_id.as_deref())
        .bind(tier_to_str(&fact.tier))
        .bind(fact.category.as_deref())
        .bind(fact.source.as_deref())
        .bind(fact.confidence.map(|c| c as f64))
        .bind(fact.valid_from)
        .bind(fact.invalid_at)
        .bind(fact.created_at)
        .bind(&entity_refs_json)
        .bind(fact.supersedes)
        .bind(fact.superseded_by)
        .bind(fact.access_count as i64)
        .bind(fact.last_accessed)
        .bind(&metadata_json)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(fact.id)
    }

    async fn get_fact(&self, id: FactId) -> Result<Fact, MemoryError> {
        let row = sqlx::query_as::<_, FactRow>(
            "SELECT id, text, org_id, agent_id, user_id, session_id, tier, category, source, confidence, valid_from, invalid_at, created_at, entity_refs, supersedes, superseded_by, access_count, last_accessed, metadata FROM facts WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?
        .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;

        row_to_fact(row)
    }

    async fn update_fact(&self, id: FactId, patch: FactPatch) -> Result<Fact, MemoryError> {
        // Each value is bound as the column's own type. They used to be bound
        // as text, and Postgres has no `uuid = text`: every patch failed.
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE facts SET ");
        let mut set = query.separated(", ");
        let mut changed = false;

        if let Some(ref text) = patch.text {
            set.push("text = ").push_bind_unseparated(text.clone());
            changed = true;
        }
        if let Some(ref tier) = patch.tier {
            set.push("tier = ").push_bind_unseparated(tier_to_str(tier));
            changed = true;
        }
        if let Some(ref category) = patch.category {
            set.push("category = ")
                .push_bind_unseparated(category.clone());
            changed = true;
        }
        if let Some(ref source) = patch.source {
            set.push("source = ").push_bind_unseparated(source.clone());
            changed = true;
        }
        if let Some(confidence) = patch.confidence {
            set.push("confidence = ")
                .push_bind_unseparated(f64::from(confidence));
            changed = true;
        }
        if let Some(invalid_at) = patch.invalid_at {
            set.push("invalid_at = ").push_bind_unseparated(invalid_at);
            changed = true;
        }
        if let Some(superseded_by) = patch.superseded_by {
            set.push("superseded_by = ")
                .push_bind_unseparated(superseded_by);
            changed = true;
        }
        if !patch.metadata.is_empty() {
            set.push("metadata = ")
                .push_bind_unseparated(serde_json::Value::Object(patch.metadata.clone()));
            changed = true;
        }

        if changed {
            query.push(" WHERE id = ").push_bind(id);
            query
                .build()
                .execute(&self.pool)
                .await
                .map_err(|e| MemoryError::Database(e.to_string()))?;
        }

        self.get_fact(id).await
    }

    async fn list_facts(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError> {
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "SELECT id, text, org_id, agent_id, user_id, session_id, tier, category, source, confidence, valid_from, invalid_at, created_at, entity_refs, supersedes, superseded_by, access_count, last_accessed, metadata FROM facts WHERE 1=1",
        );

        if let Some(ref scope) = filter.scope {
            query.push(" AND ");
            crate::sql::push_scope(&mut query, scope);
        }

        if let Some(ref tier) = filter.tier {
            query.push(" AND tier = ").push_bind(tier_to_str(tier));
        }

        if let Some(ref category) = filter.category {
            query.push(" AND category = ").push_bind(category.clone());
        }

        if let Some(as_of) = filter.as_of {
            query.push(" AND valid_from <= ").push_bind(as_of);
            query
                .push(" AND (invalid_at IS NULL OR invalid_at > ")
                .push_bind(as_of)
                .push(")");
        } else if filter.valid_only {
            query.push(" AND invalid_at IS NULL");
        }

        if let Some(ref text_contains) = filter.text_contains {
            query
                .push(" AND text LIKE ")
                .push_bind(format!("%{text_contains}%"));
        }

        query
            .push(" ORDER BY created_at DESC LIMIT ")
            .push_bind(i64::from(filter.limit))
            .push(" OFFSET ")
            .push_bind(i64::from(filter.offset));

        let rows = query
            .build_query_as::<FactRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_fact).collect()
    }

    async fn invalidate_fact(&self, id: FactId) -> Result<(), MemoryError> {
        let now = Utc::now();
        sqlx::query("UPDATE facts SET invalid_at = $1 WHERE id = $2")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn delete_scope_data(&self, scope: &Scope) -> Result<u64, MemoryError> {
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new("DELETE FROM facts WHERE ");
        crate::sql::push_scope(&mut query, scope);

        let result = query
            .build()
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(result.rows_affected())
    }

    async fn delete_scope_returning(&self, scope: &Scope) -> Result<Vec<FactId>, MemoryError> {
        // One statement: the ids are exactly the rows this deleted.
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new("DELETE FROM facts WHERE ");
        crate::sql::push_scope(&mut query, scope);
        query.push(" RETURNING id");
        let rows: Vec<(uuid::Uuid,)> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn export(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError> {
        self.list_facts(filter).await
    }

    async fn import(&self, facts: Vec<Fact>) -> Result<u64, MemoryError> {
        let mut imported: u64 = 0;
        for fact in facts {
            let entity_refs_json = {
                let strs: Vec<String> = fact.entity_refs.iter().map(|u| u.to_string()).collect();
                serde_json::to_value(&strs)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            };

            let metadata_json = if fact.metadata.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::to_value(&fact.metadata)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?
            };

            let result = sqlx::query(
                r#"
                INSERT INTO facts
                    (id, text, org_id, agent_id, user_id, session_id,
                     tier, category, source, confidence,
                     valid_from, invalid_at, created_at,
                     entity_refs, supersedes, superseded_by,
                     access_count, last_accessed, metadata)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
                ON CONFLICT (id) DO NOTHING
                "#,
            )
            .bind(fact.id)
            .bind(&fact.text)
            .bind(&fact.scope.org_id)
            .bind(fact.scope.agent_id.as_deref())
            .bind(fact.scope.user_id.as_deref())
            .bind(fact.scope.session_id.as_deref())
            .bind(tier_to_str(&fact.tier))
            .bind(fact.category.as_deref())
            .bind(fact.source.as_deref())
            .bind(fact.confidence.map(|c| c as f64))
            .bind(fact.valid_from)
            .bind(fact.invalid_at)
            .bind(fact.created_at)
            .bind(&entity_refs_json)
            .bind(fact.supersedes)
            .bind(fact.superseded_by)
            .bind(fact.access_count as i64)
            .bind(fact.last_accessed)
            .bind(&metadata_json)
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
        let now = Utc::now();
        sqlx::query(
            "UPDATE facts SET access_count = access_count + 1, last_accessed = $1 WHERE id = $2",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn keyword_search(
        &self,
        query: &str,
        scope: &Scope,
        top_k: usize,
    ) -> Result<Vec<Fact>, MemoryError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let sql = r#"
            SELECT id, text, org_id, agent_id, user_id, session_id, tier, category,
                   source, confidence, valid_from, invalid_at, created_at, entity_refs,
                   supersedes, superseded_by, access_count, last_accessed, metadata
            FROM facts
            WHERE search_vector @@ plainto_tsquery('english', $1)
              AND org_id = $2
              AND ($3::text IS NULL OR user_id = $3)
              AND invalid_at IS NULL
            ORDER BY ts_rank(search_vector, plainto_tsquery('english', $1)) DESC
            LIMIT $4
        "#;

        let rows = sqlx::query_as::<_, FactRow>(sql)
            .bind(trimmed)
            .bind(&scope.org_id)
            .bind(scope.user_id.as_deref())
            .bind(top_k as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_fact).collect()
    }
}

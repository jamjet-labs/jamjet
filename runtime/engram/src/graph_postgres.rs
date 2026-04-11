//! PostgreSQL-backed `GraphStore` implementation.
//!
//! Uses native Postgres types: `UUID` for ids, `TIMESTAMPTZ` for timestamps,
//! `JSONB` for attributes. Placeholder syntax uses `$1, $2, …`.

use crate::fact::{Entity, EntityId, Relationship, RelationshipId, SubGraph};
use crate::graph::GraphStore;
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// DDL statements for the Postgres graph tables. Each element is a single
/// statement to be executed independently.
const PG_GRAPH_STORE_DDL: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS entities (
        id          UUID PRIMARY KEY,
        name        TEXT NOT NULL,
        entity_type TEXT,
        org_id      TEXT NOT NULL DEFAULT 'default',
        agent_id    TEXT,
        user_id     TEXT,
        session_id  TEXT,
        attributes  JSONB NOT NULL DEFAULT 'null',
        created_at  TIMESTAMPTZ NOT NULL,
        updated_at  TIMESTAMPTZ NOT NULL
    )
    "#,
    "CREATE INDEX IF NOT EXISTS idx_pg_entities_name    ON entities (name)",
    "CREATE INDEX IF NOT EXISTS idx_pg_entities_user_id ON entities (user_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_entities_org_id  ON entities (org_id)",
    r#"
    CREATE TABLE IF NOT EXISTS relationships (
        id          UUID PRIMARY KEY,
        source_id   UUID NOT NULL,
        relation    TEXT NOT NULL,
        target_id   UUID NOT NULL,
        org_id      TEXT NOT NULL DEFAULT 'default',
        agent_id    TEXT,
        user_id     TEXT,
        session_id  TEXT,
        valid_from  TIMESTAMPTZ NOT NULL,
        invalid_at  TIMESTAMPTZ,
        created_at  TIMESTAMPTZ NOT NULL
    )
    "#,
    "CREATE INDEX IF NOT EXISTS idx_pg_rel_source_id  ON relationships (source_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_rel_target_id  ON relationships (target_id)",
    "CREATE INDEX IF NOT EXISTS idx_pg_rel_relation   ON relationships (relation)",
    "CREATE INDEX IF NOT EXISTS idx_pg_rel_invalid_at ON relationships (invalid_at)",
];

// ---------------------------------------------------------------------------
// PostgresGraphStore
// ---------------------------------------------------------------------------

pub struct PostgresGraphStore {
    pool: PgPool,
}

impl PostgresGraphStore {
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
        for stmt in PG_GRAPH_STORE_DDL {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal row types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct EntityRow {
    id: Uuid,
    name: String,
    entity_type: Option<String>,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    attributes: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct RelationshipRow {
    id: Uuid,
    source_id: Uuid,
    relation: String,
    target_id: Uuid,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    valid_from: DateTime<Utc>,
    invalid_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn row_to_entity(row: EntityRow) -> Result<Entity, MemoryError> {
    let attributes: serde_json::Map<String, serde_json::Value> = match &row.attributes {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map.clone(),
        other => serde_json::from_value(other.clone())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?,
    };

    Ok(Entity {
        id: row.id,
        name: row.name,
        entity_type: row.entity_type.unwrap_or_else(|| "unknown".to_string()),
        scope: Scope {
            org_id: row.org_id,
            agent_id: row.agent_id,
            user_id: row.user_id,
            session_id: row.session_id,
        },
        attributes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn row_to_relationship(row: RelationshipRow) -> Result<Relationship, MemoryError> {
    Ok(Relationship {
        id: row.id,
        source_id: row.source_id,
        relation: row.relation,
        target_id: row.target_id,
        scope: Scope {
            org_id: row.org_id,
            agent_id: row.agent_id,
            user_id: row.user_id,
            session_id: row.session_id,
        },
        valid_from: row.valid_from,
        invalid_at: row.invalid_at,
        created_at: row.created_at,
    })
}

// ---------------------------------------------------------------------------
// GraphStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl GraphStore for PostgresGraphStore {
    async fn upsert_entity(&self, entity: &Entity) -> Result<(), MemoryError> {
        let attributes_json = if entity.attributes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&entity.attributes)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        sqlx::query(
            r#"
            INSERT INTO entities
                (id, name, entity_type, org_id, agent_id, user_id, session_id,
                 attributes, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT(id) DO UPDATE SET
                name        = EXCLUDED.name,
                entity_type = EXCLUDED.entity_type,
                attributes  = EXCLUDED.attributes,
                updated_at  = EXCLUDED.updated_at
            "#,
        )
        .bind(entity.id)
        .bind(&entity.name)
        .bind(&entity.entity_type)
        .bind(&entity.scope.org_id)
        .bind(entity.scope.agent_id.as_deref())
        .bind(entity.scope.user_id.as_deref())
        .bind(entity.scope.session_id.as_deref())
        .bind(&attributes_json)
        .bind(entity.created_at)
        .bind(entity.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(())
    }

    async fn upsert_relationship(&self, rel: &Relationship) -> Result<(), MemoryError> {
        sqlx::query(
            r#"
            INSERT INTO relationships
                (id, source_id, relation, target_id, org_id, agent_id, user_id, session_id,
                 valid_from, invalid_at, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT(id) DO UPDATE SET
                relation   = EXCLUDED.relation,
                invalid_at = EXCLUDED.invalid_at
            "#,
        )
        .bind(rel.id)
        .bind(rel.source_id)
        .bind(&rel.relation)
        .bind(rel.target_id)
        .bind(&rel.scope.org_id)
        .bind(rel.scope.agent_id.as_deref())
        .bind(rel.scope.user_id.as_deref())
        .bind(rel.scope.session_id.as_deref())
        .bind(rel.valid_from)
        .bind(rel.invalid_at)
        .bind(rel.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(())
    }

    async fn invalidate_relationship(
        &self,
        id: RelationshipId,
        invalid_at: DateTime<Utc>,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE relationships SET invalid_at = $1 WHERE id = $2")
            .bind(invalid_at)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, MemoryError> {
        let row = sqlx::query_as::<_, EntityRow>("SELECT * FROM entities WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        row.map(row_to_entity).transpose()
    }

    async fn neighbors(
        &self,
        id: EntityId,
        depth: u8,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<SubGraph, MemoryError> {
        // Build the temporal validity filter for relationships.
        let validity_clause = match as_of {
            Some(t) => {
                let s = t.to_rfc3339();
                format!("valid_from <= '{s}' AND (invalid_at IS NULL OR invalid_at > '{s}')")
            }
            None => "invalid_at IS NULL".to_string(),
        };

        let mut visited_entities: HashSet<EntityId> = HashSet::new();
        visited_entities.insert(id);

        let mut discovered_entities: HashMap<EntityId, Entity> = HashMap::new();
        let mut discovered_relationships: HashMap<RelationshipId, Relationship> = HashMap::new();

        // BFS queue: (entity_id, remaining_depth)
        let mut queue: VecDeque<(EntityId, u8)> = VecDeque::new();
        queue.push_back((id, depth));

        while let Some((current_id, remaining)) = queue.pop_front() {
            if remaining == 0 {
                continue;
            }

            // Fetch all valid relationships where current_id is source or target.
            let sql = format!(
                "SELECT * FROM relationships WHERE (source_id = $1 OR target_id = $2) AND {validity_clause}"
            );

            let rel_rows = sqlx::query_as::<_, RelationshipRow>(&sql)
                .bind(current_id)
                .bind(current_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| MemoryError::Database(e.to_string()))?;

            for row in rel_rows {
                let rel = row_to_relationship(row)?;
                let neighbor_id = if rel.source_id == current_id {
                    rel.target_id
                } else {
                    rel.source_id
                };

                // Store the relationship (deduplicated by id).
                discovered_relationships.entry(rel.id).or_insert(rel);

                // Enqueue unvisited neighbors.
                if !visited_entities.contains(&neighbor_id) {
                    visited_entities.insert(neighbor_id);

                    // Fetch the neighbor entity.
                    if let Some(entity) = self.get_entity(neighbor_id).await? {
                        discovered_entities.entry(neighbor_id).or_insert(entity);
                    }

                    queue.push_back((neighbor_id, remaining - 1));
                }
            }
        }

        Ok(SubGraph {
            entities: discovered_entities.into_values().collect(),
            relationships: discovered_relationships.into_values().collect(),
        })
    }

    async fn search_entities(&self, query: &str, top_k: usize) -> Result<Vec<Entity>, MemoryError> {
        let sql = "SELECT * FROM entities WHERE name ILIKE $1 LIMIT $2";

        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));

        let rows = sqlx::query_as::<_, EntityRow>(sql)
            .bind(&pattern)
            .bind(top_k as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        rows.into_iter().map(row_to_entity).collect()
    }

    async fn delete_by_scope(&self, scope: &Scope) -> Result<u64, MemoryError> {
        // Build WHERE clause matching the scope fields.
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

        // Delete relationships first.
        let rel_sql = format!("DELETE FROM relationships WHERE {where_clause}");
        let rel_result = sqlx::query(&rel_sql)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        // Then delete entities.
        let ent_sql = format!("DELETE FROM entities WHERE {where_clause}");
        let ent_result = sqlx::query(&ent_sql)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(rel_result.rows_affected() + ent_result.rows_affected())
    }
}

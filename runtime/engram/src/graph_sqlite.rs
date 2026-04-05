//! SQLite-backed `GraphStore` implementation.
//!
//! All `DateTime<Utc>` values are stored as RFC 3339 strings.
//! UUIDs are stored as TEXT. `attributes` is a JSON object string.

use crate::fact::{Entity, EntityId, Relationship, RelationshipId, SubGraph};
use crate::graph::GraphStore;
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

pub const GRAPH_STORE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS entities (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    entity_type TEXT,
    org_id      TEXT NOT NULL DEFAULT 'default',
    agent_id    TEXT,
    user_id     TEXT,
    session_id  TEXT,
    attributes  TEXT NOT NULL DEFAULT 'null',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_entities_name    ON entities (name);
CREATE INDEX IF NOT EXISTS idx_entities_user_id ON entities (user_id);
CREATE INDEX IF NOT EXISTS idx_entities_org_id  ON entities (org_id);

CREATE TABLE IF NOT EXISTS relationships (
    id          TEXT PRIMARY KEY,
    source_id   TEXT NOT NULL,
    relation    TEXT NOT NULL,
    target_id   TEXT NOT NULL,
    org_id      TEXT NOT NULL DEFAULT 'default',
    agent_id    TEXT,
    user_id     TEXT,
    session_id  TEXT,
    valid_from  TEXT NOT NULL,
    invalid_at  TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rel_source_id  ON relationships (source_id);
CREATE INDEX IF NOT EXISTS idx_rel_target_id  ON relationships (target_id);
CREATE INDEX IF NOT EXISTS idx_rel_relation   ON relationships (relation);
CREATE INDEX IF NOT EXISTS idx_rel_invalid_at ON relationships (invalid_at);
"#;

// ---------------------------------------------------------------------------
// SqliteGraphStore
// ---------------------------------------------------------------------------

pub struct SqliteGraphStore {
    pool: SqlitePool,
}

impl SqliteGraphStore {
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
        for stmt in GRAPH_STORE_DDL.split(';') {
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
// Internal row types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct EntityRow {
    id: String,
    name: String,
    entity_type: Option<String>,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    attributes: String,
    created_at: String,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct RelationshipRow {
    id: String,
    source_id: String,
    relation: String,
    target_id: String,
    org_id: String,
    agent_id: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
    valid_from: String,
    invalid_at: Option<String>,
    created_at: String,
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

fn row_to_entity(row: EntityRow) -> Result<Entity, MemoryError> {
    let id = Uuid::parse_str(&row.id).map_err(|e| MemoryError::Serialization(e.to_string()))?;

    let attributes: serde_json::Map<String, serde_json::Value> =
        if row.attributes == "null" || row.attributes.is_empty() {
            serde_json::Map::new()
        } else {
            serde_json::from_str(&row.attributes)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

    Ok(Entity {
        id,
        name: row.name,
        entity_type: row.entity_type.unwrap_or_else(|| "unknown".to_string()),
        scope: Scope {
            org_id: row.org_id,
            agent_id: row.agent_id,
            user_id: row.user_id,
            session_id: row.session_id,
        },
        attributes,
        created_at: parse_dt(&row.created_at)?,
        updated_at: parse_dt(&row.updated_at)?,
    })
}

fn row_to_relationship(row: RelationshipRow) -> Result<Relationship, MemoryError> {
    let id = Uuid::parse_str(&row.id).map_err(|e| MemoryError::Serialization(e.to_string()))?;
    let source_id =
        Uuid::parse_str(&row.source_id).map_err(|e| MemoryError::Serialization(e.to_string()))?;
    let target_id =
        Uuid::parse_str(&row.target_id).map_err(|e| MemoryError::Serialization(e.to_string()))?;

    Ok(Relationship {
        id,
        source_id,
        relation: row.relation,
        target_id,
        scope: Scope {
            org_id: row.org_id,
            agent_id: row.agent_id,
            user_id: row.user_id,
            session_id: row.session_id,
        },
        valid_from: parse_dt(&row.valid_from)?,
        invalid_at: parse_opt_dt(&row.invalid_at)?,
        created_at: parse_dt(&row.created_at)?,
    })
}

// ---------------------------------------------------------------------------
// GraphStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl GraphStore for SqliteGraphStore {
    async fn upsert_entity(&self, entity: &Entity) -> Result<(), MemoryError> {
        let attributes_json = if entity.attributes.is_empty() {
            "null".to_string()
        } else {
            serde_json::to_string(&entity.attributes)
                .map_err(|e| MemoryError::Serialization(e.to_string()))?
        };

        sqlx::query(
            r#"
            INSERT INTO entities
                (id, name, entity_type, org_id, agent_id, user_id, session_id,
                 attributes, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name        = excluded.name,
                entity_type = excluded.entity_type,
                attributes  = excluded.attributes,
                updated_at  = excluded.updated_at
            "#,
        )
        .bind(entity.id.to_string())
        .bind(&entity.name)
        .bind(&entity.entity_type)
        .bind(&entity.scope.org_id)
        .bind(entity.scope.agent_id.as_deref())
        .bind(entity.scope.user_id.as_deref())
        .bind(entity.scope.session_id.as_deref())
        .bind(&attributes_json)
        .bind(entity.created_at.to_rfc3339())
        .bind(entity.updated_at.to_rfc3339())
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
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                relation   = excluded.relation,
                invalid_at = excluded.invalid_at
            "#,
        )
        .bind(rel.id.to_string())
        .bind(rel.source_id.to_string())
        .bind(&rel.relation)
        .bind(rel.target_id.to_string())
        .bind(&rel.scope.org_id)
        .bind(rel.scope.agent_id.as_deref())
        .bind(rel.scope.user_id.as_deref())
        .bind(rel.scope.session_id.as_deref())
        .bind(rel.valid_from.to_rfc3339())
        .bind(rel.invalid_at.map(|dt| dt.to_rfc3339()))
        .bind(rel.created_at.to_rfc3339())
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
        sqlx::query("UPDATE relationships SET invalid_at = ? WHERE id = ?")
            .bind(invalid_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, MemoryError> {
        let row = sqlx::query_as::<_, EntityRow>("SELECT * FROM entities WHERE id = ?")
            .bind(id.to_string())
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
                format!(
                    "valid_from <= '{s}' AND (invalid_at IS NULL OR invalid_at > '{s}')"
                )
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
                "SELECT * FROM relationships WHERE (source_id = ? OR target_id = ?) AND {validity_clause}"
            );

            let rel_rows = sqlx::query_as::<_, RelationshipRow>(&sql)
                .bind(current_id.to_string())
                .bind(current_id.to_string())
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
        let escaped = query.replace('\'', "''");
        let sql = format!(
            "SELECT * FROM entities WHERE name LIKE '%{escaped}%' LIMIT {top_k}"
        );

        let rows = sqlx::query_as::<_, EntityRow>(&sql)
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
            wheres.push(format!(
                "session_id = '{}'",
                session_id.replace('\'', "''")
            ));
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

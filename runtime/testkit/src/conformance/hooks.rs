//! What a case needs from a store that the [`StateBackend`] trait does not
//! offer. A hook that writes panics when it changes nothing: one that
//! silently missed its row would make the case prove nothing.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::{
    InMemoryBackend, PgBackend, SqliteBackend, StateBackend, TenantId, WorkItem, WorkItemId,
    DEFAULT_TENANT,
};
use sqlx::postgres::{PgArguments, Postgres};
use sqlx::sqlite::{Sqlite, SqliteArguments, SqliteRow};
use sqlx::Row;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Every row of a store, by table. Each row is one line of text naming its
/// columns, and a table's rows are sorted, so two reads of an unchanged
/// store compare equal.
pub type StoreRows = BTreeMap<String, Vec<String>>;

/// A JSON value the store keeps as text, named by the row that holds it.
#[derive(Debug, Clone)]
pub enum StoredJson {
    /// The kind of the run's event at `sequence`.
    Event {
        execution: ExecutionId,
        sequence: i64,
    },
    /// The work item's payload.
    ItemPayload(WorkItemId),
    /// The run's input.
    RunInput(ExecutionId),
}

/// A work item as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRow {
    /// `pending`, `claimed`, `completed`, `failed` or `dead_lettered`.
    pub status: String,
    pub attempt: u32,
    pub lease_epoch: i64,
    pub lease_fence: i64,
    pub worker_id: Option<String>,
    pub retry_after: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait StoreHooks: Send + Sync {
    /// Move the item's lease into the past, so the next sweep or claim
    /// takes it as expired.
    async fn force_lease_expired(&self, item: WorkItemId);
    /// Set the item's lease expiry.
    async fn set_lease_expiry(&self, item: WorkItemId, at: DateTime<Utc>);
    /// Raise the store's failover term to at least `term`, as a promotion
    /// does. Returns the term after.
    async fn set_store_term_at_least(&self, term: i64) -> i64;
    /// The item's row. Memory keeps no row for an item once it completed or
    /// failed, so there it is `None`; a dead-lettered item reads
    /// `dead_lettered` everywhere. Memory has no `retry_after`.
    async fn item_row(&self, item: WorkItemId) -> Option<ItemRow>;
    /// The run of the item's dead-letter row, if it has one.
    async fn dead_letter_execution(&self, item: WorkItemId) -> Option<ExecutionId>;
    /// How many stored copies of the artifact there are, in `tenant` or in
    /// all tenants.
    async fn artifact_rows(&self, hash: &str, tenant: Option<&str>) -> u64;
    /// Move the named token to `tenant`, as an operator editing its row
    /// would. Where tenants are rows, `tenant` must exist.
    async fn set_token_tenant(&self, token_name: &str, tenant: &str);
    /// A new handle on the same store through a new pool, as after a
    /// restart, scoped like the fixture's backend. `None` for memory.
    async fn reopen(&self) -> Option<Arc<dyn StateBackend>>;
    /// The same store with no tenant scope, on the fixture's first pool, as
    /// the engine's scheduler holds it. On memory, the backend itself.
    fn unscoped(&self) -> Arc<dyn StateBackend>;
    /// The tenant on the item's dead-letter row, if it has one.
    async fn dead_letter_tenant(&self, item: WorkItemId) -> Option<String>;
    /// The hash stored for the token. `None` where the store keeps no hash
    /// (memory) or there is no such token.
    async fn stored_token_hash(&self, token_id: &str) -> Option<String>;
    /// Write a token row by its hash, in the default tenant, as an earlier
    /// build did. `false` where the store keeps no hash (memory).
    async fn insert_token_row(&self, id: &str, token_hash: &str, name: &str, role: &str) -> bool;
    /// Write an event of the run whose kind is `kind_json` as stored text,
    /// as a later release might. `false` where the store keeps parsed
    /// events (memory).
    async fn insert_event_json(
        &self,
        execution: &ExecutionId,
        sequence: i64,
        kind_json: &str,
    ) -> bool;
    /// Damage the run's row so it no longer reads back. `false` where the
    /// store cannot hold such a row (memory).
    async fn make_execution_unreadable(&self, execution: &ExecutionId) -> bool;
    /// The JSON text the store holds for `what`, exactly as stored. `None`
    /// where the store keeps parsed values (memory); a missing row panics.
    async fn stored_json(&self, what: StoredJson) -> Option<String>;
    /// Every row of every table the store holds, across tenants. `None`
    /// where the store keeps no rows (memory). Panics on a table it does not
    /// know, so a new table cannot drop out of the comparison unseen.
    async fn every_row(&self) -> Option<StoreRows>;
}

/// Hooks on an [`InMemoryBackend`].
pub(crate) struct MemoryHooks(pub Arc<InMemoryBackend>);

impl MemoryHooks {
    async fn live(&self, item: WorkItemId) -> Option<WorkItem> {
        self.0
            .get_work_item(item)
            .await
            .expect("memory reads cannot fail")
    }
}

#[async_trait]
impl StoreHooks for MemoryHooks {
    async fn force_lease_expired(&self, item: WorkItemId) {
        self.set_lease_expiry(item, past()).await;
    }

    async fn set_lease_expiry(&self, item: WorkItemId, at: DateTime<Utc>) {
        assert!(self.live(item).await.is_some(), "no work item {item}");
        self.0.set_lease_expiry_for_test(item, at);
    }

    async fn set_store_term_at_least(&self, term: i64) -> i64 {
        self.0.set_store_term_at_least(term)
    }

    async fn item_row(&self, item: WorkItemId) -> Option<ItemRow> {
        let lease_epoch = self.0.lease_epoch_for_test(item);
        // Memory has no status column: a live item is claimed while it has
        // a worker, and a dead-lettered one sits in the dead-letter queue.
        if let Some(w) = self.live(item).await {
            return Some(ItemRow {
                status: if w.worker_id.is_some() {
                    "claimed"
                } else {
                    "pending"
                }
                .into(),
                attempt: w.attempt,
                lease_epoch,
                lease_fence: w.lease_fence,
                worker_id: w.worker_id,
                retry_after: None,
            });
        }
        self.0.dead_letter_item_for_test(item).map(|w| ItemRow {
            status: "dead_lettered".into(),
            attempt: w.attempt,
            lease_epoch,
            lease_fence: w.lease_fence,
            // The SQL stores clear it when they dead-letter.
            worker_id: None,
            retry_after: None,
        })
    }

    async fn dead_letter_execution(&self, item: WorkItemId) -> Option<ExecutionId> {
        self.0
            .dead_letter_item_for_test(item)
            .map(|w| w.execution_id)
    }

    async fn artifact_rows(&self, hash: &str, tenant: Option<&str>) -> u64 {
        // Memory keeps one copy per hash, all in the default tenant.
        if tenant.is_some_and(|t| t != DEFAULT_TENANT) {
            return 0;
        }
        let stored = self
            .0
            .get_artifact(hash)
            .await
            .expect("memory reads cannot fail");
        u64::from(stored.is_some())
    }

    async fn set_token_tenant(&self, token_name: &str, tenant: &str) {
        let moved = self.0.set_token_tenant_for_test(token_name, tenant);
        assert!(moved > 0, "no token named {token_name}");
    }

    async fn reopen(&self) -> Option<Arc<dyn StateBackend>> {
        None
    }

    fn unscoped(&self) -> Arc<dyn StateBackend> {
        self.0.clone()
    }

    async fn dead_letter_tenant(&self, item: WorkItemId) -> Option<String> {
        self.0.dead_letter_item_for_test(item).map(|w| w.tenant_id)
    }

    async fn stored_token_hash(&self, _token_id: &str) -> Option<String> {
        // Memory keys tokens by their plaintext.
        None
    }

    async fn insert_token_row(
        &self,
        _id: &str,
        _token_hash: &str,
        _name: &str,
        _role: &str,
    ) -> bool {
        false
    }

    async fn insert_event_json(
        &self,
        _execution: &ExecutionId,
        _sequence: i64,
        _kind_json: &str,
    ) -> bool {
        false
    }

    async fn make_execution_unreadable(&self, _execution: &ExecutionId) -> bool {
        false
    }

    async fn stored_json(&self, _what: StoredJson) -> Option<String> {
        None
    }

    async fn every_row(&self) -> Option<StoreRows> {
        None
    }
}

/// The tables of the SQLite state schema, every one of which
/// [`StoreHooks::every_row`] reads. SQLite's own `sqlite_*` tables and the
/// migration log are left out.
const SQLITE_TABLES: &[&str] = &[
    "agents",
    "api_tokens",
    "artifacts",
    "cron_jobs",
    "dead_letter_items",
    "engine_ticks",
    "events",
    "proj_approvals",
    "projector_checkpoints",
    "run_keys",
    "snapshots",
    "store_identity",
    "tenants",
    "timers",
    "tool_effects",
    "tool_reservations",
    "work_items",
    "workflow_definitions",
    "workflow_executions",
];

/// A row as `column=value` pairs: text and numbers quoted as Rust would
/// print them, blobs in hex, `NULL` for null.
fn row_text(row: &SqliteRow) -> String {
    use sqlx::{Column, TypeInfo, ValueRef};
    row.columns()
        .iter()
        .map(|column| {
            let i = column.ordinal();
            let raw = row
                .try_get_raw(i)
                .unwrap_or_else(|e| panic!("read {}: {e}", column.name()));
            let kind = raw.type_info().name().to_string();
            let value = if raw.is_null() {
                "NULL".to_string()
            } else {
                match kind.as_str() {
                    "INTEGER" => value_at::<i64>(row, i).to_string(),
                    "REAL" => value_at::<f64>(row, i).to_string(),
                    "BLOB" => {
                        let hex: String = value_at::<Vec<u8>>(row, i)
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        format!("x'{hex}'")
                    }
                    _ => format!("{:?}", value_at::<String>(row, i)),
                }
            };
            format!("{}={value}", column.name())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Column `i` of `row`, read as the type its value is stored as.
fn value_at<'r, T: sqlx::Decode<'r, Sqlite>>(row: &'r SqliteRow, i: usize) -> T {
    row.try_get_unchecked::<T, _>(i)
        .unwrap_or_else(|e| panic!("read column {i}: {e}"))
}

/// Hooks on a SQLite file, through the fixture's first pool.
pub(crate) struct SqliteHooks {
    pub base: Arc<SqliteBackend>,
    pub url: String,
    /// The tenant the fixture's backend is scoped to, if it is.
    pub scope: Option<TenantId>,
}

impl SqliteHooks {
    /// Run a statement that must change exactly one row.
    async fn update_one(&self, what: &str, query: sqlx::query::Query<'_, Sqlite, SqliteArguments>) {
        let done = query
            .execute(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(done.rows_affected(), 1, "{what}: no such row");
    }
}

#[async_trait]
impl StoreHooks for SqliteHooks {
    async fn force_lease_expired(&self, item: WorkItemId) {
        self.set_lease_expiry(item, past()).await;
    }

    async fn set_lease_expiry(&self, item: WorkItemId, at: DateTime<Utc>) {
        // The format the backend writes, which its expiry checks compare as text.
        let query = sqlx::query("UPDATE work_items SET lease_expires_at = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(item.to_string());
        self.update_one(&format!("set the lease of {item}"), query)
            .await;
    }

    async fn set_store_term_at_least(&self, term: i64) -> i64 {
        self.base
            .set_store_term_at_least(term)
            .await
            .unwrap_or_else(|e| panic!("raise the store term: {e}"))
    }

    async fn item_row(&self, item: WorkItemId) -> Option<ItemRow> {
        let row = sqlx::query(
            "SELECT status, attempt, lease_epoch, lease_fence, worker_id, retry_after \
             FROM work_items WHERE id = ?",
        )
        .bind(item.to_string())
        .fetch_optional(&self.base.pool())
        .await
        .unwrap_or_else(|e| panic!("read work item {item}: {e}"))?;
        let retry_after: Option<String> = row.get("retry_after");
        Some(ItemRow {
            status: row.get("status"),
            attempt: u32::try_from(row.get::<i64, _>("attempt")).expect("attempt fits a u32"),
            lease_epoch: row.get("lease_epoch"),
            lease_fence: row.get("lease_fence"),
            worker_id: row.get("worker_id"),
            retry_after: retry_after.map(|t| {
                DateTime::parse_from_rfc3339(&t)
                    .unwrap_or_else(|e| panic!("retry_after {t:?} of {item}: {e}"))
                    .with_timezone(&Utc)
            }),
        })
    }

    async fn dead_letter_execution(&self, item: WorkItemId) -> Option<ExecutionId> {
        let execution: Option<String> =
            sqlx::query_scalar("SELECT execution_id FROM dead_letter_items WHERE id = ?")
                .bind(item.to_string())
                .fetch_optional(&self.base.pool())
                .await
                .unwrap_or_else(|e| panic!("read the dead letter of {item}: {e}"));
        execution.map(|id| {
            ExecutionId(
                uuid::Uuid::parse_str(&id)
                    .unwrap_or_else(|e| panic!("dead letter execution id {id:?}: {e}")),
            )
        })
    }

    async fn artifact_rows(&self, hash: &str, tenant: Option<&str>) -> u64 {
        let pool = self.base.pool();
        let count: i64 = match tenant {
            None => {
                sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE hash = ?")
                    .bind(hash)
                    .fetch_one(&pool)
                    .await
            }
            Some(tenant) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM artifacts WHERE hash = ? AND tenant_id = ?",
                )
                .bind(hash)
                .bind(tenant)
                .fetch_one(&pool)
                .await
            }
        }
        .unwrap_or_else(|e| panic!("count artifact {hash}: {e}"));
        u64::try_from(count).expect("a count is not negative")
    }

    async fn set_token_tenant(&self, token_name: &str, tenant: &str) {
        let done = sqlx::query("UPDATE api_tokens SET tenant_id = ? WHERE name = ?")
            .bind(tenant)
            .bind(token_name)
            .execute(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("move token {token_name} to {tenant}: {e}"));
        assert!(done.rows_affected() > 0, "no token named {token_name}");
    }

    async fn reopen(&self) -> Option<Arc<dyn StateBackend>> {
        let again = super::factories::open(&self.url).await;
        Some(match &self.scope {
            Some(tenant) => Arc::new(again.for_tenant(tenant.clone())),
            None => Arc::new(again),
        })
    }

    fn unscoped(&self) -> Arc<dyn StateBackend> {
        self.base.clone()
    }

    async fn dead_letter_tenant(&self, item: WorkItemId) -> Option<String> {
        sqlx::query_scalar("SELECT tenant_id FROM dead_letter_items WHERE id = ?")
            .bind(item.to_string())
            .fetch_optional(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("read the dead letter of {item}: {e}"))
    }

    async fn stored_token_hash(&self, token_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT token_hash FROM api_tokens WHERE id = ?")
            .bind(token_id)
            .fetch_optional(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("read token {token_id}: {e}"))
    }

    async fn insert_token_row(&self, id: &str, token_hash: &str, name: &str, role: &str) -> bool {
        // The columns an earlier build wrote; the tenant takes its default.
        let query = sqlx::query(
            "INSERT INTO api_tokens (id, token_hash, name, role, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(token_hash)
        .bind(name)
        .bind(role)
        .bind(Utc::now().to_rfc3339());
        self.update_one(&format!("insert token {id}"), query).await;
        true
    }

    async fn insert_event_json(
        &self,
        execution: &ExecutionId,
        sequence: i64,
        kind_json: &str,
    ) -> bool {
        let query = sqlx::query(
            "INSERT INTO events (id, execution_id, sequence, kind_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(execution.0.to_string())
        .bind(sequence)
        .bind(kind_json)
        .bind(Utc::now().to_rfc3339());
        self.update_one(&format!("insert event {sequence} of {execution}"), query)
            .await;
        true
    }

    async fn make_execution_unreadable(&self, execution: &ExecutionId) -> bool {
        let query = sqlx::query(
            "UPDATE workflow_executions SET started_at = 'not a time' WHERE execution_id = ?",
        )
        .bind(execution.0.to_string());
        self.update_one(&format!("damage run {execution}"), query)
            .await;
        true
    }

    async fn stored_json(&self, what: StoredJson) -> Option<String> {
        let pool = self.base.pool();
        let text: Option<String> = match &what {
            StoredJson::Event {
                execution,
                sequence,
            } => {
                sqlx::query_scalar(
                    "SELECT kind_json FROM events WHERE execution_id = ? AND sequence = ?",
                )
                .bind(execution.0.to_string())
                .bind(sequence)
                .fetch_optional(&pool)
                .await
            }
            StoredJson::ItemPayload(item) => {
                sqlx::query_scalar("SELECT payload_json FROM work_items WHERE id = ?")
                    .bind(item.to_string())
                    .fetch_optional(&pool)
                    .await
            }
            StoredJson::RunInput(execution) => {
                sqlx::query_scalar(
                    "SELECT initial_input FROM workflow_executions WHERE execution_id = ?",
                )
                .bind(execution.0.to_string())
                .fetch_optional(&pool)
                .await
            }
        }
        .unwrap_or_else(|e| panic!("read {what:?}: {e}"));
        Some(text.unwrap_or_else(|| panic!("no row for {what:?}")))
    }

    async fn every_row(&self) -> Option<StoreRows> {
        let pool = self.base.pool();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite%' AND name != '_sqlx_migrations' ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap_or_else(|e| panic!("list the tables: {e}"));
        assert_eq!(
            tables, SQLITE_TABLES,
            "the schema's tables are not the ones every_row reads: add the new one to SQLITE_TABLES"
        );
        let mut rows = StoreRows::new();
        for table in SQLITE_TABLES {
            // `table` is one of the literal names above.
            let found = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {table}")))
                .fetch_all(&pool)
                .await
                .unwrap_or_else(|e| panic!("read {table}: {e}"));
            let mut texts: Vec<String> = found.iter().map(row_text).collect();
            texts.sort();
            rows.insert(table.to_string(), texts);
        }
        Some(rows)
    }
}

/// The tables of the Postgres state schema, every one of which
/// [`StoreHooks::every_row`] reads. The migration log is left out.
const POSTGRES_TABLES: &[&str] = &[
    "agents",
    "api_tokens",
    "artifacts",
    "audit_head",
    "audit_log",
    "audit_outbox",
    "dead_letter_items",
    "engine_ticks",
    "events",
    "proj_approvals",
    "projector_checkpoints",
    "run_keys",
    "snapshots",
    "store_identity",
    "tenants",
    "tool_effects",
    "tool_reservations",
    "work_items",
    "workflow_definitions",
    "workflow_executions",
];

/// Hooks on a Postgres test database, through the fixture's first pool.
pub(crate) struct PgHooks {
    pub base: Arc<PgBackend>,
    pub url: String,
    /// The tenant the fixture's backend is scoped to, if it is.
    pub scope: Option<TenantId>,
}

impl PgHooks {
    /// Run a statement that must change exactly one row.
    async fn update_one(&self, what: &str, query: sqlx::query::Query<'_, Postgres, PgArguments>) {
        let done = query
            .execute(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(done.rows_affected(), 1, "{what}: no such row");
    }
}

#[async_trait]
impl StoreHooks for PgHooks {
    async fn force_lease_expired(&self, item: WorkItemId) {
        self.set_lease_expiry(item, past()).await;
    }

    async fn set_lease_expiry(&self, item: WorkItemId, at: DateTime<Utc>) {
        let query = sqlx::query("UPDATE work_items SET lease_expires_at = $1 WHERE id = $2")
            .bind(at)
            .bind(item);
        self.update_one(&format!("set the lease of {item}"), query)
            .await;
    }

    async fn set_store_term_at_least(&self, term: i64) -> i64 {
        self.base
            .set_store_term_at_least(term)
            .await
            .unwrap_or_else(|e| panic!("raise the store term: {e}"))
    }

    async fn item_row(&self, item: WorkItemId) -> Option<ItemRow> {
        let row = sqlx::query(
            "SELECT status, attempt, lease_epoch, lease_fence, worker_id, retry_after \
             FROM work_items WHERE id = $1",
        )
        .bind(item)
        .fetch_optional(&self.base.pool())
        .await
        .unwrap_or_else(|e| panic!("read work item {item}: {e}"))?;
        Some(ItemRow {
            status: row.get("status"),
            attempt: u32::try_from(row.get::<i32, _>("attempt")).expect("attempt fits a u32"),
            lease_epoch: row.get("lease_epoch"),
            lease_fence: row.get("lease_fence"),
            worker_id: row.get("worker_id"),
            retry_after: row.get("retry_after"),
        })
    }

    async fn dead_letter_execution(&self, item: WorkItemId) -> Option<ExecutionId> {
        let execution: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT execution_id FROM dead_letter_items WHERE id = $1")
                .bind(item)
                .fetch_optional(&self.base.pool())
                .await
                .unwrap_or_else(|e| panic!("read the dead letter of {item}: {e}"));
        execution.map(ExecutionId)
    }

    async fn artifact_rows(&self, hash: &str, tenant: Option<&str>) -> u64 {
        let pool = self.base.pool();
        let count: i64 = match tenant {
            None => {
                sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE hash = $1")
                    .bind(hash)
                    .fetch_one(&pool)
                    .await
            }
            Some(tenant) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM artifacts WHERE hash = $1 AND tenant_id = $2",
                )
                .bind(hash)
                .bind(tenant)
                .fetch_one(&pool)
                .await
            }
        }
        .unwrap_or_else(|e| panic!("count artifact {hash}: {e}"));
        u64::try_from(count).expect("a count is not negative")
    }

    async fn set_token_tenant(&self, token_name: &str, tenant: &str) {
        let done = sqlx::query("UPDATE api_tokens SET tenant_id = $1 WHERE name = $2")
            .bind(tenant)
            .bind(token_name)
            .execute(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("move token {token_name} to {tenant}: {e}"));
        assert!(done.rows_affected() > 0, "no token named {token_name}");
    }

    async fn reopen(&self) -> Option<Arc<dyn StateBackend>> {
        let again = super::factories::connect_pg(&self.url).await;
        Some(match &self.scope {
            Some(tenant) => Arc::new(again.for_tenant(tenant.clone())),
            None => Arc::new(again),
        })
    }

    fn unscoped(&self) -> Arc<dyn StateBackend> {
        self.base.clone()
    }

    async fn dead_letter_tenant(&self, item: WorkItemId) -> Option<String> {
        sqlx::query_scalar("SELECT tenant_id FROM dead_letter_items WHERE id = $1")
            .bind(item)
            .fetch_optional(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("read the dead letter of {item}: {e}"))
    }

    async fn stored_token_hash(&self, token_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT token_hash FROM api_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_optional(&self.base.pool())
            .await
            .unwrap_or_else(|e| panic!("read token {token_id}: {e}"))
    }

    async fn insert_token_row(&self, id: &str, token_hash: &str, name: &str, role: &str) -> bool {
        // The columns an earlier build wrote; the tenant takes its default.
        let query = sqlx::query(
            "INSERT INTO api_tokens (id, token_hash, name, role, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(token_hash)
        .bind(name)
        .bind(role)
        .bind(Utc::now());
        self.update_one(&format!("insert token {id}"), query).await;
        true
    }

    async fn insert_event_json(
        &self,
        execution: &ExecutionId,
        sequence: i64,
        kind_json: &str,
    ) -> bool {
        // The two columns the store fills from the kind at insert, as a
        // later release would fill them for a kind of its own.
        let kind: serde_json::Value = serde_json::from_str(kind_json)
            .unwrap_or_else(|e| panic!("the event kind is JSON: {e}: {kind_json}"));
        let kind_type = kind["type"].as_str().unwrap_or_default().to_string();
        let node_id = kind["node_id"].as_str().map(str::to_string);
        let query = sqlx::query(
            "INSERT INTO events (id, execution_id, sequence, kind_type, node_id, kind_json, \
             created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(execution.0)
        .bind(sequence)
        .bind(kind_type)
        .bind(node_id)
        .bind(kind_json)
        .bind(Utc::now());
        self.update_one(&format!("insert event {sequence} of {execution}"), query)
            .await;
        true
    }

    async fn make_execution_unreadable(&self, execution: &ExecutionId) -> bool {
        // A timestamptz column cannot hold a bad time, as SQLite's text can;
        // a run's input that is not JSON does not read back either.
        let query = sqlx::query(
            "UPDATE workflow_executions SET initial_input = 'not json' WHERE execution_id = $1",
        )
        .bind(execution.0);
        self.update_one(&format!("damage run {execution}"), query)
            .await;
        true
    }

    async fn stored_json(&self, what: StoredJson) -> Option<String> {
        let pool = self.base.pool();
        let text: Option<String> = match &what {
            StoredJson::Event {
                execution,
                sequence,
            } => {
                sqlx::query_scalar(
                    "SELECT kind_json FROM events WHERE execution_id = $1 AND sequence = $2",
                )
                .bind(execution.0)
                .bind(sequence)
                .fetch_optional(&pool)
                .await
            }
            StoredJson::ItemPayload(item) => {
                sqlx::query_scalar("SELECT payload_json FROM work_items WHERE id = $1")
                    .bind(item)
                    .fetch_optional(&pool)
                    .await
            }
            StoredJson::RunInput(execution) => {
                sqlx::query_scalar(
                    "SELECT initial_input FROM workflow_executions WHERE execution_id = $1",
                )
                .bind(execution.0)
                .fetch_optional(&pool)
                .await
            }
        }
        .unwrap_or_else(|e| panic!("read {what:?}: {e}"));
        Some(text.unwrap_or_else(|| panic!("no row for {what:?}")))
    }

    async fn every_row(&self) -> Option<StoreRows> {
        let pool = self.base.pool();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema() \
             AND tablename <> '_sqlx_migrations' ORDER BY tablename",
        )
        .fetch_all(&pool)
        .await
        .unwrap_or_else(|e| panic!("list the tables: {e}"));
        assert_eq!(
            tables, POSTGRES_TABLES,
            "the schema's tables are not the ones every_row reads: add the new one to POSTGRES_TABLES"
        );
        let mut rows = StoreRows::new();
        for table in POSTGRES_TABLES {
            // `table` is one of the literal names above. Each row reads as
            // one JSON object, its columns in order.
            let mut texts: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT row_to_json(t)::text FROM {table} t"
            )))
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|e| panic!("read {table}: {e}"));
            texts.sort();
            rows.insert(table.to_string(), texts);
        }
        Some(rows)
    }
}

/// A lease expiry well in the past.
fn past() -> DateTime<Utc> {
    Utc::now() - chrono::Duration::days(1)
}

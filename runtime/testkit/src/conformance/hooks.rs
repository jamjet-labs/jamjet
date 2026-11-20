//! What a case needs from a store that the [`StateBackend`] trait does not
//! offer. A hook that writes panics when it changes nothing: one that
//! silently missed its row would make the case prove nothing.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::{
    InMemoryBackend, SqliteBackend, StateBackend, TenantId, WorkItem, WorkItemId, DEFAULT_TENANT,
};
use sqlx::sqlite::{Sqlite, SqliteArguments};
use sqlx::Row;
use std::sync::Arc;

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
}

/// A lease expiry well in the past.
fn past() -> DateTime<Utc> {
    Utc::now() - chrono::Duration::days(1)
}

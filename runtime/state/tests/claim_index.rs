//! A claim that names an execution looks its items up by
//! (execution_id, status); the index for it exists after migration.

use jamjet_state::SqliteBackend;
use sqlx::Row;

#[tokio::test]
async fn work_items_are_indexed_by_execution_and_status() {
    let b = SqliteBackend::open("sqlite::memory:").await.unwrap();
    let row = sqlx::query(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_work_items_execution_status'",
    )
    .fetch_optional(&b.pool())
    .await
    .unwrap()
    .expect("the index exists");
    let sql: String = row.get("sql");
    assert!(
        sql.contains("execution_id") && sql.contains("status"),
        "{sql}"
    );
}

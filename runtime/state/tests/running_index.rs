//! The scheduler walks running executions a page at a time, by
//! `execution_id` (`list_running_executions_after`). Each page must come
//! from an index in order, not from sorting every running row.

use jamjet_state::SqliteBackend;

async fn plan(pool: &sqlx::SqlitePool, sql: &str, binds: &[&str]) -> Vec<String> {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut q = sqlx::query_as::<_, (i64, i64, i64, String)>(&explain);
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.3)
        .collect()
}

#[tokio::test]
async fn the_running_walk_reads_an_index_in_order() {
    let path = std::env::temp_dir().join(format!("jj-running-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}", path.display());
    let backend = SqliteBackend::open(&url).await.unwrap();
    let pool = backend.pool();
    for (sql, binds, index) in [
        (
            "SELECT * FROM workflow_executions WHERE status = 'running' AND execution_id > ? \
             ORDER BY execution_id LIMIT 100",
            vec![""],
            "idx_executions_status_id",
        ),
        (
            "SELECT * FROM workflow_executions WHERE status = 'running' AND tenant_id = ? \
             AND execution_id > ? ORDER BY execution_id LIMIT 100",
            vec!["default", ""],
            "idx_executions_tenant_status_id",
        ),
    ] {
        let steps = plan(&pool, sql, &binds).await;
        assert!(
            !steps.iter().any(|s| s.contains("TEMP B-TREE")),
            "sorts in a temporary B-tree: {steps:?}"
        );
        assert!(
            steps.iter().any(|s| s.contains(index)),
            "does not use {index}: {steps:?}"
        );
    }
    let _ = std::fs::remove_file(path);
}

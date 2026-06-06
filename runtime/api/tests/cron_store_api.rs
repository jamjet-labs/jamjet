use jamjet_state::SqliteBackend;
use jamjet_timers::{CronJob, CronStore};
use uuid::Uuid;

#[tokio::test]
async fn cron_store_roundtrip() {
    let db = std::env::temp_dir().join(format!("jjcronapi-{}.db", Uuid::new_v4()));
    let url = format!("sqlite://{}", db.display());
    let backend = SqliteBackend::open(&url).await.unwrap();
    let store = CronStore::new(backend.pool());

    let next = jamjet_timers::cron_next("0 9 * * *", chrono::Utc::now()).unwrap();
    store
        .upsert(&CronJob {
            id: Uuid::new_v4(),
            name: "researcher".into(),
            cron_expression: "0 9 * * *".into(),
            workflow_id: "researcher".into(),
            workflow_version: "0.1.0".into(),
            input: serde_json::json!({}),
            enabled: true,
            last_run_at: None,
            next_run_at: next,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let all = store.list_all().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].workflow_id, "researcher");

    let _ = std::fs::remove_file(&db);
}

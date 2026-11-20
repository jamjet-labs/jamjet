//! The Postgres fixture's hooks read and write the rows they name. Checked
//! here with plain SQL, beside the store's own methods: the fixture cases
//! check the hooks through the backend, and on Postgres they can do that
//! only once its methods exist.

use chrono::{DateTime, TimeZone, Utc};
use jamjet_core::workflow::ExecutionId;
use jamjet_testkit::conformance::{PostgresFactory, StoreFactory, StoredJson};
use jamjet_testkit::pg::TestDatabase;
use uuid::Uuid;

/// A time Postgres keeps exactly (whole seconds).
fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_900_000_000 + secs, 0).unwrap()
}

#[tokio::test]
async fn the_postgres_hooks_read_and_write_their_rows() {
    let Some(fx) = PostgresFactory.fresh().await else {
        return jamjet_testkit::skip("the_postgres_hooks_read_and_write_their_rows");
    };
    let db = fx
        .keep
        .downcast_ref::<TestDatabase>()
        .expect("a Postgres fixture keeps its test database");
    let pool = db.pool().await;
    let hooks = &fx.hooks;

    let run = Uuid::new_v4();
    let item = Uuid::new_v4();
    let input = r#"{"input":"as written"}"#;
    let payload = r#"{"payload":"as written"}"#;
    let kind = r#"{"type":"node_skipped","node_id":"step","reason":"as written"}"#;
    sqlx::query(
        "INSERT INTO workflow_executions (execution_id, workflow_id, workflow_version, status, \
         initial_input, current_state, started_at, updated_at) \
         VALUES ($1, 'wf', '1', 'running', $2, '{}', now(), now())",
    )
    .bind(run)
    .bind(input)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO work_items (id, execution_id, node_id, queue_type, payload_json, created_at) \
         VALUES ($1, $2, 'step', 'general', $3, now())",
    )
    .bind(item)
    .bind(run)
    .bind(payload)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO events (id, execution_id, sequence, kind_type, node_id, kind_json, created_at) \
         VALUES ($1, $2, 1, 'node_skipped', 'step', $3, now())",
    )
    .bind(Uuid::new_v4())
    .bind(run)
    .bind(kind)
    .execute(&pool)
    .await
    .unwrap();

    // The item's row, its lease and its retry time.
    let row = hooks.item_row(item).await.expect("the item's row");
    assert_eq!(
        (
            row.status.as_str(),
            row.attempt,
            row.lease_epoch,
            row.lease_fence
        ),
        ("pending", 0, 0, 0)
    );
    assert_eq!((row.worker_id, row.retry_after), (None, None));
    sqlx::query(
        "UPDATE work_items SET status = 'claimed', worker_id = 'w', attempt = 2, \
         lease_epoch = 3, lease_fence = 4294967299, retry_after = $2 WHERE id = $1",
    )
    .bind(item)
    .bind(at(5))
    .execute(&pool)
    .await
    .unwrap();
    let row = hooks.item_row(item).await.unwrap();
    assert_eq!(
        (
            row.status.as_str(),
            row.attempt,
            row.lease_epoch,
            row.lease_fence
        ),
        ("claimed", 2, 3, 4_294_967_299)
    );
    assert_eq!(
        (row.worker_id.as_deref(), row.retry_after),
        (Some("w"), Some(at(5)))
    );
    assert_eq!(hooks.item_row(Uuid::new_v4()).await, None);

    let lease = || async {
        sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "SELECT lease_expires_at FROM work_items WHERE id = $1",
        )
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    hooks.set_lease_expiry(item, at(60)).await;
    assert_eq!(lease().await, Some(at(60)));
    hooks.force_lease_expired(item).await;
    assert!(lease().await.unwrap() < Utc::now());

    // Stored text, exactly.
    let execution = ExecutionId(run);
    assert_eq!(
        hooks
            .stored_json(StoredJson::RunInput(execution.clone()))
            .await,
        Some(input.to_string())
    );
    assert_eq!(
        hooks.stored_json(StoredJson::ItemPayload(item)).await,
        Some(payload.to_string())
    );
    assert_eq!(
        hooks
            .stored_json(StoredJson::Event {
                execution: execution.clone(),
                sequence: 1
            })
            .await,
        Some(kind.to_string())
    );

    // An event of a later release gets its derived columns from its JSON.
    let later = r#"{"type":"from_a_later_release","node_id":"other","extra":1}"#;
    assert!(hooks.insert_event_json(&execution, 2, later).await);
    let derived: (String, Option<String>, String) = sqlx::query_as(
        "SELECT kind_type, node_id, kind_json FROM events WHERE execution_id = $1 AND sequence = 2",
    )
    .bind(run)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        derived,
        (
            "from_a_later_release".to_string(),
            Some("other".to_string()),
            later.to_string()
        )
    );

    // Every row, every table.
    let rows = hooks.every_row().await.expect("Postgres keeps rows");
    assert_eq!(rows.len(), 14, "{:?}", rows.keys());
    assert_eq!(rows["events"].len(), 2);
    assert!(rows["workflow_executions"][0].contains(&run.to_string()));
    assert!(rows["work_items"][0].contains("as written"));
    assert_eq!(rows["tenants"].len(), 1);

    // The run's input no longer reads as JSON.
    assert!(hooks.make_execution_unreadable(&execution).await);
    let damaged: String =
        sqlx::query_scalar("SELECT initial_input FROM workflow_executions WHERE execution_id = $1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&damaged).is_err());

    // A dead letter names its run and its tenant.
    assert_eq!(hooks.dead_letter_execution(item).await, None);
    sqlx::query(
        "INSERT INTO dead_letter_items (id, execution_id, tenant_id, node_id, queue_type, \
         payload_json, attempt, last_error, created_at, dead_lettered_at) \
         VALUES ($1, $2, 'default', 'step', 'general', '{}', 3, 'gave up', now(), now())",
    )
    .bind(item)
    .bind(run)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        hooks.dead_letter_execution(item).await,
        Some(execution.clone())
    );
    assert_eq!(
        hooks.dead_letter_tenant(item).await.as_deref(),
        Some("default")
    );

    // Artifacts are counted per tenant.
    sqlx::query(
        "INSERT INTO artifacts (tenant_id, hash, bytes, size, created_at) \
         VALUES ('default', 'h1', '\\x0102', 2, now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(hooks.artifact_rows("h1", None).await, 1);
    assert_eq!(hooks.artifact_rows("h1", Some("default")).await, 1);
    assert_eq!(hooks.artifact_rows("h1", Some("nobody")).await, 0);
    assert_eq!(hooks.artifact_rows("h2", None).await, 0);

    // Tokens: a row as an earlier build wrote it, by any id, and moved.
    assert!(
        hooks
            .insert_token_row("tok-old", "hash-old", "old token", "operator")
            .await
    );
    assert_eq!(
        hooks.stored_token_hash("tok-old").await.as_deref(),
        Some("hash-old")
    );
    assert_eq!(hooks.stored_token_hash("no-such-token").await, None);
    sqlx::query("INSERT INTO tenants (id, name) VALUES ('acme', 'Acme')")
        .execute(&pool)
        .await
        .unwrap();
    hooks.set_token_tenant("old token", "acme").await;
    let tenant: String =
        sqlx::query_scalar("SELECT tenant_id FROM api_tokens WHERE id = 'tok-old'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tenant, "acme");

    // The term, and handles on the same database.
    assert_eq!(hooks.set_store_term_at_least(4).await, 4);
    assert_eq!(hooks.set_store_term_at_least(2).await, 4);
    assert!(hooks.reopen().await.is_some());
    let _unscoped = hooks.unscoped();
}

#[tokio::test]
async fn a_writing_hook_that_changes_nothing_panics() {
    let Some(fx) = PostgresFactory.fresh().await else {
        return jamjet_testkit::skip("a_writing_hook_that_changes_nothing_panics");
    };
    let hooks = fx.hooks.clone();
    let missed = tokio::spawn(async move { hooks.set_lease_expiry(Uuid::new_v4(), at(1)).await });
    assert!(missed.await.is_err(), "a lease set on no row passed");
    let hooks = fx.hooks.clone();
    let missed = tokio::spawn(async move { hooks.set_token_tenant("nobody", "default").await });
    assert!(
        missed.await.is_err(),
        "a token moved that does not exist passed"
    );
}

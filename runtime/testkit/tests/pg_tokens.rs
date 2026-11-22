//! Tokens and tenant rows on the Postgres store, where the suite's cases do
//! not reach.
//!
//! A token validates while it is neither revoked nor expired by the
//! database's clock, and each validation stamps its last use. One
//! `PgBackend` type is both the engine's unscoped store and a tenant's view,
//! as `SqliteBackend` and `TenantScopedSqliteBackend` are on SQLite: the
//! unscoped store reads tenants and, as `SqliteBackend` does, leaves
//! creating and changing them to a view, which may create or change any
//! tenant.

use chrono::{DateTime, Utc};
use jamjet_state::{
    PgBackend, PgSettings, StateBackend, StateBackendError, Tenant, TenantId, TenantStatus,
};
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use serde_json::json;

fn settings() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

fn tenant(id: &str, name: &str) -> Tenant {
    let now = Utc::now();
    Tenant {
        id: TenantId::from(id),
        name: name.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn the_unscoped_store_reads_tenants_and_leaves_their_writes_to_a_view() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip(
            "the_unscoped_store_reads_tenants_and_leaves_their_writes_to_a_view",
        );
    };
    let base = PgBackend::connect(db.url(), settings()).await.unwrap();

    for refused in [
        base.create_tenant(tenant("acme", "Acme")).await,
        base.update_tenant(tenant("default", "Renamed")).await,
    ] {
        match refused {
            Err(StateBackendError::Database(m)) if m == "tenant management not supported" => {}
            other => panic!("the unscoped store wrote a tenant: {other:?}"),
        }
    }
    let names: Vec<String> = base
        .list_tenants()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["Default"], "the refused writes changed nothing");

    // Any view writes any tenant, as the tenant routes do through the
    // default tenant's view.
    let view = base.for_tenant(TenantId::default());
    view.create_tenant(tenant("acme", "Acme")).await.unwrap();
    let mut changed = tenant("acme", "Acme Corp");
    changed.status = TenantStatus::Suspended;
    changed.policy = Some(json!({ "blocked_tools": ["rm_rf"] }));
    view.update_tenant(changed).await.unwrap();
    let missing = view.update_tenant(tenant("nobody", "Nobody")).await;
    assert!(
        matches!(missing, Err(StateBackendError::NotFound(_))),
        "{missing:?}"
    );

    let read = base
        .get_tenant(&TenantId::from("acme"))
        .await
        .unwrap()
        .expect("the unscoped store reads the view's tenant");
    assert_eq!(read.name, "Acme Corp");
    assert_eq!(read.status, TenantStatus::Suspended);
    assert_eq!(read.policy, Some(json!({ "blocked_tools": ["rm_rf"] })));
    assert!(base
        .get_tenant(&TenantId::from("nobody"))
        .await
        .unwrap()
        .is_none());
    let ids: Vec<String> = base
        .list_tenants()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id.0)
        .collect();
    assert_eq!(ids, ["default", "acme"], "oldest first");
}

/// A token validates, and its last use is stamped, until it is revoked. A
/// token whose expiry has passed by the database's clock does not validate;
/// one whose expiry is ahead does.
#[tokio::test]
async fn a_revoked_or_expired_token_does_not_validate() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_revoked_or_expired_token_does_not_validate");
    };
    let base = PgBackend::connect(db.url(), settings()).await.unwrap();
    let pool = db.pool().await;
    let last_used = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                "SELECT last_used_at FROM api_tokens WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let (revoked, info) = base.create_token("to-revoke", "operator").await.unwrap();
    assert!(last_used(info.id.clone()).await.is_none());
    let seen = base.validate_token(&revoked).await.unwrap().expect("valid");
    assert_eq!(seen.id, info.id);
    assert!(
        last_used(info.id.clone()).await.is_some(),
        "the use is stamped"
    );
    base.revoke_token(&info.id).await.unwrap().expect("revoked");
    assert!(
        base.validate_token(&revoked).await.unwrap().is_none(),
        "a revoked token validated"
    );

    let (expiring, info) = base.create_token("to-expire", "operator").await.unwrap();
    for (expiry, valid) in [
        ("- interval '1 second'", false),
        ("+ interval '1 hour'", true),
    ] {
        let sql = format!(
            "UPDATE api_tokens SET expires_at = statement_timestamp() {expiry} WHERE id = $1"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&info.id)
            .execute(&pool)
            .await
            .unwrap();
        let seen = base.validate_token(&expiring).await.unwrap();
        assert_eq!(seen.is_some(), valid, "expiry now {expiry}: {seen:?}");
        if let Some(seen) = seen {
            assert!(seen.expires_at.is_some_and(|at| at > Utc::now()));
        }
    }
}

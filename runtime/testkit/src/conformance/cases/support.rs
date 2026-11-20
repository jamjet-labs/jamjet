//! What several case modules share: tenant views and skipping a case on a
//! store it does not apply to.

use crate::conformance::{Fixture, StoreKind};
use jamjet_state::{StateBackend, Tenant, TenantId, TenantStatus};
use std::sync::Arc;

/// Whether the case does not apply to this store. `kinds` are the stores it
/// skips and `why` says why; the case returns when this answers true.
pub fn not_on(fx: &Fixture, kinds: &[StoreKind], why: &str) -> bool {
    let skip = kinds.contains(&fx.kind);
    if skip {
        skipped(fx, why);
    }
    skip
}

/// Say the case stops here on this store, and why. One line on stderr, so
/// a run with `--nocapture` shows what each store left out.
pub fn skipped(fx: &Fixture, why: &str) {
    eprintln!("not on {:?}: {why}", fx.kind);
}

/// For a case that needs tenant views, on a store that has none.
pub fn without_tenant_views(fx: &Fixture) {
    skipped(fx, "the store has no tenant views");
}

/// Whether a check is left out on this store because it fails there and
/// never ran there before the suite: a finding for the owner to decide, not
/// a porting error. `kinds` are the stores it is left out on.
pub fn finding(fx: &Fixture, kinds: &[StoreKind], what: &str) -> bool {
    let left_out = kinds.contains(&fx.kind);
    if left_out {
        eprintln!("finding on {:?}: {what}", fx.kind);
    }
    left_out
}

/// A tenant row for `id`, active.
pub fn tenant_row(id: &str) -> Tenant {
    let now = chrono::Utc::now();
    Tenant {
        id: TenantId::from(id),
        name: id.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    }
}

/// `tenant`'s view of the store, with its tenant row made first (the SQL
/// stores refuse an unknown tenant). `None` where the fixture has no
/// scoping.
pub async fn tenant_view(fx: &Fixture, tenant: &str) -> Option<Arc<dyn StateBackend>> {
    let scoper = fx.scoped.as_ref()?;
    let id = TenantId::from(tenant);
    let view = scoper(&id);
    if view.get_tenant(&id).await.unwrap().is_none() {
        view.create_tenant(tenant_row(tenant)).await.unwrap();
    }
    Some(view)
}

/// The backend a case runs on, seen through `tenant` where the fixture is
/// scoped, so the scoped code runs with a tenant other than the default.
/// Elsewhere, the fixture's backend.
pub async fn in_tenant(fx: &Fixture, tenant: &str) -> Arc<dyn StateBackend> {
    match tenant_view(fx, tenant).await {
        Some(view) => view,
        None => fx.backend.clone(),
    }
}

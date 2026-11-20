//! A factory per backend. Each fixture is a store no other case uses.

use super::hooks::{MemoryHooks, PgHooks, SqliteHooks};
use super::{Fixture, Scoper, StoreFactory, StoreKind};
use crate::pg::{TestDatabase, POOL_SIZE};
use async_trait::async_trait;
use jamjet_state::{InMemoryBackend, PgBackend, PgSettings, SqliteBackend, StateBackend, TenantId};
use std::path::PathBuf;
use std::sync::Arc;

/// A fresh [`InMemoryBackend`]. `second` is the same backend.
pub struct MemoryFactory;

#[async_trait]
impl StoreFactory for MemoryFactory {
    async fn fresh(&self) -> Option<Fixture> {
        let memory = Arc::new(InMemoryBackend::new());
        Some(Fixture {
            backend: memory.clone(),
            second: Some(memory.clone()),
            scoped: None,
            hooks: Arc::new(MemoryHooks(memory)),
            kind: StoreKind::Memory,
            keep: Box::new(()),
        })
    }
}

/// A fresh SQLite file in a temp directory, opened twice: `backend` and
/// `second` are two pools on it, as two engine processes would hold.
pub struct SqliteFactory;

#[async_trait]
impl StoreFactory for SqliteFactory {
    async fn fresh(&self) -> Option<Fixture> {
        let file = SqliteFile::new().await;
        Some(Fixture {
            backend: file.first.clone(),
            second: Some(file.second.clone()),
            scoped: None,
            hooks: Arc::new(SqliteHooks {
                base: file.first.clone(),
                url: file.url.clone(),
                scope: None,
            }),
            kind: StoreKind::Sqlite,
            keep: Box::new(file.dir),
        })
    }
}

/// As [`SqliteFactory`], seen through `TenantScopedSqliteBackend`: `backend`
/// and `second` are the default tenant's views on the two pools, and
/// `scoped` gives any tenant's view on the first.
pub struct SqliteScopedFactory;

#[async_trait]
impl StoreFactory for SqliteScopedFactory {
    async fn fresh(&self) -> Option<Fixture> {
        let file = SqliteFile::new().await;
        let tenant = TenantId::default();
        let first = file.first.clone();
        let scoped: Scoper = Arc::new(move |t: &TenantId| {
            Arc::new(first.for_tenant(t.clone())) as Arc<dyn StateBackend>
        });
        Some(Fixture {
            backend: Arc::new(file.first.for_tenant(tenant.clone())),
            second: Some(Arc::new(file.second.for_tenant(tenant.clone()))),
            scoped: Some(scoped),
            hooks: Arc::new(SqliteHooks {
                base: file.first.clone(),
                url: file.url.clone(),
                scope: Some(tenant),
            }),
            kind: StoreKind::SqliteScoped,
            keep: Box::new(file.dir),
        })
    }
}

/// A fresh Postgres database of its own, connected twice: `backend` and
/// `second` are two pools on it, as two engine instances would hold. `None`
/// when no Postgres server is configured.
pub struct PostgresFactory;

#[async_trait]
impl StoreFactory for PostgresFactory {
    async fn fresh(&self) -> Option<Fixture> {
        let db = PgDatabase::new().await?;
        Some(Fixture {
            backend: db.first.clone(),
            second: Some(db.second.clone()),
            scoped: None,
            hooks: Arc::new(PgHooks {
                base: db.first.clone(),
                url: db.keep.url().to_string(),
                scope: None,
            }),
            kind: StoreKind::Postgres,
            keep: Box::new(db.keep),
        })
    }
}

/// As [`PostgresFactory`], seen through a tenant: `backend` and `second` are
/// the default tenant's views on the two pools, and `scoped` gives any
/// tenant's view on the first.
pub struct PostgresScopedFactory;

#[async_trait]
impl StoreFactory for PostgresScopedFactory {
    async fn fresh(&self) -> Option<Fixture> {
        let db = PgDatabase::new().await?;
        let tenant = TenantId::default();
        let first = db.first.clone();
        let scoped: Scoper = Arc::new(move |t: &TenantId| {
            Arc::new(first.for_tenant(t.clone())) as Arc<dyn StateBackend>
        });
        Some(Fixture {
            backend: Arc::new(db.first.for_tenant(tenant.clone())),
            second: Some(Arc::new(db.second.for_tenant(tenant.clone()))),
            scoped: Some(scoped),
            hooks: Arc::new(PgHooks {
                base: db.first.clone(),
                url: db.keep.url().to_string(),
                scope: Some(tenant),
            }),
            kind: StoreKind::PostgresScoped,
            keep: Box::new(db.keep),
        })
    }
}

/// A test database holding the schema, and two pools on it.
struct PgDatabase {
    keep: TestDatabase,
    first: Arc<PgBackend>,
    second: Arc<PgBackend>,
}

impl PgDatabase {
    async fn new() -> Option<Self> {
        let keep = TestDatabase::fresh().await?;
        let first = Arc::new(connect_pg(keep.url()).await);
        let second = Arc::new(connect_pg(keep.url()).await);
        Some(Self {
            keep,
            first,
            second,
        })
    }
}

/// Connect to the test database at `url`, checking its schema as the
/// engine does, on a pool of at most [`POOL_SIZE`] connections.
pub(crate) async fn connect_pg(url: &str) -> PgBackend {
    let settings = PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    };
    PgBackend::connect(url, settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

/// A migrated SQLite file and two pools on it.
struct SqliteFile {
    dir: TempDir,
    url: String,
    first: Arc<SqliteBackend>,
    second: Arc<SqliteBackend>,
}

impl SqliteFile {
    async fn new() -> Self {
        let dir = TempDir::new();
        let url = format!("sqlite://{}", dir.0.join("state.db").display());
        let first = Arc::new(open(&url).await);
        let second = Arc::new(open(&url).await);
        Self {
            dir,
            url,
            first,
            second,
        }
    }
}

/// Open and migrate the SQLite file at `url`, creating it if needed.
pub(crate) async fn open(url: &str) -> SqliteBackend {
    SqliteBackend::open(url)
        .await
        .unwrap_or_else(|e| panic!("open the SQLite file at {url}: {e}"))
}

/// A directory under the system temp directory, removed on drop. The file
/// and its WAL live here, so nothing is left behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jamjet_conformance_{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

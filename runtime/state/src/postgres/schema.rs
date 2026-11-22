//! The Postgres schema: its migrations, where a database stands against
//! them, and applying them.

use crate::backend::{BackendResult, StateBackendError};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// The Postgres state migrations, `state/migrations_pg`.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations_pg");

/// Where a database's schema stands against [`MIGRATOR`], from its
/// `_sqlx_migrations` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// Every migration this build knows is applied, and nothing else.
    Current { version: i64 },
    /// No migration is applied.
    Empty,
    /// Some are applied, not all: `at` is the newest applied, `needs` the
    /// newest this build knows.
    Behind { at: i64, needs: i64 },
    /// A migration this build does not know is applied: a newer build
    /// migrated the database.
    Ahead { at: i64, knows: i64 },
    /// A migration started and did not finish.
    Dirty { version: i64 },
    /// An applied migration differs from this build's copy of it.
    Changed { version: i64 },
}

impl SchemaState {
    /// Why a store must not run on this schema, or `None` when it is current.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Current { .. } => None,
            Self::Empty => Some(
                "the Postgres database has no JamJet schema: run `jamjet-server migrate`".into(),
            ),
            Self::Behind { at, needs } => Some(format!(
                "the Postgres schema is at version {at} and this build needs {needs}: \
                 run `jamjet-server migrate`"
            )),
            Self::Ahead { at, knows } => Some(format!(
                "the Postgres schema is at version {at} and this build knows up to {knows}: \
                 this build is older than the database schema; do not roll back below a \
                 schema migration"
            )),
            Self::Dirty { version } => Some(format!(
                "Postgres schema migration {version} started and did not finish"
            )),
            Self::Changed { version } => Some(format!(
                "Postgres schema migration {version} in the database differs from this build's"
            )),
        }
    }
}

/// One row of `_sqlx_migrations`.
#[derive(Debug, Clone)]
struct Applied {
    version: i64,
    success: bool,
    checksum: Vec<u8>,
}

/// Where the database behind `pool` stands against [`MIGRATOR`]. Reads
/// only.
pub async fn schema_state(pool: &PgPool) -> BackendResult<SchemaState> {
    let applied = applied(pool).await?;
    let known: Vec<(i64, &[u8])> = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| (m.version, m.checksum.as_ref()))
        .collect();
    Ok(judge(&applied, &known))
}

/// The rows of `_sqlx_migrations`, none when the table does not exist.
async fn applied(pool: &PgPool) -> BackendResult<Vec<Applied>> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await
        .map_err(read_failed)?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows =
        sqlx::query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(read_failed)?;
    rows.iter()
        .map(|row| {
            Ok(Applied {
                version: row.try_get("version").map_err(read_failed)?,
                success: row.try_get("success").map_err(read_failed)?,
                checksum: row.try_get("checksum").map_err(read_failed)?,
            })
        })
        .collect()
}

/// Where `applied` stands against the migrations a build `known`s, as
/// `(version, checksum)`. A failed migration is reported first, then one
/// this build does not know, then one that changed.
fn judge(applied: &[Applied], known: &[(i64, &[u8])]) -> SchemaState {
    if let Some(dirty) = applied.iter().find(|a| !a.success) {
        return SchemaState::Dirty {
            version: dirty.version,
        };
    }
    let Some(at) = applied.iter().map(|a| a.version).max() else {
        return SchemaState::Empty;
    };
    let knows = known.iter().map(|(v, _)| *v).max().unwrap_or(0);
    let checksum_of = |version: i64| known.iter().find(|(v, _)| *v == version).map(|(_, c)| *c);
    if applied.iter().any(|a| checksum_of(a.version).is_none()) {
        return SchemaState::Ahead { at, knows };
    }
    if let Some(changed) = applied
        .iter()
        .find(|a| checksum_of(a.version) != Some(a.checksum.as_slice()))
    {
        return SchemaState::Changed {
            version: changed.version,
        };
    }
    if known
        .iter()
        .all(|(v, _)| applied.iter().any(|a| a.version == *v))
    {
        SchemaState::Current { version: at }
    } else {
        SchemaState::Behind { at, needs: knows }
    }
}

fn read_failed(e: sqlx::Error) -> StateBackendError {
    StateBackendError::Database(format!("cannot read the Postgres schema version: {e}"))
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// The newest migration applied now.
    pub version: i64,
    /// How many this run applied.
    pub applied: usize,
}

/// Apply every pending migration in [`MIGRATOR`] to the database at `url`,
/// on a connection of its own that is closed afterwards. sqlx's migrator
/// takes a session lock and does not release it when a migration fails;
/// closing the connection does.
///
/// The connection is a pool of one, closed when done: the migrator's future
/// on a bare `&mut PgConnection` is not `Send` for every lifetime, so a
/// caller in a `Send` future could not await it.
pub async fn migrate(url: &str) -> Result<MigrateReport, String> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .map_err(|e| format!("cannot connect to Postgres: {e}"))?;
    let outcome = migrate_on(&pool).await;
    pool.close().await;
    outcome
}

async fn migrate_on(pool: &PgPool) -> Result<MigrateReport, String> {
    let before = applied(pool).await.map_err(|e| e.to_string())?.len();
    MIGRATOR
        .run(pool)
        .await
        .map_err(|e| format!("migration failed: {e}"))?;
    let after = applied(pool).await.map_err(|e| e.to_string())?;
    Ok(MigrateReport {
        version: after.iter().map(|a| a.version).max().unwrap_or(0),
        applied: after.len().saturating_sub(before),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(version: i64, success: bool, checksum: &[u8]) -> Applied {
        Applied {
            version,
            success,
            checksum: checksum.to_vec(),
        }
    }

    const KNOWN: &[(i64, &[u8])] = &[(1, b"one"), (2, b"two")];

    #[test]
    fn judge_reads_each_state() {
        assert_eq!(judge(&[], KNOWN), SchemaState::Empty);
        assert_eq!(
            judge(&[row(1, true, b"one")], KNOWN),
            SchemaState::Behind { at: 1, needs: 2 }
        );
        assert_eq!(
            judge(&[row(1, true, b"one"), row(2, true, b"two")], KNOWN),
            SchemaState::Current { version: 2 }
        );
        assert_eq!(
            judge(
                &[
                    row(1, true, b"one"),
                    row(2, true, b"two"),
                    row(9, true, b"x")
                ],
                KNOWN
            ),
            SchemaState::Ahead { at: 9, knows: 2 }
        );
        assert_eq!(
            judge(&[row(1, true, b"one"), row(2, false, b"two")], KNOWN),
            SchemaState::Dirty { version: 2 }
        );
        assert_eq!(
            judge(&[row(1, true, b"changed"), row(2, true, b"two")], KNOWN),
            SchemaState::Changed { version: 1 }
        );
        // A failed migration from a newer build is dirty, not ahead.
        assert_eq!(
            judge(&[row(1, true, b"one"), row(9, false, b"x")], KNOWN),
            SchemaState::Dirty { version: 9 }
        );
    }

    #[test]
    fn only_a_current_schema_is_accepted() {
        assert_eq!(SchemaState::Current { version: 1 }.refusal(), None);
        for state in [
            SchemaState::Empty,
            SchemaState::Behind { at: 1, needs: 2 },
            SchemaState::Ahead { at: 9, knows: 2 },
            SchemaState::Dirty { version: 2 },
            SchemaState::Changed { version: 1 },
        ] {
            assert!(state.refusal().is_some(), "{state:?}");
        }
        assert!(SchemaState::Empty
            .refusal()
            .unwrap()
            .contains("jamjet-server migrate"));
        assert!(SchemaState::Ahead { at: 9, knows: 2 }
            .refusal()
            .unwrap()
            .contains("do not roll back"));
    }

    #[test]
    fn every_migration_is_an_up_migration_in_a_transaction() {
        assert!(MIGRATOR.iter().count() > 0);
        assert!(MIGRATOR
            .iter()
            .all(|m| !m.migration_type.is_down_migration() && !m.no_tx));
    }
}

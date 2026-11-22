//! The Postgres schema: the same tables and columns as SQLite's, every
//! migration in a transaction, and `schema_state` telling each state a
//! database can be in.

use jamjet_state::postgres::{migrate, schema_state, MigrateReport, SchemaState, MIGRATOR};
use jamjet_state::SqliteBackend;
use jamjet_testkit::pg::TestDatabase;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// SQLite tables Postgres has none of: nothing reads timers or cron jobs on
/// Postgres.
const SQLITE_ONLY: &[&str] = &["cron_jobs", "timers"];

/// Postgres tables SQLite's state store has none of: the audit log, which
/// SQLite keeps in a table the audit backend creates
/// ([`audit_log_matches_sqlite`]), its head and its outbox.
const POSTGRES_ONLY: &[&str] = &["audit_head", "audit_log", "audit_outbox"];

/// Columns only Postgres has: the engine fills them at insert from what
/// SQLite reads out of the JSON, and the claim order on the database clock.
const DERIVED: &[(&str, &str)] = &[
    ("events", "kind_type"),
    ("events", "node_id"),
    ("work_items", "by_execution"),
    ("work_items", "enqueued_at"),
];

type Columns = BTreeMap<String, BTreeSet<String>>;

/// A foreign key column: its table and column, and the table and column it
/// refers to.
type ForeignKey = (String, String, String, String);

#[tokio::test]
async fn schema_matches_sqlite() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("schema_matches_sqlite");
    };
    let sqlite = sqlite_columns().await;
    let postgres = postgres_columns(&db).await;

    for table in SQLITE_ONLY {
        assert!(sqlite.contains_key(*table), "SQLite has no table {table}");
        assert!(!postgres.contains_key(*table), "Postgres has {table} now");
    }
    for table in POSTGRES_ONLY {
        assert!(
            postgres.contains_key(*table),
            "Postgres has no table {table}"
        );
    }

    let mut expected: Columns = sqlite
        .into_iter()
        .filter(|(table, _)| !SQLITE_ONLY.contains(&table.as_str()))
        .collect();
    for (table, column) in DERIVED {
        let columns = expected
            .get_mut(*table)
            .unwrap_or_else(|| panic!("SQLite has no table {table}"));
        assert!(
            columns.insert(column.to_string()),
            "SQLite has {table}.{column} now"
        );
    }
    let shared: Columns = postgres
        .into_iter()
        .filter(|(table, _)| !POSTGRES_ONLY.contains(&table.as_str()))
        .collect();

    let tables: BTreeSet<&String> = expected.keys().chain(shared.keys()).collect();
    let mut differences = Vec::new();
    for table in tables {
        let empty = BTreeSet::new();
        let want = expected.get(table).unwrap_or(&empty);
        let have = shared.get(table).unwrap_or(&empty);
        let missing: Vec<&String> = want.difference(have).collect();
        let extra: Vec<&String> = have.difference(want).collect();
        if !missing.is_empty() || !extra.is_empty() {
            differences.push(format!(
                "{table}: missing on Postgres {missing:?}, only on Postgres {extra:?}"
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "the Postgres schema differs from SQLite's:\n{}",
        differences.join("\n")
    );
}

/// Every foreign key SQLite has, Postgres has too: a row SQLite refuses for
/// naming a run or a tenant that does not exist, Postgres refuses as well.
#[tokio::test]
async fn foreign_keys_match_sqlite() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("foreign_keys_match_sqlite");
    };
    let sqlite: BTreeSet<ForeignKey> = sqlite_store(|pool, tables| async move {
        let mut keys = BTreeSet::new();
        for table in tables {
            let found: Vec<(String, String, String)> =
                sqlx::query_as(r#"SELECT "from", "table", "to" FROM pragma_foreign_key_list(?)"#)
                    .bind(&table)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            for (column, references, to) in found {
                keys.insert((table.clone(), column, references, to));
            }
        }
        keys
    })
    .await
    .into_iter()
    .filter(|(table, ..)| !SQLITE_ONLY.contains(&table.as_str()))
    .collect();
    assert!(
        !sqlite.is_empty(),
        "the scan found no foreign key on SQLite"
    );

    let postgres: BTreeSet<ForeignKey> = sqlx::query_as(
        "SELECT cl.relname::text, a.attname::text, rcl.relname::text, ra.attname::text \
         FROM pg_constraint c \
         JOIN pg_class cl ON cl.oid = c.conrelid \
         JOIN pg_class rcl ON rcl.oid = c.confrelid \
         JOIN pg_namespace n ON n.oid = cl.relnamespace \
         CROSS JOIN LATERAL unnest(c.conkey, c.confkey) AS k(col, rcol) \
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.col \
         JOIN pg_attribute ra ON ra.attrelid = c.confrelid AND ra.attnum = k.rcol \
         WHERE c.contype = 'f' AND n.nspname = current_schema()",
    )
    .fetch_all(&db.pool().await)
    .await
    .unwrap()
    .into_iter()
    .filter(|(table, ..): &ForeignKey| !POSTGRES_ONLY.contains(&table.as_str()))
    .collect();

    let missing: Vec<&ForeignKey> = sqlite.difference(&postgres).collect();
    let extra: Vec<&ForeignKey> = postgres.difference(&sqlite).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "foreign keys (table, column, references, column) only on SQLite: {missing:#?}\n\
         only on Postgres: {extra:#?}"
    );
}

/// The Postgres audit log has the columns of SQLite's, and `position`,
/// which orders two entries of one instant as SQLite's `rowid` does.
#[tokio::test]
async fn audit_log_matches_sqlite() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("audit_log_matches_sqlite");
    };
    // One connection: each connection to `sqlite::memory:` is a database
    // of its own.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    jamjet_audit::SqliteAuditBackend::new(pool.clone())
        .migrate()
        .await
        .unwrap();
    let mut expected: BTreeSet<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('audit_log')")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert!(!expected.is_empty(), "SQLite has no audit_log");
    assert!(
        expected.insert("position".into()),
        "SQLite has position now"
    );
    let postgres = postgres_columns(&db)
        .await
        .remove("audit_log")
        .unwrap_or_default();
    assert_eq!(postgres, expected);
}

#[test]
fn no_migration_runs_outside_a_transaction() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../state/migrations_pg");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("a directory entry").path())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no migration in {}", dir.display());
    for file in files {
        let sql = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        assert!(
            !sql.contains("-- no-transaction"),
            "{} runs outside a transaction, so a failure part way leaves it half applied",
            file.display()
        );
    }
}

#[tokio::test]
async fn a_fresh_database_is_current() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_fresh_database_is_current");
    };
    assert_eq!(
        schema_state(&db.pool().await).await.unwrap(),
        SchemaState::Current { version: newest() }
    );
}

#[tokio::test]
async fn an_empty_database_is_empty() {
    let Some(db) = TestDatabase::empty().await else {
        return jamjet_testkit::skip("an_empty_database_is_empty");
    };
    assert_eq!(
        schema_state(&db.pool().await).await.unwrap(),
        SchemaState::Empty
    );
}

#[tokio::test]
async fn migrate_takes_an_empty_database_to_current_once() {
    let Some(db) = TestDatabase::empty().await else {
        return jamjet_testkit::skip("migrate_takes_an_empty_database_to_current_once");
    };
    let all = MIGRATOR.iter().count();
    assert_eq!(
        migrate(db.url()).await.unwrap(),
        MigrateReport {
            version: newest(),
            applied: all
        }
    );
    assert_eq!(
        migrate(db.url()).await.unwrap(),
        MigrateReport {
            version: newest(),
            applied: 0
        }
    );
    assert_eq!(
        schema_state(&db.pool().await).await.unwrap(),
        SchemaState::Current { version: newest() }
    );
}

#[tokio::test]
async fn a_fake_newer_row_is_ahead() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_fake_newer_row_is_ahead");
    };
    let pool = db.pool().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer build', true, '\\x00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        schema_state(&pool).await.unwrap(),
        SchemaState::Ahead {
            at: 9999,
            knows: newest()
        }
    );
}

#[tokio::test]
async fn a_dirty_row_is_dirty() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_dirty_row_is_dirty");
    };
    let pool = db.pool().await;
    let done = sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = $1")
        .bind(newest())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(done.rows_affected(), 1);
    assert_eq!(
        schema_state(&pool).await.unwrap(),
        SchemaState::Dirty { version: newest() }
    );
}

#[tokio::test]
async fn a_changed_checksum_is_changed() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_changed_checksum_is_changed");
    };
    let pool = db.pool().await;
    let done =
        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\xdeadbeef' WHERE version = 1")
            .execute(&pool)
            .await
            .unwrap();
    assert_eq!(done.rows_affected(), 1);
    assert_eq!(
        schema_state(&pool).await.unwrap(),
        SchemaState::Changed { version: 1 }
    );
}

/// The newest migration this build has.
fn newest() -> i64 {
    MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("the store has migrations")
}

/// Every column of every table of a freshly migrated SQLite store, by table.
async fn sqlite_columns() -> Columns {
    sqlite_store(|pool, tables| async move {
        let mut columns = Columns::new();
        for table in tables {
            let names: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                .bind(&table)
                .fetch_all(&pool)
                .await
                .unwrap();
            columns.insert(table, names.into_iter().collect());
        }
        columns
    })
    .await
}

/// `read` on a freshly migrated SQLite store in a temp directory, given its
/// pool and its tables (SQLite's own and the migration log left out). The
/// directory is removed afterwards.
async fn sqlite_store<T, F, Fut>(read: F) -> T
where
    F: FnOnce(sqlx::SqlitePool, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let dir = std::env::temp_dir().join(format!(
        "jamjet_pg_schema_{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}", dir.join("state.db").display());
    let store = SqliteBackend::open(&url)
        .await
        .unwrap_or_else(|e| panic!("open {url}: {e}"));
    let pool = store.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite%' AND name != '_sqlx_migrations'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let found = read(pool.clone(), tables).await;
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
    found
}

/// Every column of every table in the test database's schema, by table.
async fn postgres_columns(db: &TestDatabase) -> Columns {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name <> '_sqlx_migrations'",
    )
    .fetch_all(&db.pool().await)
    .await
    .unwrap();
    let mut columns = Columns::new();
    for (table, column) in rows {
        columns.entry(table).or_default().insert(column);
    }
    columns
}

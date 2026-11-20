//! A fresh Postgres database per test.
//!
//! Tests read an admin URL from `JAMJET_TEST_POSTGRES_URL`;
//! `runtime/scripts/pg-test.sh up` starts a local server and prints it.
//! Without the URL a test skips. With `JAMJET_REQUIRE_POSTGRES_TESTS=1`, as
//! in CI, a missing URL is a failure. A URL that is set but cannot be
//! reached is always a failure.
//!
//! Each [`TestDatabase`] is a copy of a template database that holds the
//! schema, made once per schema version and kept on the server: copying a
//! template is much faster than migrating each test's database. Nothing
//! cleans up after a run that crashed, so two checkouts testing against one
//! server never drop each other's databases; `pg-test.sh reset` clears
//! what is left.

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use std::str::FromStr;

/// The admin URL tests create their databases with.
pub const URL_ENV: &str = "JAMJET_TEST_POSTGRES_URL";
/// Set to `1` to make a missing [`URL_ENV`] a failure instead of a skip.
pub const REQUIRE_ENV: &str = "JAMJET_REQUIRE_POSTGRES_TESTS";

/// The most connections a test pool holds.
pub const POOL_SIZE: u32 = 3;

/// Held while a template is made, so two test binaries never make one at
/// once. "JJTEST" in ASCII.
const TEMPLATE_LOCK: i64 = 0x4a4a_5445_5354;

/// The admin URL, or `None` when it is not set and nobody requires it.
///
/// # Panics
///
/// When [`REQUIRE_ENV`] is set and the URL is not.
pub fn admin_url() -> Option<String> {
    match std::env::var(URL_ENV) {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ if required() => panic!(
            "{REQUIRE_ENV} is set but {URL_ENV} is not set; \
             runtime/scripts/pg-test.sh up prints it"
        ),
        _ => None,
    }
}

/// Any value but empty or `0` requires Postgres.
fn required() -> bool {
    std::env::var(REQUIRE_ENV).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Note that a case skipped because Postgres is not configured. The reason
/// is printed once per test binary, past the test harness's output capture
/// so a plain `cargo test` shows it; the case name only with `--nocapture`.
pub fn skip(case: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "skipped: {URL_ENV} not set (runtime/scripts/pg-test.sh up prints it)"
        );
    });
    eprintln!("skipped {case}");
}

/// A database of its own for one test. Dropping it drops the database,
/// closing any connection still open on it.
pub struct TestDatabase {
    name: String,
    url: String,
    admin: String,
}

impl TestDatabase {
    /// A new database on the server in [`URL_ENV`], holding the schema.
    /// `None` when the URL is not set and not required.
    ///
    /// # Panics
    ///
    /// When [`REQUIRE_ENV`] is set and the URL is not, and when the server
    /// cannot be reached or refuses to create the database.
    pub async fn fresh() -> Option<TestDatabase> {
        let admin = admin_url()?;
        let template = template(&admin).await;
        Some(create(admin, Some(template)).await)
    }

    /// A new database with no schema at all, for tests of what the store
    /// does before it is migrated. `None` as for [`Self::fresh`].
    ///
    /// # Panics
    ///
    /// As [`Self::fresh`].
    pub async fn empty() -> Option<TestDatabase> {
        let admin = admin_url()?;
        Some(create(admin, None).await)
    }

    /// The database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A URL for this database, with the admin URL's user and password.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Options for connecting to this database.
    pub fn connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::from_str(&self.url).expect("a test database URL parses")
    }

    /// A pool on this database of at most [`POOL_SIZE`] connections.
    pub async fn pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(POOL_SIZE)
            .connect_with(self.connect_options())
            .await
            .unwrap_or_else(|e| panic!("connect to test database {}: {e}", self.name))
    }
}

impl Drop for TestDatabase {
    /// Drops the database from a thread with its own runtime: the test's
    /// runtime may be a current-thread one, which cannot block on a future.
    /// A failure is reported, not raised, since a panic here while the test
    /// is already failing would abort the binary.
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async {
                let mut conn = PgConnection::connect(&admin)
                    .await
                    .map_err(|e| e.to_string())?;
                sqlx::raw_sql(AssertSqlSafe(format!(
                    r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#
                )))
                .execute(&mut conn)
                .await
                .map_err(|e| e.to_string())?;
                let _ = conn.close().await;
                Ok::<(), String>(())
            })
        })
        .join();
        let error = match dropped {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e,
            Err(_) => "the dropping thread panicked".to_string(),
        };
        eprintln!(
            "could not drop test database {}: {error}; runtime/scripts/pg-test.sh reset clears it",
            self.name
        );
    }
}

/// A database of its own on the server at `admin`, a copy of `template`, or
/// of the server's default template when there is none.
async fn create(admin: String, template: Option<&str>) -> TestDatabase {
    let name = ident(format!("jj_t_{}", uuid::Uuid::new_v4().simple()));
    let mut conn = connect_admin(&admin).await;
    let sql = match template {
        Some(template) => format!(r#"CREATE DATABASE "{name}" TEMPLATE "{template}""#),
        None => format!(r#"CREATE DATABASE "{name}""#),
    };
    // Both names passed `ident`.
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("create test database {name}: {e}"));
    let _ = conn.close().await;
    TestDatabase {
        url: with_database(&admin, &name),
        name,
        admin,
    }
}

/// What a fresh database holds before a test touches it: the Postgres state
/// store's schema.
fn schema() -> &'static Migrator {
    &jamjet_state::postgres::MIGRATOR
}

/// The template for [`schema`], made by the first test in this binary that
/// asks for a database.
async fn template(admin: &str) -> &'static str {
    static TEMPLATE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    TEMPLATE.get_or_init(|| make_template(admin)).await
}

/// `jj_tmpl_<12 hex of the schema's checksum>`, made if the server does not
/// have it yet. It is built under another name and renamed when migrated,
/// so a run that dies halfway never leaves a template that looks finished.
async fn make_template(admin: &str) -> String {
    let migrator = schema();
    let name = ident(format!("jj_tmpl_{}", &schema_checksum(migrator)[..12]));
    let mut conn = connect_admin(admin).await;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK)
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("take the template lock: {e}"));
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&name)
            .fetch_one(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("look for template {name}: {e}"));
    if !exists {
        let building = ident(format!("{name}_new_{}", uuid::Uuid::new_v4().simple()));
        sqlx::raw_sql(AssertSqlSafe(format!(r#"CREATE DATABASE "{building}""#)))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("create template {building}: {e}"));
        // `migrate` closes its connection when done: a template must have
        // none when it is renamed or copied.
        jamjet_state::postgres::migrate(&with_database(admin, &building))
            .await
            .unwrap_or_else(|e| panic!("migrate template {building}: {e}"));
        sqlx::raw_sql(AssertSqlSafe(format!(
            r#"ALTER DATABASE "{building}" RENAME TO "{name}""#
        )))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("rename template {building} to {name}: {e}"));
    }
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK)
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("release the template lock: {e}"));
    let _ = conn.close().await;
    name
}

/// SHA-256 over each migration's version and checksum, in order.
fn schema_checksum(migrator: &Migrator) -> String {
    let mut bytes = Vec::new();
    for m in migrator.iter() {
        bytes.extend_from_slice(&m.version.to_le_bytes());
        bytes.extend_from_slice(&m.checksum);
    }
    jamjet_state::sha256_hex(&bytes)
}

async fn connect_admin(admin: &str) -> PgConnection {
    PgConnection::connect(admin)
        .await
        .unwrap_or_else(|e| panic!("connect to the server in {URL_ENV}: {e}"))
}

/// A database name goes into SQL text, since DDL cannot bind it. Every name
/// here is a fixed prefix and hex digits; refuse anything else.
fn ident(name: String) -> String {
    assert!(
        name.len() <= 63
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "not a test database name: {name:?}"
    );
    name
}

/// `url` with its database replaced by `database`, keeping any query.
fn with_database(url: &str, database: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (url, None),
    };
    let authority = head.find("://").map_or(0, |i| i + 3);
    let base = match head[authority..].find('/') {
        Some(i) => &head[..authority + i],
        None => head,
    };
    match query {
        Some(query) => format!("{base}/{database}?{query}"),
        None => format!("{base}/{database}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_database_replaces_only_the_database() {
        assert_eq!(
            with_database("postgres://u:p@127.0.0.1:55440/postgres", "jj_t_1"),
            "postgres://u:p@127.0.0.1:55440/jj_t_1"
        );
        assert_eq!(
            with_database("postgres://u:p@host/postgres?sslmode=disable", "jj_t_1"),
            "postgres://u:p@host/jj_t_1?sslmode=disable"
        );
        assert_eq!(
            with_database("postgres://u:p@host:5432", "jj_t_1"),
            "postgres://u:p@host:5432/jj_t_1"
        );
    }

    #[test]
    fn the_template_name_follows_the_schema() {
        // SHA-256 of no bytes: the name of a template with no migrations.
        let empty = Migrator::with_migrations(Vec::new());
        assert_eq!(&schema_checksum(&empty)[..12], "e3b0c44298fc");
        assert!(schema().iter().count() > 0, "the store has migrations");
        assert_ne!(schema_checksum(schema()), schema_checksum(&empty));
    }

    #[test]
    #[should_panic(expected = "not a test database name")]
    fn a_name_with_a_quote_is_refused() {
        ident("jj_t_\"x".to_string());
    }
}

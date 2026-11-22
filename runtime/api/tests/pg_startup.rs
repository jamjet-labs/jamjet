//! `jamjet-server` on Postgres, as a process: the schema check at start,
//! the `migrate` command, cron refused, and tokens issued from the command
//! line.
//!
//! A server refuses a schema it does not match: an empty or older one until
//! `jamjet-server migrate` runs, a newer one (a rollback below a schema
//! migration), and one a migration left half done. `migrate` applies what is
//! pending once, however many run at once, and `migrate --check` reports
//! where the schema stands without changing it.
//!
//! Each case runs the real binary in its own temp directory with a cleared
//! environment, against a database of its own. Without
//! `JAMJET_TEST_POSTGRES_URL` the cases skip.

use jamjet_state::postgres::{migrate_with, MigrateError, SchemaState, MIGRATE_LOCK, MIGRATOR};
use jamjet_testkit::pg::TestDatabase;
use sqlx::PgPool;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_jamjet-server");
/// How long a process may take to exit or to listen.
const WAIT: Duration = Duration::from_secs(60);
/// The whole case, so a hang fails instead of eating the run.
const CASE: Duration = Duration::from_secs(240);

// ── Processes ────────────────────────────────────────────────────────────────

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jjpg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // `dotenvy` walks up to the first `.env`; an empty one stops it there.
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// The binary with only `PATH`, `HOME` and `TMPDIR` inherited, run in
/// `dir`, on `url`, with a small pool.
fn server(dir: &Path, url: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir).env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("RUST_LOG", "info")
        .env("DATABASE_URL", url)
        .env("JAMJET_PG_POOL_SIZE", "3")
        .env("JAMJET_BIND", "127.0.0.1")
        .env("JAMJET_PORT", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// How a process that ran to the end ended.
struct Exit {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Exit {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Wait for `child` to exit; a process still running after [`WAIT`] is
/// killed and fails the case.
fn finish(mut child: Child, what: &str) -> Exit {
    let start = Instant::now();
    // Read the pipes while waiting, so a full pipe cannot stall the child.
    let out = reader(child.stdout.take());
    let err = reader(child.stderr.take());
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Exit {
                code: status.code(),
                stdout: out.join().unwrap(),
                stderr: err.join().unwrap(),
            };
        }
        if start.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{what} did not exit within {WAIT:?}: {}{}",
                out.join().unwrap(),
                err.join().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn reader(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    })
}

/// Run `jamjet-server <args>` on `url` to the end.
fn run(dir: &Path, url: &str, args: &[&str], env: &[(&str, &str)]) -> Exit {
    let child = server(dir, url, args, env).spawn().unwrap();
    finish(child, &format!("jamjet-server {}", args.join(" ")))
}

/// A server that is listening, killed when dropped.
struct Engine {
    child: Child,
    addr: String,
    log: String,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a server on `url` and wait until it listens. Fails the case with
/// what it said if it exits or stays silent.
fn serve(dir: &Path, url: &str, env: &[(&str, &str)]) -> Engine {
    let mut child = server(dir, url, &[], env).spawn().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut log = String::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                log.push_str(&line);
                log.push('\n');
                if let Some(rest) = line.split("listening on ").nth(1) {
                    let addr: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
                        .collect();
                    return Engine { child, addr, log };
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the server never listened: {log}");
}

/// The status code of `GET path` on `addr`, with `token` if given.
fn get(addr: &str, path: &str, token: Option<&str>) -> u16 {
    request(addr, "GET", path, token, None).0
}

/// `method path` on `addr` with `token` and a JSON `body` if given: the
/// status code and the response body.
fn request(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    let code = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP answer: {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

// ── Databases ────────────────────────────────────────────────────────────────

/// The password in a Postgres URL.
fn password_of(url: &str) -> String {
    let rest = url.split_once("://").unwrap().1;
    let userinfo = rest.rsplit_once('@').unwrap().0;
    let password = userinfo.split_once(':').unwrap().1;
    assert!(password.len() >= 4, "a password the checks can look for");
    password.to_string()
}

/// No password from `url` in anything the process printed.
fn no_password(url: &str, said: &str) {
    let password = password_of(url);
    assert!(
        !said.contains(&password),
        "the password was printed: {said}"
    );
}

/// The versions `_sqlx_migrations` records, with how many rows each has.
async fn recorded(pool: &PgPool) -> Vec<(i64, i64)> {
    sqlx::query_as(
        "SELECT version, count(*) FROM _sqlx_migrations GROUP BY version ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn known_versions() -> Vec<i64> {
    MIGRATOR.iter().map(|m| m.version).collect()
}

fn newest() -> i64 {
    *known_versions().last().unwrap()
}

/// What a successful `migrate` prints.
fn migrated_line(applied: usize) -> String {
    format!("schema at version {} ({applied} applied)", newest())
}

/// An empty database, `None` (skipped) without a Postgres URL.
async fn empty(case: &str) -> Option<TestDatabase> {
    let db = TestDatabase::empty().await;
    if db.is_none() {
        jamjet_testkit::skip(case);
    }
    db
}

/// A database migrated by `jamjet-server migrate`.
async fn migrated(case: &str, dir: &Path) -> Option<TestDatabase> {
    let db = empty(case).await?;
    let out = run(dir, db.url(), &["migrate"], &[]);
    assert_eq!(out.code, Some(0), "migrate: {}", out.all());
    Some(db)
}

/// Record a migration this build does not know, as a newer build would.
async fn make_ahead(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer build', true, '\\x00', 0)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The version [`make_dirty`] records as started and not finished.
const DIRTY_VERSION: i64 = 20_991_231;

async fn make_dirty(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'half done', false, '\\x00', 0)",
    )
    .bind(DIRTY_VERSION)
    .execute(pool)
    .await
    .unwrap();
}

/// The database is left at version 1 only, as an older build left it.
async fn make_older(db: &TestDatabase) {
    let pool = db.pool().await;
    MIGRATOR.run_to(1, &pool).await.unwrap();
    pool.close().await;
    assert!(newest() > 1, "there is a version after 1 to be behind on");
}

/// Sessions on this database waiting for `migrate`'s own lock.
async fn waiting_for_the_migrate_lock(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks \
         WHERE locktype = 'advisory' AND NOT granted \
           AND classid = $1 AND objid = $2 AND objsubid = 1 \
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .bind((MIGRATE_LOCK >> 32) as i32 as i64)
    .bind((MIGRATE_LOCK & 0xffff_ffff) as u32 as i64)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Advisory locks any session holds on this database.
async fn advisory_locks_held(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks \
         WHERE locktype = 'advisory' AND granted \
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn within_budget(case: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(CASE, case)
        .await
        .expect("the case ran past its time budget");
}

// ── The schema check at start ────────────────────────────────────────────────

/// An empty database is refused with what to run; after `migrate` the same
/// server starts, says the store is a preview, and logs no password.
#[tokio::test]
async fn server_refuses_an_empty_database_until_migrated() {
    within_budget(async {
        let Some(db) = empty("server_refuses_an_empty_database_until_migrated").await else {
            return;
        };
        let dir = temp_dir();
        let refused = finish(
            server(&dir, db.url(), &[], &[]).spawn().unwrap(),
            "a server on an empty database",
        );
        assert_ne!(refused.code, Some(0), "{}", refused.all());
        assert!(
            refused.all().contains("run `jamjet-server migrate`"),
            "{}",
            refused.all()
        );
        no_password(db.url(), &refused.all());

        let out = run(&dir, db.url(), &["migrate"], &[]);
        assert_eq!(out.code, Some(0), "{}", out.all());

        let engine = serve(&dir, db.url(), &[]);
        assert!(
            engine.log.contains("Postgres store (preview)"),
            "{}",
            engine.log
        );
        assert!(
            engine.log.contains(r#""storage":"postgres""#),
            "{}",
            engine.log
        );
        assert!(
            engine.log.contains("Audit outbox drain started"),
            "{}",
            engine.log
        );
        no_password(db.url(), &engine.log);
        assert_eq!(get(&engine.addr, "/health", None), 200);
        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// A database an older build migrated (version 1 only) is refused until
/// `migrate` brings it up.
#[tokio::test]
async fn server_refuses_an_older_schema() {
    within_budget(async {
        let Some(db) = empty("server_refuses_an_older_schema").await else {
            return;
        };
        make_older(&db).await;
        let dir = temp_dir();
        let refused = finish(
            server(&dir, db.url(), &[], &[]).spawn().unwrap(),
            "a server on an older schema",
        );
        assert_ne!(refused.code, Some(0), "{}", refused.all());
        assert!(
            refused.all().contains("at version 1")
                && refused.all().contains("run `jamjet-server migrate`"),
            "{}",
            refused.all()
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// A database a newer build migrated is refused: this build rolled back
/// below a schema migration.
#[tokio::test]
async fn server_refuses_a_newer_schema() {
    within_budget(async {
        let dir = temp_dir();
        let Some(db) = migrated("server_refuses_a_newer_schema", &dir).await else {
            return;
        };
        make_ahead(&db.pool().await).await;
        let refused = finish(
            server(&dir, db.url(), &[], &[]).spawn().unwrap(),
            "a server on a newer schema",
        );
        assert_ne!(refused.code, Some(0), "{}", refused.all());
        assert!(
            refused.all().contains(
                "this build is older than the database schema; do not roll back below a schema migration"
            ),
            "{}",
            refused.all()
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// A migration that started and did not finish is refused, naming it.
#[tokio::test]
async fn server_refuses_a_dirty_schema() {
    within_budget(async {
        let dir = temp_dir();
        let Some(db) = migrated("server_refuses_a_dirty_schema", &dir).await else {
            return;
        };
        make_dirty(&db.pool().await).await;
        let refused = finish(
            server(&dir, db.url(), &[], &[]).spawn().unwrap(),
            "a server on a dirty schema",
        );
        assert_ne!(refused.code, Some(0), "{}", refused.all());
        assert!(
            refused.all().contains(&format!(
                "migration {DIRTY_VERSION} started and did not finish"
            )),
            "{}",
            refused.all()
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

// ── migrate ──────────────────────────────────────────────────────────────────

/// The first run applies every migration; the second applies none and
/// still succeeds.
#[tokio::test]
async fn migrate_twice_is_a_no_op() {
    within_budget(async {
        let Some(db) = empty("migrate_twice_is_a_no_op").await else {
            return;
        };
        let dir = temp_dir();
        let first = run(&dir, db.url(), &["migrate"], &[]);
        assert_eq!(first.code, Some(0), "{}", first.all());
        assert!(
            first
                .stdout
                .contains(&migrated_line(known_versions().len())),
            "{}",
            first.all()
        );
        let second = run(&dir, db.url(), &["migrate"], &[]);
        assert_eq!(second.code, Some(0), "{}", second.all());
        assert!(
            second.stdout.contains(&migrated_line(0)),
            "{}",
            second.all()
        );
        let pool = db.pool().await;
        let once: Vec<(i64, i64)> = known_versions().into_iter().map(|v| (v, 1)).collect();
        assert_eq!(recorded(&pool).await, once);
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// Two `migrate` processes on one empty database: both wait on migrate's
/// lock (the test holds it until both do), both succeed, one applies every
/// migration and the other none, and each version is recorded once.
#[tokio::test]
async fn two_migrators_at_once_apply_each_migration_once() {
    within_budget(async {
        let Some(db) = empty("two_migrators_at_once_apply_each_migration_once").await else {
            return;
        };
        let dir = temp_dir();
        let pool = db.pool().await;
        let mut holder = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATE_LOCK)
            .execute(&mut *holder)
            .await
            .unwrap();
        let a = server(&dir, db.url(), &["migrate"], &[]).spawn().unwrap();
        let b = server(&dir, db.url(), &["migrate"], &[]).spawn().unwrap();

        // The race opened: both are inside migrate, waiting on its lock.
        let deadline = Instant::now() + WAIT;
        while waiting_for_the_migrate_lock(&pool).await < 2 {
            assert!(
                Instant::now() < deadline,
                "the two migrators never both waited on migrate's lock"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATE_LOCK)
            .execute(&mut *holder)
            .await
            .unwrap();
        drop(holder);

        let (a, b) = (finish(a, "migrator a"), finish(b, "migrator b"));
        assert_eq!(a.code, Some(0), "{}", a.all());
        assert_eq!(b.code, Some(0), "{}", b.all());
        let all = migrated_line(known_versions().len());
        let none = migrated_line(0);
        let said = [a.stdout.trim(), b.stdout.trim()];
        assert!(
            said == [all.as_str(), none.as_str()] || said == [none.as_str(), all.as_str()],
            "one applies everything, the other nothing: {said:?}"
        );
        let once: Vec<(i64, i64)> = known_versions().into_iter().map(|v| (v, 1)).collect();
        assert_eq!(recorded(&pool).await, once);
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// A migration that fails leaves the schema at the version before it, with
/// no row for the one that failed, and frees the lock: nothing is held
/// afterwards and the next migrator runs at once.
#[tokio::test]
async fn a_failed_migration_leaves_the_previous_version_and_frees_the_lock() {
    within_budget(async {
        let Some(db) =
            empty("a_failed_migration_leaves_the_previous_version_and_frees_the_lock").await
        else {
            return;
        };
        let source = temp_dir();
        std::fs::write(
            source.join("0001_fine.sql"),
            "CREATE TABLE fine_table (id int);",
        )
        .unwrap();
        std::fs::write(
            source.join("0002_fails.sql"),
            "CREATE TABLE never_table (id int);\nSELECT 1 / 0;",
        )
        .unwrap();
        let migrator = sqlx::migrate::Migrator::new(source.as_path())
            .await
            .unwrap();

        let failed = migrate_with(db.url(), &migrator)
            .await
            .expect_err("the second migration fails");
        assert!(failed.to_string().contains("division by zero"), "{failed}");

        let pool = db.pool().await;
        assert_eq!(recorded(&pool).await, vec![(1, 1)]);
        let never: bool = sqlx::query_scalar("SELECT to_regclass('never_table') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!never, "the failed migration left nothing behind");
        assert_eq!(advisory_locks_held(&pool).await, 0, "no lock is left held");

        // The next one does not wait for a lock the first kept.
        let again =
            tokio::time::timeout(Duration::from_secs(10), migrate_with(db.url(), &migrator))
                .await
                .expect("the second migrator ran at once");
        assert!(again.is_err(), "the same migration fails again");
        assert_eq!(recorded(&pool).await, vec![(1, 1)]);
        let _ = std::fs::remove_dir_all(&source);
    })
    .await;
}

/// A migrate refused on a newer schema closes its connection too, so the
/// lock it took before reading the schema is free when it returns.
#[tokio::test]
async fn a_refused_migration_frees_the_lock() {
    within_budget(async {
        let Some(db) = empty("a_refused_migration_frees_the_lock").await else {
            return;
        };
        make_older(&db).await;
        let pool = db.pool().await;
        make_ahead(&pool).await;
        let refused = migrate_with(db.url(), &MIGRATOR).await;
        assert!(
            matches!(
                refused,
                Err(MigrateError::Refused(SchemaState::Ahead { .. }))
            ),
            "{refused:?}"
        );
        assert_eq!(advisory_locks_held(&pool).await, 0, "no lock is left held");
    })
    .await;
}

/// `migrate --check` changes nothing and exits 0 current, 2 behind, 3 ahead,
/// 4 dirty or changed. `migrate` itself exits 3 on a newer schema and 4 on a
/// dirty or changed one, and applies nothing there.
#[tokio::test]
async fn migrate_check_exit_codes() {
    within_budget(async {
        let Some(db) = empty("migrate_check_exit_codes").await else {
            return;
        };
        let dir = temp_dir();
        let check = |url: &str| run(&dir, url, &["migrate", "--check"], &[]);

        // Empty: behind, and the check created nothing.
        let out = check(db.url());
        assert_eq!(out.code, Some(2), "{}", out.all());
        let pool = db.pool().await;
        let table: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!table, "--check changed nothing");

        // Older: behind.
        let older = TestDatabase::empty().await.unwrap();
        make_older(&older).await;
        let out = check(older.url());
        assert_eq!(out.code, Some(2), "{}", out.all());
        assert_eq!(recorded(&older.pool().await).await, vec![(1, 1)]);

        // Current.
        let out = run(&dir, db.url(), &["migrate"], &[]);
        assert_eq!(out.code, Some(0), "{}", out.all());
        let out = check(db.url());
        assert_eq!(out.code, Some(0), "{}", out.all());

        // Ahead: 3, and migrate refuses with 3 too.
        let ahead = TestDatabase::fresh().await.unwrap();
        make_ahead(&ahead.pool().await).await;
        let out = check(ahead.url());
        assert_eq!(out.code, Some(3), "{}", out.all());
        let out = run(&dir, ahead.url(), &["migrate"], &[]);
        assert_eq!(out.code, Some(3), "{}", out.all());
        assert!(out.all().contains("do not roll back"), "{}", out.all());

        // Dirty: 4, both.
        let dirty = TestDatabase::fresh().await.unwrap();
        make_dirty(&dirty.pool().await).await;
        let out = check(dirty.url());
        assert_eq!(out.code, Some(4), "{}", out.all());
        let out = run(&dir, dirty.url(), &["migrate"], &[]);
        assert_eq!(out.code, Some(4), "{}", out.all());
        assert!(
            out.all().contains(&DIRTY_VERSION.to_string()),
            "{}",
            out.all()
        );

        // Changed: 4, both, naming the version.
        let changed = TestDatabase::fresh().await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1")
            .execute(&changed.pool().await)
            .await
            .unwrap();
        let out = check(changed.url());
        assert_eq!(out.code, Some(4), "{}", out.all());
        let out = run(&dir, changed.url(), &["migrate"], &[]);
        assert_eq!(out.code, Some(4), "{}", out.all());
        assert!(out.all().contains("migration 1"), "{}", out.all());
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// Neither `migrate` nor `migrate --check` prints the URL's password, when
/// it works and when the password is wrong.
#[tokio::test]
async fn migrate_never_prints_the_password() {
    within_budget(async {
        let Some(db) = empty("migrate_never_prints_the_password").await else {
            return;
        };
        let dir = temp_dir();
        for args in [&["migrate"][..], &["migrate", "--check"]] {
            let out = run(&dir, db.url(), args, &[("RUST_LOG", "debug")]);
            assert_eq!(out.code, Some(0), "{args:?}: {}", out.all());
            no_password(db.url(), &out.all());
        }
        let password = password_of(db.url());
        let wrong = db
            .url()
            .replacen(&format!(":{password}@"), ":not-the-password-7f3a@", 1);
        for args in [&["migrate"][..], &["migrate", "--check"]] {
            let out = run(&dir, &wrong, args, &[("RUST_LOG", "debug")]);
            assert_ne!(out.code, Some(0), "{args:?}: {}", out.all());
            no_password(&wrong, &out.all());
        }
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

// ── Cron and tokens ──────────────────────────────────────────────────────────

/// `JAMJET_EMBED_CRON=1` stops a server on Postgres: every engine would
/// fire every schedule. In dev mode cron is skipped with a warning and
/// `/cron` answers 400, as on the memory store.
#[tokio::test]
async fn postgres_refuses_embedded_cron() {
    within_budget(async {
        let dir = temp_dir();
        let Some(db) = migrated("postgres_refuses_embedded_cron", &dir).await else {
            return;
        };
        let refused = finish(
            server(&dir, db.url(), &[], &[("JAMJET_EMBED_CRON", "1")])
                .spawn()
                .unwrap(),
            "a server with embedded cron on Postgres",
        );
        assert_ne!(refused.code, Some(0), "{}", refused.all());
        assert!(
            refused.all().contains(
                "cron is not supported on Postgres yet: each engine would fire every schedule"
            ),
            "{}",
            refused.all()
        );

        let engine = serve(&dir, db.url(), &[("JAMJET_DEV_MODE", "1")]);
        assert!(
            engine.log.contains("cron is not supported on Postgres yet"),
            "{}",
            engine.log
        );
        assert!(
            !engine.log.contains("Cron scheduler started"),
            "{}",
            engine.log
        );
        assert_eq!(get(&engine.addr, "/cron", None), 400);
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

/// A token issued by `jamjet-server token create` on Postgres is accepted by
/// two engines on that database, is sealed into the audit log, and a
/// revoke from the command line is refused by both from then on.
#[tokio::test]
async fn token_created_on_postgres_works_on_two_engines() {
    within_budget(async {
        let dir = temp_dir();
        let Some(db) = migrated("token_created_on_postgres_works_on_two_engines", &dir).await
        else {
            return;
        };
        let key = [("JAMJET_AUDIT_SIGNING_KEY", "pg-startup-test-key")];
        let created = run(
            &dir,
            db.url(),
            &["token", "create", "--role", "developer", "--name", "ci"],
            &key,
        );
        assert_eq!(created.code, Some(0), "{}", created.all());
        no_password(db.url(), &created.all());
        let token = created.stdout.trim().to_string();
        assert!(token.starts_with("jj_"), "{token:?}");

        let one = serve(&dir, db.url(), &key);
        let two = serve(&dir, db.url(), &key);
        for engine in [&one, &two] {
            assert_eq!(get(&engine.addr, "/executions", Some(&token)), 200);
            assert_eq!(get(&engine.addr, "/executions", None), 401);
        }

        // Both engines keep agents in the database: one registers, the
        // other lists it.
        let uri = "https://agents.example/researcher";
        let card = serde_json::json!({
            "id": uri, "uri": uri, "name": "researcher", "description": "research agent",
            "version": "1.0.0",
            "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
            "autonomy": "bounded_autonomous", "constraints": null, "auth": { "type": "none" }
        });
        let (st, body) = request(&one.addr, "POST", "/agents", Some(&token), Some(&card));
        assert_eq!(st, 201, "{body}");
        let (st, body) = request(&two.addr, "GET", "/agents", Some(&token), None);
        assert_eq!(st, 200, "{body}");
        assert!(body.contains(uri), "the other engine lists the agent: {body}");

        let pool = db.pool().await;
        let id: String = sqlx::query_scalar("SELECT id FROM api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        let revoked = run(&dir, db.url(), &["token", "revoke", &id], &key);
        assert_eq!(revoked.code, Some(0), "{}", revoked.all());
        no_password(db.url(), &revoked.all());
        for engine in [&one, &two] {
            assert_eq!(get(&engine.addr, "/executions", Some(&token)), 401);
        }

        let sealed: Vec<String> =
            sqlx::query_scalar("SELECT event_type FROM audit_log ORDER BY position")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(sealed, ["token_issued", "token_revoked"], "{sealed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

// ── The outbox drain ─────────────────────────────────────────────────────────

/// A sealed event whose seal never landed (its outbox row is a minute old)
/// is sealed by a server that starts on the database: the drain runs at
/// startup and every 2 s, on the server's pool.
#[tokio::test]
async fn a_server_seals_what_the_outbox_holds() {
    use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
    use jamjet_state::event::{Event, EventKind};
    use jamjet_state::{PgBackend, PgSettings, StateBackend};

    within_budget(async {
        let dir = temp_dir();
        let Some(db) = migrated("a_server_seals_what_the_outbox_holds", &dir).await else {
            return;
        };
        let settings = PgSettings {
            pool_size: 2,
            ..PgSettings::default()
        };
        let pg = PgBackend::connect(db.url(), settings).await.unwrap();
        let now = chrono::Utc::now();
        let eid = ExecutionId::new();
        pg.create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf-unsealed".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
        let event = Event::new(
            eid,
            0,
            EventKind::ToolApprovalRequired {
                node_id: "pay".into(),
                tool_name: "pay".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        );
        let event_id = event.id;
        pg.append_event(event).await.unwrap();
        let pool = pg.pool();
        sqlx::query("UPDATE audit_outbox SET created_at = created_at - interval '1 minute'")
            .execute(&pool)
            .await
            .unwrap();
        let entries = || async {
            sqlx::query_scalar::<_, String>("SELECT actor_id FROM audit_log WHERE event_id = $1")
                .bind(event_id)
                .fetch_all(&pool)
                .await
                .unwrap()
        };
        assert!(entries().await.is_empty(), "nothing sealed it yet");

        let _engine = serve(&dir, db.url(), &[]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while entries().await.is_empty() {
            assert!(
                Instant::now() < deadline,
                "the server never sealed the outbox row"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(entries().await, ["system"], "sealed once, by the drain");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&dir);
    })
    .await;
}

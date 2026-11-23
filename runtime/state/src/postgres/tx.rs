//! Every Postgres transaction runs through [`with_tx`], which decides what an
//! error means and whether the whole transaction runs again.
//!
//! | Error | Retried | Answer |
//! |---|---|---|
//! | 40P01 deadlock, 40001 serialization failure | 3 attempts in all, 10 ms then 40 ms apart, with jitter | `Unavailable` after the last |
//! | 55P03 lock timeout | 2 attempts in all | `Unavailable` |
//! | the connection lost before `COMMIT` was sent (I/O, 08xxx, 57P01 to 57P03) | 3 attempts in all, on a new connection | `Unavailable` |
//! | any error from `COMMIT` itself | never: the outcome is unknown | `Unavailable("commit outcome unknown")` |
//! | 57014 statement timeout, 25P03 idle-in-transaction timeout | no | `Unavailable` |
//! | no free connection in time, the pool closed | no | `Unavailable` |
//! | 23503 foreign key | no | `NotFound` |
//! | 23505 unique (on the event sequence, logged at error: a locking bug) | no | `Database` |
//! | 22021, a NUL in text ([`no_nul`] refuses it before sending) | no | `Database("text contains a NUL character")` |
//! | anything else, and every error the closure returns itself | no | `Database`, or the closure's error |
//!
//! A retry runs the closure again from `BEGIN`, so a closure must have no
//! effect outside its transaction. A closure returns every statement's error
//! with `?`: Postgres answers the `COMMIT` of a transaction whose statement
//! failed with a rollback and no error, so a swallowed error would look like
//! a write that landed.
//!
//! Each attempt starts at the fault point `pg_tx.attempt#1`, where a test
//! counts the attempts a call made, and each retry passes `pg_tx.retry#1`
//! first, where a test counts the retries.

use crate::backend::{BackendResult, StateBackendError};
use rand::RngExt as _;
use sqlx::{PgPool, Postgres, Transaction};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// What a [`with_tx`] closure returns.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The error of a [`with_tx`] closure: a statement's error, which
/// [`classify`] judges, or the closure's own answer, which goes back to the
/// caller as it is. Both convert with `?`.
#[derive(Debug)]
pub(crate) enum TxError {
    Sql(sqlx::Error),
    Backend(StateBackendError),
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<StateBackendError> for TxError {
    fn from(e: StateBackendError) -> Self {
        Self::Backend(e)
    }
}

/// What to do after an error.
#[derive(Debug)]
pub(crate) enum Retry {
    /// Run the transaction again from `BEGIN`, up to `attempts` in all.
    Again { attempts: u32 },
    /// Answer with this error.
    Never(StateBackendError),
}

/// The answer to any error from `COMMIT`.
pub(crate) const COMMIT_OUTCOME_UNKNOWN: &str = "commit outcome unknown";

/// The answer to a NUL in a text value.
pub(crate) const NUL_IN_TEXT: &str = "text contains a NUL character";

/// The unique constraint on `events (execution_id, sequence)`. Under the run
/// lock no two transactions take one sequence, so a violation is a bug.
const EVENT_SEQUENCE_KEY: &str = "events_execution_id_sequence_key";

/// The wait after each failed attempt, before jitter.
const BACKOFF: [Duration; 2] = [Duration::from_millis(10), Duration::from_millis(40)];

/// What `e` means, by the table in this module's doc. `at_commit` is true
/// when `e` came from `COMMIT`.
pub(crate) fn classify(e: &sqlx::Error, at_commit: bool) -> Retry {
    use StateBackendError::{Database, NotFound, Unavailable};
    if at_commit {
        return Retry::Never(Unavailable(COMMIT_OUTCOME_UNKNOWN.into()));
    }
    match e {
        sqlx::Error::Io(_) => Retry::Again { attempts: 3 },
        sqlx::Error::PoolTimedOut => Retry::Never(Unavailable(
            "no Postgres connection was free within the acquire timeout".into(),
        )),
        sqlx::Error::PoolClosed => Retry::Never(Unavailable("the Postgres pool is closed".into())),
        sqlx::Error::Database(db) => {
            let code = db.code().unwrap_or_default();
            match code.as_ref() {
                "40P01" | "40001" => Retry::Again { attempts: 3 },
                "55P03" => Retry::Again { attempts: 2 },
                "57P01" | "57P02" | "57P03" => Retry::Again { attempts: 3 },
                c if c.starts_with("08") => Retry::Again { attempts: 3 },
                "57014" | "25P03" => {
                    Retry::Never(Unavailable(format!("{} ({code})", db.message())))
                }
                "23503" => Retry::Never(NotFound(db.message().to_string())),
                "22021" => Retry::Never(Database(NUL_IN_TEXT.into())),
                "23505" => {
                    if db.constraint() == Some(EVENT_SEQUENCE_KEY) {
                        tracing::error!(
                            error = %e,
                            "two writers took one event sequence; the run lock did not hold"
                        );
                    }
                    Retry::Never(Database(e.to_string()))
                }
                _ => Retry::Never(Database(e.to_string())),
            }
        }
        _ => Retry::Never(Database(e.to_string())),
    }
}

/// The answer to a statement's error outside any transaction, as a read
/// runs: what [`classify`] says, and `Unavailable` where it would retry.
/// Public so the Postgres audit log answers the same errors the same way.
pub fn statement_error(e: sqlx::Error) -> StateBackendError {
    match classify(&e, false) {
        Retry::Never(answer) => answer,
        Retry::Again { .. } => StateBackendError::Unavailable(e.to_string()),
    }
}

/// Refuse `v` if it holds a NUL, which Postgres text cannot store. `what`
/// names the value in the log line.
pub(crate) fn no_nul(what: &str, v: &str) -> BackendResult<()> {
    if v.contains('\0') {
        tracing::warn!(what, "refused a text value with a NUL character");
        return Err(StateBackendError::Database(NUL_IN_TEXT.into()));
    }
    Ok(())
}

/// Run `f` in a transaction on `pool` and commit, retrying the whole
/// transaction as [`classify`] says. `op` names the call in log lines.
///
/// The transaction's lifetime `'a` is the caller's, so the closure can
/// borrow what the caller holds. Each attempt calls `f` afresh.
pub(crate) async fn with_tx<'a, T, F>(pool: &PgPool, op: &'static str, mut f: F) -> BackendResult<T>
where
    F: for<'t> FnMut(&'t mut Transaction<'a, Postgres>) -> BoxFuture<'t, Result<T, TxError>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        attempt_starts().await?;
        let failed = match pool.begin().await {
            Err(e) => e,
            Ok(tx) => {
                let mut tx: Transaction<'a, Postgres> = tx;
                match f(&mut tx).await {
                    Ok(value) => {
                        return match tx.commit().await {
                            Ok(()) => Ok(value),
                            Err(e) => {
                                tracing::warn!(
                                    op,
                                    error = %e,
                                    "a Postgres commit failed; its outcome is unknown"
                                );
                                Err(answer(classify(&e, true), &e, attempt))
                            }
                        };
                    }
                    Err(TxError::Backend(e)) => {
                        let _ = tx.rollback().await;
                        return Err(e);
                    }
                    Err(TxError::Sql(e)) => {
                        let _ = tx.rollback().await;
                        e
                    }
                }
            }
        };
        match classify(&failed, false) {
            Retry::Again { attempts } if attempt < attempts => {
                tracing::debug!(op, attempt, error = %failed, "retrying a Postgres transaction");
                retry_starts().await?;
                tokio::time::sleep(backoff(attempt)).await;
            }
            retry => return Err(answer(retry, &failed, attempt)),
        }
    }
}

/// The seam at the start of each attempt of a transaction, before `BEGIN`.
/// Not a point inside a transaction: nothing has run when it fires.
async fn attempt_starts() -> BackendResult<()> {
    fault_point!("pg_tx.attempt", 1);
    Ok(())
}

/// The seam before each retry of a transaction, after the attempt that
/// failed was rolled back, where a test counts the retries a load made.
async fn retry_starts() -> BackendResult<()> {
    fault_point!("pg_tx.retry", 1);
    Ok(())
}

/// The error to answer with when no attempt is left.
fn answer(retry: Retry, e: &sqlx::Error, attempts: u32) -> StateBackendError {
    match retry {
        Retry::Never(answer) => answer,
        Retry::Again { .. } => {
            StateBackendError::Unavailable(format!("{e} (gave up after {attempts} attempts)"))
        }
    }
}

/// The wait after failed attempt number `attempt`: the backoff step, from
/// half of it to one and a half times it.
fn backoff(attempt: u32) -> Duration {
    let step = BACKOFF[(attempt as usize - 1).min(BACKOFF.len() - 1)];
    let half = step / 2;
    half + Duration::from_micros(rand::rng().random_range(0..=step.as_micros() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    const URL_ENV: &str = "JAMJET_TEST_POSTGRES_URL";
    const REQUIRE_ENV: &str = "JAMJET_REQUIRE_POSTGRES_TESTS";

    /// One connection to the server in `JAMJET_TEST_POSTGRES_URL`, so the
    /// session's temp objects are there for every attempt: a table
    /// `landed`, a function `pg_temp.fail(code)` that raises an error with
    /// that SQLSTATE, and a table `fails_at_commit` whose inserts make the
    /// commit fail. `None` when no server is configured; a missing URL fails
    /// when `JAMJET_REQUIRE_POSTGRES_TESTS` is set.
    async fn session() -> Option<PgPool> {
        let url = match std::env::var(URL_ENV) {
            Ok(url) if !url.trim().is_empty() => url,
            _ => {
                let required = std::env::var(REQUIRE_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
                assert!(
                    !required,
                    "{REQUIRE_ENV} is set but {URL_ENV} is not set; \
                     runtime/scripts/pg-test.sh up prints it"
                );
                eprintln!("skipped: {URL_ENV} not set (runtime/scripts/pg-test.sh up prints it)");
                return None;
            }
        };
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap_or_else(|e| panic!("connect to the server in {URL_ENV}: {e}"));
        sqlx::raw_sql(
            "CREATE TEMP TABLE landed (attempt int NOT NULL);
             CREATE FUNCTION pg_temp.fail(code text) RETURNS void LANGUAGE plpgsql AS
               $$ BEGIN RAISE EXCEPTION 'synthetic error %', code USING ERRCODE = code; END $$;
             CREATE TEMP TABLE fails_at_commit (x int);
             CREATE FUNCTION pg_temp.fail_at_commit() RETURNS trigger LANGUAGE plpgsql AS
               $$ BEGIN RAISE EXCEPTION 'synthetic error at commit' USING ERRCODE = '40P01'; END $$;
             CREATE CONSTRAINT TRIGGER fail_at_commit AFTER INSERT ON fails_at_commit
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION pg_temp.fail_at_commit();",
        )
        .execute(&pool)
        .await
        .expect("make the session's test objects");
        Some(pool)
    }

    async fn landed(pool: &PgPool) -> Vec<i32> {
        sqlx::query_scalar("SELECT attempt FROM landed ORDER BY attempt")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// A `with_tx` whose attempt `n` inserts `n` into `landed`, then raises
    /// the SQLSTATE `fail(n)` names, if any. Returns the answer and how many
    /// attempts ran.
    async fn attempts(
        pool: &PgPool,
        fail: impl Fn(u32) -> Option<&'static str>,
    ) -> (BackendResult<u32>, u32) {
        let calls = AtomicU32::new(0);
        let out = with_tx(pool, "test", |tx| {
            let n = calls.fetch_add(1, SeqCst) + 1;
            let code = fail(n);
            Box::pin(async move {
                sqlx::query("INSERT INTO landed (attempt) VALUES ($1)")
                    .bind(n as i32)
                    .execute(&mut **tx)
                    .await?;
                if let Some(code) = code {
                    sqlx::query("SELECT pg_temp.fail($1)")
                        .bind(code)
                        .execute(&mut **tx)
                        .await?;
                }
                Ok(n)
            })
        })
        .await;
        (out, calls.load(SeqCst))
    }

    #[tokio::test]
    async fn retry_retries_a_deadlock_then_lands_once() {
        let Some(pool) = session().await else { return };
        let (out, calls) = attempts(&pool, |n| (n == 1).then_some("40P01")).await;
        assert_eq!(out.unwrap(), 2);
        assert_eq!(calls, 2);
        assert_eq!(
            landed(&pool).await,
            vec![2],
            "only the second attempt landed"
        );
    }

    #[tokio::test]
    async fn retry_gives_up_after_three() {
        let Some(pool) = session().await else { return };
        let (out, calls) = attempts(&pool, |_| Some("40P01")).await;
        assert!(
            matches!(&out, Err(StateBackendError::Unavailable(m)) if m.contains("3 attempts")),
            "{out:?}"
        );
        assert_eq!(calls, 3);
        assert!(landed(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_serialization_failure_is_retried_too() {
        let Some(pool) = session().await else { return };
        let (out, calls) = attempts(&pool, |n| (n < 3).then_some("40001")).await;
        assert_eq!(out.unwrap(), 3);
        assert_eq!(calls, 3);
        assert_eq!(landed(&pool).await, vec![3]);
    }

    #[tokio::test]
    async fn a_lock_timeout_is_tried_twice() {
        let Some(pool) = session().await else { return };
        let (out, calls) = attempts(&pool, |_| Some("55P03")).await;
        assert!(
            matches!(&out, Err(StateBackendError::Unavailable(_))),
            "{out:?}"
        );
        assert_eq!(calls, 2);
        assert!(landed(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn a_statement_timeout_is_not_retried() {
        let Some(pool) = session().await else { return };
        let (out, calls) = attempts(&pool, |_| Some("57014")).await;
        assert!(
            matches!(&out, Err(StateBackendError::Unavailable(_))),
            "{out:?}"
        );
        assert_eq!(calls, 1);
        assert!(landed(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn retry_never_retries_after_commit() {
        let Some(pool) = session().await else { return };
        let calls = AtomicU32::new(0);
        let out = with_tx(&pool, "test", |tx| {
            calls.fetch_add(1, SeqCst);
            Box::pin(async move {
                sqlx::query("INSERT INTO landed (attempt) VALUES (1)")
                    .execute(&mut **tx)
                    .await?;
                // Deferred: this insert's trigger raises a deadlock at COMMIT.
                sqlx::query("INSERT INTO fails_at_commit (x) VALUES (1)")
                    .execute(&mut **tx)
                    .await?;
                Ok(())
            })
        })
        .await;
        match out {
            Err(StateBackendError::Unavailable(m)) => assert_eq!(m, COMMIT_OUTCOME_UNKNOWN),
            other => panic!("expected Unavailable(commit outcome unknown), got {other:?}"),
        }
        assert_eq!(calls.load(SeqCst), 1, "a failed commit is never run again");
        assert!(landed(&pool).await.is_empty(), "this commit did fail");
    }

    #[tokio::test]
    async fn the_closures_own_error_is_answered_as_it_is() {
        let Some(pool) = session().await else { return };
        let calls = AtomicU32::new(0);
        let out: BackendResult<()> = with_tx(&pool, "test", |tx| {
            calls.fetch_add(1, SeqCst);
            Box::pin(async move {
                sqlx::query("INSERT INTO landed (attempt) VALUES (1)")
                    .execute(&mut **tx)
                    .await?;
                Err(StateBackendError::NotFound("run r".into()).into())
            })
        })
        .await;
        assert!(
            matches!(&out, Err(StateBackendError::NotFound(m)) if m == "run r"),
            "{out:?}"
        );
        assert_eq!(calls.load(SeqCst), 1);
        assert!(landed(&pool).await.is_empty(), "rolled back");
    }

    #[tokio::test]
    async fn classify_follows_the_table() {
        let Some(pool) = session().await else { return };
        let again = |n| Some(n);
        let never = None;
        for (code, attempts) in [
            ("40P01", again(3)),
            ("40001", again(3)),
            ("55P03", again(2)),
            ("08006", again(3)),
            ("57P01", again(3)),
            ("57P02", again(3)),
            ("57P03", again(3)),
            ("57014", never),
            ("25P03", never),
            ("23503", never),
            ("23505", never),
            ("22021", never),
            ("42P01", never),
        ] {
            let e = sqlx::query("SELECT pg_temp.fail($1)")
                .bind(code)
                .execute(&pool)
                .await
                .unwrap_err();
            let retry = classify(&e, false);
            match (attempts, &retry) {
                (Some(n), Retry::Again { attempts }) => assert_eq!(*attempts, n, "{code}"),
                (None, Retry::Never(answer)) => {
                    let ok = match code {
                        "57014" | "25P03" => matches!(answer, StateBackendError::Unavailable(_)),
                        "23503" => matches!(answer, StateBackendError::NotFound(_)),
                        "22021" => {
                            matches!(answer, StateBackendError::Database(m) if m == NUL_IN_TEXT)
                        }
                        _ => matches!(answer, StateBackendError::Database(_)),
                    };
                    assert!(ok, "{code}: {answer:?}");
                }
                _ => panic!("{code}: {retry:?}"),
            }
            assert!(
                matches!(
                    classify(&e, true),
                    Retry::Never(StateBackendError::Unavailable(m)) if m == COMMIT_OUTCOME_UNKNOWN
                ),
                "{code} at commit"
            );
        }
    }

    #[test]
    fn classify_judges_the_pool_and_the_socket() {
        let io = sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(matches!(classify(&io, false), Retry::Again { attempts: 3 }));
        for e in [sqlx::Error::PoolTimedOut, sqlx::Error::PoolClosed] {
            assert!(
                matches!(
                    classify(&e, false),
                    Retry::Never(StateBackendError::Unavailable(_))
                ),
                "{e}"
            );
        }
        assert!(matches!(
            classify(&sqlx::Error::RowNotFound, false),
            Retry::Never(StateBackendError::Database(_))
        ));
    }

    #[test]
    fn no_nul_refuses_a_nul_and_nothing_else() {
        assert!(no_nul("node id", "step").is_ok());
        assert!(no_nul("node id", "").is_ok());
        assert!(matches!(
            no_nul("node id", "st\0ep"),
            Err(StateBackendError::Database(m)) if m == NUL_IN_TEXT
        ));
    }

    #[test]
    fn backoff_waits_about_10_then_40_ms() {
        for _ in 0..100 {
            let first = backoff(1);
            assert!(first >= Duration::from_millis(5) && first <= Duration::from_millis(15));
            let second = backoff(2);
            assert!(second >= Duration::from_millis(20) && second <= Duration::from_millis(60));
        }
    }
}

//! Postgres-backed audit log: one chain for every engine on the database.
//!
//! The `audit_head` row names the newest entry. Every append locks that row
//! first, so appends from any number of engines take turns and each links
//! onto the entry before it. [`AuditBackend::append_sealed`] seals the entry
//! while it holds the lock, so a writer the others keep beating to the head
//! still gets its turn. Three constraints hold the chain even against a
//! writer that skips the lock: one entry per `prev_hash`, one entry with
//! none (the genesis), and one entry per event, so a second seal of one
//! event adds nothing.
//!
//! The tables come with the state store's Postgres migrations
//! (`0002_audit_agents.sql`), so this backend has no migrate of its own.
//!
//! As on SQLite, nothing here updates or deletes an `audit_log` row. There
//! is no `purge_expired`: deleting old entries would cut the chain, and
//! nothing calls it.

use crate::backend::{AuditBackend, AuditError, AuditQuery, SealOntoHead};
use crate::entry::AuditLogEntry;
use crate::sqlite::parse_actor_type;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use jamjet_state::fault_point;
use sqlx::postgres::PgArguments;
use sqlx::{PgPool, Postgres};
use uuid::Uuid;

pub struct PgAuditBackend {
    pool: PgPool,
}

impl PgAuditBackend {
    /// An audit log on `pool`, a database the state store's migrations
    /// have run on.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

const INSERT_SQL: &str =
    "INSERT INTO audit_log (id, event_id, execution_id, sequence, event_type, \
     actor_id, actor_type, tool_call_hash, policy_decision, http_request_id, http_method, \
     http_path, ip_address, created_at, raw_event, tenant_id, expires_at, redacted, prev_hash, \
     entry_hash, signature) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
     $19, $20, $21) \
     ON CONFLICT (event_id) DO NOTHING RETURNING position";

const SELECT_SQL: &str = "SELECT id, event_id, execution_id, sequence, event_type, actor_id, \
     actor_type, tool_call_hash, policy_decision, http_request_id, http_method, http_path, \
     ip_address, created_at, raw_event, tenant_id, expires_at, redacted, prev_hash, entry_hash, \
     signature FROM audit_log";

/// The head and the database's time, the head locked until the
/// transaction ends.
const LOCK_HEAD_SQL: &str =
    "SELECT entry_hash, created_at, statement_timestamp() FROM audit_head WHERE id = 1 FOR UPDATE";

const HEAD_AT_SQL: &str = "SELECT entry_hash, created_at FROM audit_head WHERE id = 1";

/// The head as [`LOCK_HEAD_SQL`] reads it: its hash and date (both `None`
/// for an empty log) and the database's time.
type Head = (Option<String>, Option<DateTime<Utc>>, DateTime<Utc>);

/// Whether an append must continue the chain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    /// [`AuditBackend::append_linked`]: an entry that does not continue the
    /// chain is refused with `Ok(false)`, to be sealed again.
    Required,
    /// [`AuditBackend::append`]: any entry starts an empty log; on a log
    /// that has entries, one that does not continue the chain is an error.
    UnlessEmpty,
}

/// Whether `entry` continues the chain at `head`: it links to the head and
/// is dated after it.
fn continues(entry: &AuditLogEntry, head: &Head) -> bool {
    let (hash, at, _) = head;
    hash.as_deref() == entry.prev_hash.as_deref() && at.is_none_or(|at| entry.created_at > at)
}

impl PgAuditBackend {
    /// One transaction: lock the head, check the entry continues it, and
    /// [`write`] it. `Ok(true)`: the event has an entry in the
    /// chain, this one or an earlier seal of it.
    async fn insert(&self, entry: &AuditLogEntry, link: Link) -> Result<bool, AuditError> {
        let raw = checked(entry)?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let head = lock_head(&mut tx).await?;
        let skip_check = link == Link::UnlessEmpty && head.1.is_none();
        if !continues(entry, &head) && !skip_check && !has_entry(&mut tx, entry.event_id).await? {
            return match link {
                Link::Required => Ok(false),
                Link::UnlessEmpty => Err(AuditError::Database(
                    "the entry does not continue the chain: it must link to the newest \
                     entry and be dated after it"
                        .into(),
                )),
            };
        }
        write(&mut tx, entry, raw).await?;
        commit(tx).await?;
        Ok(true)
    }
}

/// Lock the head row: every other append waits here until this
/// transaction ends.
async fn lock_head(tx: &mut sqlx::PgTransaction<'_>) -> Result<Head, AuditError> {
    sqlx::query_as(LOCK_HEAD_SQL)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)
}

/// Whether the event already has an entry: a second seal of it, which
/// adds nothing.
async fn has_entry(tx: &mut sqlx::PgTransaction<'_>, event_id: Uuid) -> Result<bool, AuditError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM audit_log WHERE event_id = $1)")
        .bind(event_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)
}

/// Under the head lock: insert the entry unless its event has one, move
/// the head to it, and delete the event's outbox row either way. Whether
/// the entry was inserted.
async fn write(
    tx: &mut sqlx::PgTransaction<'_>,
    entry: &AuditLogEntry,
    raw: String,
) -> Result<bool, AuditError> {
    let position: Option<i64> = bind_entry(sqlx::query_scalar(INSERT_SQL), entry, raw)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    if position.is_some() {
        sqlx::query("UPDATE audit_head SET entry_hash = $1, created_at = $2 WHERE id = 1")
            .bind(entry.entry_hash.as_deref())
            .bind(entry.created_at)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
    }
    sqlx::query("DELETE FROM audit_outbox WHERE event_id = $1")
        .bind(entry.event_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(position.is_some())
}

/// Every append commits here, a seal onto the head included, so one fault
/// point stops any of them with the head locked, the entry written and the
/// outbox row deleted, none of it committed.
async fn commit(tx: sqlx::PgTransaction<'_>) -> Result<(), AuditError> {
    fault_point!("audit.seal.before_commit", 1);
    tx.commit()
        .await
        .map_err(|e| AuditError::Database(format!("commit outcome unknown: {e}")))
}

/// What Postgres can store of `entry` as it was sealed: its `raw_event` as
/// text. A sealed entry only, dated to whole microseconds.
fn checked(entry: &AuditLogEntry) -> Result<String, AuditError> {
    if entry.entry_hash.is_none() {
        return Err(AuditError::Database(
            "an unsealed entry cannot join the chain".into(),
        ));
    }
    whole_microseconds(entry)?;
    serde_json::to_string(&entry.raw_event).map_err(|e| AuditError::Serialization(e.to_string()))
}

#[async_trait]
impl AuditBackend for PgAuditBackend {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.insert(&entry, Link::UnlessEmpty).await.map(|_| ())
    }

    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        self.insert(&entry, Link::Required).await
    }

    /// Seals under the head lock, dated by the database's clock, so no
    /// other writer can move the head between the seal and the append.
    async fn append_sealed(
        &self,
        seal: &SealOntoHead<'_>,
    ) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let head = lock_head(&mut tx).await?;
        let entry = seal(head.0.clone(), head.1, head.2);
        let raw = checked(&entry)?;
        if !continues(&entry, &head) {
            return Ok(None);
        }
        let inserted = write(&mut tx, &entry, raw).await?;
        commit(tx).await?;
        Ok(Some(match (inserted, head) {
            (true, _) | (false, (_, None, _)) => (entry.entry_hash, entry.created_at),
            // A second seal of one event: the head did not move.
            (false, (hash, Some(at), _)) => (hash, at),
        }))
    }

    async fn head_at(&self) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        let (hash, at): (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(HEAD_AT_SQL)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(at.map(|at| (hash, at)))
    }

    async fn head(&self) -> Result<Option<String>, AuditError> {
        Ok(self.head_at().await?.and_then(|(hash, _)| hash))
    }

    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        if matches_nothing(q) {
            return Ok(Vec::new());
        }
        let (where_clause, values) = where_clause(q);
        let n = values.len();
        let sql = format!(
            "{SELECT_SQL} WHERE {where_clause} ORDER BY created_at DESC, position DESC \
             LIMIT ${} OFFSET ${}",
            n + 1,
            n + 2
        );
        let mut query = sqlx::query_as::<_, AuditRow>(sqlx::AssertSqlSafe(sql));
        for value in &values {
            query = match value {
                Value::Time(t) => query.bind(*t),
                Value::Text(s) => query.bind(*s),
            };
        }
        let rows = query
            .bind(i64::from(q.limit))
            .bind(i64::from(q.offset))
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.into_iter().map(row_to_entry).collect()
    }

    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        if matches_nothing(q) {
            return Ok(0);
        }
        let (where_clause, values) = where_clause(q);
        let sql = format!("SELECT COUNT(*) FROM audit_log WHERE {where_clause}");
        let mut query = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql));
        for value in &values {
            query = match value {
                Value::Time(t) => query.bind(*t),
                Value::Text(s) => query.bind(*s),
            };
        }
        let count = query.fetch_one(&self.pool).await.map_err(db)?;
        Ok(count as u64)
    }

    /// The database's clock, so every engine dates entries by one clock.
    async fn now(&self) -> Result<DateTime<Utc>, AuditError> {
        sqlx::query_scalar("SELECT statement_timestamp()")
            .fetch_one(&self.pool)
            .await
            .map_err(db)
    }
}

/// A statement's error, as the state store answers it: `Unavailable` when
/// no connection was free or the connection was lost, so `GET /audit` can
/// answer 503; otherwise the database's own text.
fn db(e: sqlx::Error) -> AuditError {
    let text = e.to_string();
    match jamjet_state::postgres::statement_error(e) {
        jamjet_state::StateBackendError::Unavailable(why) => AuditError::Unavailable(why),
        _ => AuditError::Database(text),
    }
}

/// Postgres keeps a time to the microsecond, and the entry's hash covers its
/// times as written, so a time with nanoseconds would read back as another
/// time and the entry would never verify. Refused before anything is
/// written.
fn whole_microseconds(entry: &AuditLogEntry) -> Result<(), AuditError> {
    for (field, at) in [
        ("created_at", Some(entry.created_at)),
        ("expires_at", entry.expires_at),
    ] {
        if at.is_some_and(|at| at.timestamp_subsec_nanos() % 1_000 != 0) {
            return Err(AuditError::Database(format!(
                "{field} is finer than a microsecond, which Postgres does not keep"
            )));
        }
    }
    Ok(())
}

/// Bind an entry's 21 columns, in [`INSERT_SQL`] order.
fn bind_entry<'q, O>(
    q: sqlx::query::QueryScalar<'q, Postgres, O, PgArguments>,
    entry: &AuditLogEntry,
    raw: String,
) -> sqlx::query::QueryScalar<'q, Postgres, O, PgArguments> {
    q.bind(entry.id)
        .bind(entry.event_id)
        .bind(entry.execution_id.clone())
        .bind(entry.sequence)
        .bind(entry.event_type.clone())
        .bind(entry.actor_id.clone())
        .bind(entry.actor_type.as_str())
        .bind(entry.tool_call_hash.clone())
        .bind(entry.policy_decision.clone())
        .bind(entry.http_request_id.clone())
        .bind(entry.http_method.clone())
        .bind(entry.http_path.clone())
        .bind(entry.ip_address.clone())
        .bind(entry.created_at)
        .bind(raw)
        .bind(entry.tenant_id.clone())
        .bind(entry.expires_at)
        .bind(entry.redacted)
        .bind(entry.prev_hash.clone())
        .bind(entry.entry_hash.clone())
        .bind(entry.signature.clone())
}

/// A filter value, bound in placeholder order.
enum Value<'a> {
    Time(DateTime<Utc>),
    Text(&'a str),
}

/// Whether a text filter of `q` holds a NUL. Postgres text cannot hold one,
/// so no entry has it and the filter matches nothing, as on SQLite; sent to
/// the server, the value would be an encoding error.
fn matches_nothing(q: &AuditQuery) -> bool {
    [&q.actor_id, &q.event_type, &q.execution_id, &q.tenant_id]
        .into_iter()
        .flatten()
        .any(|value| value.contains('\0'))
}

/// The WHERE clause for `q`, with a numbered placeholder for each filter
/// value, and the values in placeholder order. The values are bound, never
/// written into the SQL text: `GET /audit` fills `q` from request query
/// parameters. The text is built from column names and operators written
/// here and placeholder numbers.
fn where_clause(q: &AuditQuery) -> (String, Vec<Value<'_>>) {
    let mut wheres = vec!["true".to_string()];
    let mut values = Vec::new();
    let filters: [(&'static str, Option<Value<'_>>); 6] = [
        ("created_at >=", q.from.map(Value::Time)),
        ("created_at <=", q.to.map(Value::Time)),
        ("actor_id =", q.actor_id.as_deref().map(Value::Text)),
        ("event_type =", q.event_type.as_deref().map(Value::Text)),
        ("execution_id =", q.execution_id.as_deref().map(Value::Text)),
        ("tenant_id =", q.tenant_id.as_deref().map(Value::Text)),
    ];
    for (test, value) in filters {
        if let Some(value) = value {
            values.push(value);
            wheres.push(format!("{test} ${}", values.len()));
        }
    }
    (wheres.join(" AND "), values)
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: Uuid,
    event_id: Uuid,
    execution_id: String,
    sequence: i64,
    event_type: String,
    actor_id: String,
    actor_type: String,
    tool_call_hash: Option<String>,
    policy_decision: Option<String>,
    http_request_id: Option<String>,
    http_method: Option<String>,
    http_path: Option<String>,
    ip_address: Option<String>,
    created_at: DateTime<Utc>,
    raw_event: String,
    tenant_id: String,
    expires_at: Option<DateTime<Utc>>,
    redacted: bool,
    prev_hash: Option<String>,
    entry_hash: String,
    signature: Option<String>,
}

fn row_to_entry(row: AuditRow) -> Result<AuditLogEntry, AuditError> {
    let raw_event = serde_json::from_str(&row.raw_event)
        .map_err(|e| AuditError::Serialization(e.to_string()))?;
    let actor_type = parse_actor_type(&row.actor_type).ok_or_else(|| {
        AuditError::Serialization(format!(
            "audit entry {} has an unknown actor_type {:?}",
            row.id, row.actor_type
        ))
    })?;
    Ok(AuditLogEntry {
        id: row.id,
        event_id: row.event_id,
        execution_id: row.execution_id,
        sequence: row.sequence,
        event_type: row.event_type,
        actor_id: row.actor_id,
        actor_type,
        tool_call_hash: row.tool_call_hash,
        policy_decision: row.policy_decision,
        http_request_id: row.http_request_id,
        http_method: row.http_method,
        http_path: row.http_path,
        ip_address: row.ip_address,
        created_at: row.created_at,
        raw_event,
        tenant_id: row.tenant_id,
        expires_at: row.expires_at,
        redacted: row.redacted,
        prev_hash: row.prev_hash,
        entry_hash: Some(row.entry_hash),
        signature: row.signature,
    })
}

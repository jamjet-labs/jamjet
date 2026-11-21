//! Every statement a tenant's view of the Postgres store sends carries the
//! tenant.
//!
//! One `PgBackend` type serves the unscoped engine and every tenant's view,
//! so each scoped statement on a table with a `tenant_id` column must filter
//! by it, and each scoped insert into one must stamp it. A statement that
//! forgot would read or change another tenant's rows, and where a run lock
//! or a checkpoint in the same transaction already refused the write, no
//! behaviour case would see it.
//!
//! The test runs the whole conformance suite on scoped Postgres, one case
//! after another, with each scoped handle wrapped in [`Intercept`] so its
//! calls run inside a span. sqlx logs every statement it sends as a tracing
//! event; a subscriber keeps the ones sent inside that span. The unscoped
//! store and the suite's raw row hooks run outside it, so what they send is
//! not counted. Then every kept statement is checked against the tables the
//! database says carry a tenant.

use jamjet_state::StateBackend;
use jamjet_testkit::conformance::{Fixture, PostgresScopedFactory, Scoper, StoreFactory};
use jamjet_testkit::pg::TestDatabase;
use jamjet_testkit::{Hooks, Intercept, Op};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// The span a scoped handle's calls run in.
const SPAN: &str = "scoped_store";

/// Cases left out, with why. The others all run.
const LEFT_OUT: &[(&str, &str)] = &[(
    "log_order_matches_sequence_order_under_concurrency",
    "takes about 8 minutes on Postgres; its statements are the ones \
     the other append_event_if cases send",
)];

/// Statements a tenant's view sends unfiltered on purpose, with why. Each
/// is matched by its start, in lower case.
const UNFILTERED: &[(&str, &str)] = &[(
    "update api_tokens set last_used_at = statement_timestamp() where token_hash = $1",
    "token validation finds the token's tenant from its hash: it runs before \
     a tenant is known, and SQLite's scoped store does the same",
)];

/// The case running now, for the report.
static CASE: Mutex<&'static str> = Mutex::new("");
/// Each statement sent inside the span, with the first case that sent it.
static SENT: Mutex<BTreeMap<String, &'static str>> = Mutex::new(BTreeMap::new());

/// Puts each call of a scoped handle in the span.
struct InScope;

impl Hooks for InScope {
    fn span(&self, _op: &Op<'_>) -> tracing::Span {
        tracing::info_span!(SPAN)
    }
}

fn wrap(backend: Arc<dyn StateBackend>) -> Arc<dyn StateBackend> {
    Arc::new(Intercept::new(backend, Arc::new(InScope)))
}

/// A scoped Postgres fixture whose scoped handles are wrapped. The row
/// hooks, the unscoped store and a reopened store stay as they are.
async fn fixture() -> Option<Fixture> {
    let mut fx = PostgresScopedFactory.fresh().await?;
    fx.backend = wrap(fx.backend);
    fx.second = fx.second.map(wrap);
    let scoper = fx.scoped.take().expect("a scoped fixture has a scoper");
    let wrapped: Scoper = Arc::new(move |t| wrap(scoper(t)));
    fx.scoped = Some(wrapped);
    Some(fx)
}

/// Keeps the SQL of each sqlx statement event sent inside [`SPAN`].
struct Statements;

impl<S> Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let inside = ctx
            .event_scope(event)
            .is_some_and(|mut spans| spans.any(|s| s.name() == SPAN));
        if !inside {
            return;
        }
        let mut sql = Sql::default();
        event.record(&mut sql);
        // sqlx puts the whole statement in `db.statement` when it is longer
        // than its four-word summary, and leaves it empty otherwise.
        let text = if sql.statement.trim().is_empty() {
            sql.summary
        } else {
            sql.statement
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let case = *CASE.lock().unwrap();
        SENT.lock().unwrap().entry(text).or_insert(case);
    }
}

#[derive(Default)]
struct Sql {
    summary: String,
    statement: String,
}

impl Visit for Sql {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "summary" => self.summary = value.to_string(),
            "db.statement" => self.statement = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "summary" | "db.statement") {
            let text = format!("{value:?}");
            let text = text.trim_matches('"').replace("\\n", "\n");
            self.record_str(field, &text);
        }
    }
}

/// The subscriber, set once for this binary. sqlx logs statements at debug
/// by default, which the registry lets through.
fn listen() {
    static SET: OnceLock<()> = OnceLock::new();
    SET.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(Statements);
        tracing::subscriber::set_global_default(subscriber).expect("no other subscriber");
    });
}

/// Runs each case of a module of the suite on a fresh wrapped fixture.
macro_rules! run_cases {
    ($ran:expr, $module:ident, [$($(#[$meta:meta])* $case:ident),* $(,)?]) => {
        $(
            let ignored = false $(|| { let _ = stringify!($meta); true })*;
            let name = stringify!($case);
            if !ignored && !LEFT_OUT.iter().any(|(n, _)| *n == name) {
                *CASE.lock().unwrap() = concat!(stringify!($module), "::", stringify!($case));
                let fx = fixture().await.expect("the caller checked for a Postgres server");
                // Boxed, so the one test's future does not hold every case.
                Box::pin(jamjet_testkit::conformance::cases::$module::$case(&fx)).await;
                drop(fx);
                $ran.push(name);
            }
        )*
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scoped_statement_filters_by_tenant() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("every_scoped_statement_filters_by_tenant");
    };
    let tenant_tables: BTreeSet<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns \
         WHERE table_schema = 'public' AND column_name = 'tenant_id'",
    )
    .fetch_all(&db.pool().await)
    .await
    .unwrap()
    .into_iter()
    .collect();
    drop(db);
    assert!(
        tenant_tables.contains("events") && tenant_tables.contains("work_items"),
        "the schema read found {tenant_tables:?}"
    );

    listen();
    let mut ran: Vec<&str> = Vec::new();
    jamjet_testkit::for_each_case!(run_cases, ran);
    assert!(ran.len() > 150, "only {} cases ran", ran.len());

    let sent = std::mem::take(&mut *SENT.lock().unwrap());
    assert!(
        sent.keys().any(|s| s.contains("FOR UPDATE SKIP LOCKED")),
        "the claim was never seen; {} statements kept",
        sent.len()
    );
    let mut bad = Vec::new();
    for (sql, case) in &sent {
        if let Some(why) = missing_tenant(sql, &tenant_tables) {
            bad.push(format!("{case}: {why}\n    {sql}"));
        }
    }
    println!(
        "{} cases ran, {} distinct scoped statements checked",
        ran.len(),
        sent.len()
    );
    assert!(
        bad.is_empty(),
        "{} scoped statements do not carry the tenant:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Why `sql`, sent by a tenant's view, could touch another tenant's rows,
/// or `None`. An insert into a table in `tenant_tables` must name
/// `tenant_id` among its columns, and an upsert into one whose conflict key
/// leaves out `tenant_id` could overwrite another tenant's row, so it needs
/// a `tenant_id = $n` filter on the rows it inserts. An upsert whose key
/// holds `tenant_id` only ever meets a row of the tenant it stamps. Any
/// other mention of such a table (read, update, delete, or the source of an
/// insert's rows) needs a `tenant_id = $n` filter in the statement.
fn missing_tenant(sql: &str, tenant_tables: &BTreeSet<String>) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    if UNFILTERED.iter().any(|(start, _)| lower.starts_with(start)) {
        return None;
    }
    let mut rest = lower.as_str();
    if let Some(after) = rest.strip_prefix("insert into ") {
        let target: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let after = after[target.len()..].trim_start();
        let (columns, tail) = match after.strip_prefix('(') {
            Some(inner) => inner.split_once(')').unwrap_or((inner, "")),
            None => ("", after),
        };
        if tenant_tables.contains(&target) && !words(columns).contains("tenant_id") {
            return Some(format!(
                "an insert into {target} that does not stamp tenant_id"
            ));
        }
        rest = tail;
        if let Some(at) = tail.find("on conflict (") {
            let key = tail[at + "on conflict (".len()..]
                .split_once(')')
                .map_or("", |(key, _)| key);
            if words(key).contains("tenant_id") {
                rest = &tail[..at];
            } else if tenant_tables.contains(&target)
                && tail[at..].contains("do update")
                && !has_tenant_filter(tail)
            {
                return Some(format!(
                    "an upsert into {target} keyed without tenant_id and no tenant_id = $n"
                ));
            }
        }
    }
    let touched: Vec<&String> = tenant_tables
        .iter()
        .filter(|t| words(rest).contains(t.as_str()))
        .collect();
    if !touched.is_empty() && !has_tenant_filter(rest) {
        return Some(format!("touches {touched:?} with no tenant_id = $n"));
    }
    None
}

/// The identifiers in `sql`, a qualified name split at its dot.
fn words(sql: &str) -> BTreeSet<&str> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether `sql` holds `tenant_id = $n`, the column qualified or not.
fn has_tenant_filter(sql: &str) -> bool {
    sql.match_indices("tenant_id").any(|(at, m)| {
        let before = sql[..at].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
        let after = sql[at + m.len()..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            return false;
        };
        let after = after.trim_start();
        after
            .strip_prefix('$')
            .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// The rule on small statements, the projection upsert without its tenant
/// clause among them.
#[test]
fn the_rule_reads_statements() {
    let tables: BTreeSet<String> = ["events", "proj_approvals", "workflow_executions"]
        .iter()
        .map(|t| t.to_string())
        .collect();
    let ok = [
        "SELECT status, tenant_id FROM workflow_executions \
         WHERE execution_id = $1 AND tenant_id = $2 FOR UPDATE",
        "INSERT INTO events (execution_id, sequence, tenant_id) VALUES ($1, $2, $3)",
        "INSERT INTO proj_approvals (execution_id, node_id, tenant_id) \
         SELECT e.execution_id, $2, e.tenant_id FROM workflow_executions e \
         WHERE e.execution_id = $1 AND e.tenant_id = $10 ON CONFLICT (execution_id, node_id) \
         DO UPDATE SET node_id = EXCLUDED.node_id",
        "SELECT statement_timestamp()",
        "UPDATE store_identity SET term = GREATEST(term, $1) WHERE id = 1",
        "INSERT INTO events (tenant_id, execution_id) VALUES ($1, $2) \
         ON CONFLICT (tenant_id, execution_id) DO UPDATE SET execution_id = EXCLUDED.execution_id \
         WHERE events.execution_id = $2",
        "UPDATE api_tokens SET last_used_at = statement_timestamp() WHERE token_hash = $1",
    ];
    for sql in ok {
        assert_eq!(missing_tenant(sql, &tables), None, "{sql}");
    }
    let bad = [
        "SELECT * FROM events WHERE execution_id = $1",
        "INSERT INTO events (execution_id, sequence) VALUES ($1, $2)",
        "INSERT INTO proj_approvals (execution_id, node_id, tenant_id) \
         SELECT e.execution_id, $2, e.tenant_id FROM workflow_executions e \
         WHERE e.execution_id = $1 ON CONFLICT (execution_id, node_id) \
         DO UPDATE SET node_id = EXCLUDED.node_id",
        "SELECT * FROM events WHERE execution_id = $1 AND my_tenant_id = $2",
        "SELECT * FROM events WHERE execution_id = $1 AND tenant_id = 'default'",
        "INSERT INTO proj_approvals (execution_id, node_id, tenant_id) VALUES ($1, $2, $3) \
         ON CONFLICT (execution_id, node_id) DO UPDATE SET tenant_id = EXCLUDED.tenant_id",
        "INSERT INTO events (tenant_id, execution_id) VALUES ($1, $2) \
         ON CONFLICT (id) DO UPDATE SET execution_id = EXCLUDED.execution_id \
         WHERE events.execution_id = $2",
    ];
    for sql in bad {
        assert!(missing_tenant(sql, &tables).is_some(), "let through: {sql}");
    }
}

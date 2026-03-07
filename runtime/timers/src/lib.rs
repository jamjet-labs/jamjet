//! JamJet Durable Timers (A2.4) and Cron Scheduler (A2.5)
//!
//! ## Timers
//! Durable timers fire `TimerFired` workflow events at a configured wall-clock
//! time, surviving runtime restarts. Backed by the SQLite `timers` table
//! (migration 0002).
//!
//! ## Cron
//! `CronScheduler` reads `cron_jobs` rows and starts workflow executions on
//! schedule using a minimal 5-field cron parser (no extra crates needed).

use chrono::{DateTime, Datelike, Timelike, Utc};
use jamjet_core::workflow::ExecutionId;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::time::Duration;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

// ── Timer types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timer {
    pub id: Uuid,
    pub execution_id: String,
    pub node_id: String,
    pub fire_at: DateTime<Utc>,
    pub correlation_key: Option<String>,
    pub fired: bool,
    pub created_at: DateTime<Utc>,
    pub fired_at: Option<DateTime<Utc>>,
}

// ── Cron types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: Uuid,
    pub name: String,
    pub cron_expression: String,
    pub workflow_id: String,
    pub workflow_version: String,
    pub input: serde_json::Value,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

// ── Timer store ───────────────────────────────────────────────────────────────

pub struct TimerStore {
    pool: SqlitePool,
}

impl TimerStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Schedule a timer to fire at `fire_at`.
    #[instrument(skip(self), fields(execution_id = %execution_id, node_id = %node_id))]
    pub async fn schedule(
        &self,
        execution_id: &str,
        node_id: &str,
        fire_at: DateTime<Utc>,
        correlation_key: Option<&str>,
    ) -> Result<Timer, String> {
        let id = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query(
            r#"INSERT INTO timers (id, execution_id, node_id, fire_at, correlation_key, fired, created_at)
               VALUES (?, ?, ?, ?, ?, 0, ?)"#,
        )
        .bind(id.to_string())
        .bind(execution_id)
        .bind(node_id)
        .bind(fire_at.to_rfc3339())
        .bind(correlation_key)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("schedule timer: {e}"))?;

        info!(timer_id = %id, fire_at = %fire_at, "Timer scheduled");
        Ok(Timer {
            id,
            execution_id: execution_id.to_string(),
            node_id: node_id.to_string(),
            fire_at,
            correlation_key: correlation_key.map(str::to_string),
            fired: false,
            created_at: now,
            fired_at: None,
        })
    }

    /// Return all timers due to fire (fire_at <= now, fired = 0).
    pub async fn list_due(&self) -> Result<Vec<Timer>, String> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT * FROM timers WHERE fired = 0 AND fire_at <= ? ORDER BY fire_at ASC",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("list due timers: {e}"))?;

        rows.iter().map(|r| row_to_timer(r)).collect()
    }

    /// Mark a timer as fired.
    pub async fn mark_fired(&self, id: Uuid) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE timers SET fired = 1, fired_at = ? WHERE id = ?")
            .bind(&now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("mark_fired: {e}"))?;
        Ok(())
    }

    /// Cancel a pending timer.
    pub async fn cancel(&self, id: Uuid) -> Result<(), String> {
        sqlx::query("UPDATE timers SET fired = 1 WHERE id = ? AND fired = 0")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("cancel timer: {e}"))?;
        Ok(())
    }
}

fn row_to_timer(row: &sqlx::sqlite::SqliteRow) -> Result<Timer, String> {
    let id = Uuid::parse_str(row.get::<&str, _>("id")).map_err(|e| format!("bad timer id: {e}"))?;
    Ok(Timer {
        id,
        execution_id: row.get("execution_id"),
        node_id: row.get("node_id"),
        fire_at: parse_dt(row.get("fire_at"))?,
        correlation_key: row.get("correlation_key"),
        fired: row.get::<i64, _>("fired") != 0,
        created_at: parse_dt(row.get("created_at"))?,
        fired_at: row
            .get::<Option<&str>, _>("fired_at")
            .map(parse_dt)
            .transpose()?,
    })
}

// ── Timer runner ──────────────────────────────────────────────────────────────

/// Polls for due timers and emits `TimerFired` events via the state backend.
pub struct TimerRunner {
    store: TimerStore,
    backend: std::sync::Arc<dyn jamjet_state::StateBackend>,
    poll_interval: Duration,
}

impl TimerRunner {
    pub fn new(pool: SqlitePool, backend: std::sync::Arc<dyn jamjet_state::StateBackend>) -> Self {
        Self {
            store: TimerStore::new(pool),
            backend,
            poll_interval: Duration::from_secs(1),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Run the timer polling loop until the task is cancelled.
    pub async fn run(self) {
        info!("TimerRunner started");
        loop {
            if let Err(e) = self.tick().await {
                warn!(error = %e, "TimerRunner tick error");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn tick(&self) -> Result<(), String> {
        let due = self.store.list_due().await?;
        for timer in due {
            debug!(timer_id = %timer.id, node_id = %timer.node_id, "Firing timer");

            // Parse execution_id
            let exec_id = ExecutionId(
                uuid::Uuid::parse_str(&timer.execution_id)
                    .map_err(|e| format!("bad execution_id in timer: {e}"))?,
            );

            let seq = self
                .backend
                .latest_sequence(&exec_id)
                .await
                .map_err(|e| format!("{e}"))?
                + 1;

            let event = jamjet_state::Event::new(
                exec_id,
                seq,
                jamjet_state::EventKind::TimerFired {
                    node_id: timer.node_id.clone(),
                    correlation_key: timer.correlation_key.clone(),
                },
            );
            self.backend
                .append_event(event)
                .await
                .map_err(|e| format!("append TimerFired: {e}"))?;

            self.store.mark_fired(timer.id).await?;
            info!(timer_id = %timer.id, node_id = %timer.node_id, "Timer fired");
        }
        Ok(())
    }
}

// ── Cron store ────────────────────────────────────────────────────────────────

pub struct CronStore {
    pool: SqlitePool,
}

impl CronStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Create or update a cron job.
    pub async fn upsert(&self, job: &CronJob) -> Result<(), String> {
        sqlx::query(
            r#"INSERT OR REPLACE INTO cron_jobs
               (id, name, cron_expression, workflow_id, workflow_version, input_json, enabled, next_run_at, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(job.id.to_string())
        .bind(&job.name)
        .bind(&job.cron_expression)
        .bind(&job.workflow_id)
        .bind(&job.workflow_version)
        .bind(serde_json::to_string(&job.input).unwrap_or_default())
        .bind(job.enabled as i64)
        .bind(job.next_run_at.to_rfc3339())
        .bind(job.created_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("upsert cron job: {e}"))?;
        Ok(())
    }

    /// List cron jobs due to run (next_run_at <= now, enabled = 1).
    pub async fn list_due(&self) -> Result<Vec<CronJob>, String> {
        let now = Utc::now().to_rfc3339();
        let rows = sqlx::query(
            "SELECT * FROM cron_jobs WHERE enabled = 1 AND next_run_at <= ? ORDER BY next_run_at ASC",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("list due cron jobs: {e}"))?;

        rows.iter().map(|r| row_to_cron(r)).collect()
    }

    /// Advance a cron job's next_run_at after it has fired.
    pub async fn advance(&self, id: Uuid, next_run_at: DateTime<Utc>) -> Result<(), String> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE cron_jobs SET last_run_at = ?, next_run_at = ? WHERE id = ?")
            .bind(&now)
            .bind(next_run_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("advance cron job: {e}"))?;
        Ok(())
    }

    /// Disable a cron job.
    pub async fn disable(&self, name: &str) -> Result<(), String> {
        sqlx::query("UPDATE cron_jobs SET enabled = 0 WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("disable cron: {e}"))?;
        Ok(())
    }
}

fn row_to_cron(row: &sqlx::sqlite::SqliteRow) -> Result<CronJob, String> {
    let id = Uuid::parse_str(row.get::<&str, _>("id")).map_err(|e| format!("bad cron id: {e}"))?;
    let input: serde_json::Value =
        serde_json::from_str(row.get::<&str, _>("input_json")).unwrap_or(serde_json::json!({}));

    Ok(CronJob {
        id,
        name: row.get("name"),
        cron_expression: row.get("cron_expression"),
        workflow_id: row.get("workflow_id"),
        workflow_version: row.get("workflow_version"),
        input,
        enabled: row.get::<i64, _>("enabled") != 0,
        last_run_at: row
            .get::<Option<&str>, _>("last_run_at")
            .map(parse_dt)
            .transpose()?,
        next_run_at: parse_dt(row.get("next_run_at"))?,
        created_at: parse_dt(row.get("created_at"))?,
    })
}

// ── Cron scheduler ────────────────────────────────────────────────────────────

/// Runs due cron jobs by POSTing to the JamJet runtime API.
pub struct CronScheduler {
    store: CronStore,
    /// Base URL of the JamJet API (e.g. `http://localhost:7700`).
    api_url: String,
    poll_interval: Duration,
}

impl CronScheduler {
    pub fn new(pool: SqlitePool, api_url: impl Into<String>) -> Self {
        Self {
            store: CronStore::new(pool),
            api_url: api_url.into(),
            poll_interval: Duration::from_secs(10),
        }
    }

    pub async fn run(self) {
        info!(api = %self.api_url, "CronScheduler started");
        let client = reqwest::Client::new();
        loop {
            if let Err(e) = self.tick(&client).await {
                warn!(error = %e, "CronScheduler tick error");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn tick(&self, client: &reqwest::Client) -> Result<(), String> {
        let due = self.store.list_due().await?;
        for job in due {
            info!(cron_job = %job.name, workflow = %job.workflow_id, "Cron job firing");

            // POST /executions to start the workflow.
            let body = serde_json::json!({
                "workflow_id": job.workflow_id,
                "workflow_version": job.workflow_version,
                "input": job.input,
            });

            match client
                .post(format!("{}/executions", self.api_url))
                .json(&body)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    info!(cron_job = %job.name, "Cron workflow started");
                }
                Ok(r) => {
                    warn!(
                        cron_job = %job.name,
                        status = %r.status(),
                        "Cron workflow start failed"
                    );
                }
                Err(e) => {
                    warn!(cron_job = %job.name, error = %e, "Cron API error");
                }
            }

            // Compute next_run_at.
            match cron_next(&job.cron_expression, Utc::now()) {
                Ok(next) => {
                    self.store.advance(job.id, next).await?;
                }
                Err(e) => {
                    warn!(cron_job = %job.name, error = %e, "Bad cron expression — disabling");
                    self.store.disable(&job.name).await?;
                }
            }
        }
        Ok(())
    }
}

// ── Cron expression parser ────────────────────────────────────────────────────
//
// Supports standard 5-field cron syntax:
//   minute hour day_of_month month day_of_week
// Fields: * (any), N (exact), N-M (range), N,M,... (list), */N (step)

/// Compute the next fire time after `from` for a 5-field cron expression.
pub fn cron_next(expr: &str, from: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron: expected 5 fields, got {}: {expr}",
            fields.len()
        ));
    }

    let minutes = parse_field(fields[0], 0, 59)?;
    let hours = parse_field(fields[1], 0, 23)?;
    let doms = parse_field(fields[2], 1, 31)?;
    let months = parse_field(fields[3], 1, 12)?;
    let dows = parse_field(fields[4], 0, 6)?;

    // Advance by at least 1 minute.
    let mut candidate = from
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(from);
    candidate = candidate + chrono::Duration::minutes(1);

    // Scan up to 366 days forward.
    for _ in 0..(366 * 24 * 60) {
        let m = candidate.minute() as u32;
        let h = candidate.hour() as u32;
        let dom = candidate.day() as u32;
        let mon = candidate.month() as u32;
        let dow = candidate.weekday().num_days_from_sunday() as u32;

        if months.contains(&mon)
            && doms.contains(&dom)
            && dows.contains(&dow)
            && hours.contains(&h)
            && minutes.contains(&m)
        {
            return Ok(candidate);
        }
        candidate = candidate + chrono::Duration::minutes(1);
    }
    Err(format!("cron: no next time found in 366 days for: {expr}"))
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();
    for part in field.split(',') {
        if part == "*" {
            values.extend(min..=max);
        } else if let Some(step_part) = part.strip_prefix("*/") {
            let step: u32 = step_part.parse().map_err(|_| format!("bad step: {part}"))?;
            if step == 0 {
                return Err(format!("step cannot be 0 in: {part}"));
            }
            values.extend((min..=max).step_by(step as usize));
        } else if part.contains('-') {
            let mut iter = part.splitn(2, '-');
            let lo: u32 = iter
                .next()
                .unwrap()
                .parse()
                .map_err(|_| format!("bad range start: {part}"))?;
            let hi: u32 = iter
                .next()
                .unwrap()
                .parse()
                .map_err(|_| format!("bad range end: {part}"))?;
            if lo > hi || lo < min || hi > max {
                return Err(format!("range out of bounds [{min}-{max}]: {part}"));
            }
            values.extend(lo..=hi);
        } else {
            let n: u32 = part
                .parse()
                .map_err(|_| format!("bad cron field value: {part}"))?;
            if n < min || n > max {
                return Err(format!("value {n} out of [{min}-{max}]"));
            }
            values.push(n);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_dt(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("bad datetime '{s}': {e}"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_every_minute() {
        let from = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = cron_next("* * * * *", from).unwrap();
        assert_eq!(next.minute(), 1);
        assert_eq!(next.hour(), 12);
    }

    #[test]
    fn test_cron_specific_time() {
        let from = DateTime::parse_from_rfc3339("2026-01-01T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = cron_next("30 9 * * *", from).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn test_cron_step() {
        let from = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = cron_next("*/15 * * * *", from).unwrap();
        assert!(next.minute() % 15 == 0);
    }

    #[test]
    fn test_cron_bad_field_count() {
        assert!(cron_next("* * * *", Utc::now()).is_err());
    }
}

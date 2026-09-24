//! SQLite persistence via `sqlx`.
//!
//! Migrations are inline (`CREATE TABLE IF NOT EXISTS`) and applied at startup.
//!
//! Timestamps are stored as Unix epoch milliseconds (INTEGER) and JSON blobs as TEXT;
//! the conversion happens at the boundary so the domain model stays clean.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, NaiveTime, TimeZone, Utc};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use favetto_core::agent_state::{AgentUsage, UsageBucket, UsagePeriod, UsageStats, UsageTotals};
use favetto_core::model::{
    Event, EventKind, Failure, NotificationRecord, RunStatus, Schedule, Task, TaskRun, TaskStatus,
};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    status      TEXT NOT NULL,
    attempt     INTEGER NOT NULL DEFAULT 0,
    input       TEXT NOT NULL,
    output      TEXT,
    dedupe_key  TEXT UNIQUE,
    created_at  INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    error       TEXT,
    failure     TEXT,
    session_id  TEXT,
    session_title TEXT,
    parent_id   TEXT,
    root_id     TEXT,
    interactive INTEGER NOT NULL DEFAULT 0,
    retry_at    INTEGER
);

CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_id ON events(id);

CREATE TABLE IF NOT EXISTS schedules (
    id          TEXT PRIMARY KEY,
    cron        TEXT NOT NULL,
    task        TEXT NOT NULL,
    input       TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run    INTEGER,
    job_id      TEXT
);

CREATE TABLE IF NOT EXISTS notifications (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    channel     TEXT NOT NULL,
    subject     TEXT NOT NULL,
    body        TEXT NOT NULL,
    status      TEXT NOT NULL,
    sent_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    delivery_id TEXT PRIMARY KEY,
    provider    TEXT NOT NULL,
    received_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS worktrees (
    task_id    TEXT PRIMARY KEY,
    repo       TEXT NOT NULL,
    path       TEXT NOT NULL,
    branch     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS task_runs (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    attempt     INTEGER NOT NULL,
    status      TEXT NOT NULL,
    agent       TEXT,
    model       TEXT,
    session_id  TEXT,
    started_at  INTEGER,
    finished_at INTEGER,
    exit_code   INTEGER,
    error       TEXT,
    failure     TEXT,
    input_tokens        INTEGER,
    output_tokens       INTEGER,
    reasoning_tokens    INTEGER,
    cache_read_tokens   INTEGER,
    cache_write_tokens  INTEGER,
    cost_usd            REAL
);

-- Per-instance workflow dependencies for runtime DAGs created through
-- `workflow.create` / `workflow.spawn`. Catalog `needs`/`spawn` edges are not
-- represented here. `kind` is reserved; only `finished` is written today.
CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id    TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    kind       TEXT NOT NULL DEFAULT 'finished',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, depends_on)
);

CREATE INDEX IF NOT EXISTS idx_task_dependencies_task_id ON task_dependencies(task_id);
CREATE INDEX IF NOT EXISTS idx_task_dependencies_depends_on ON task_dependencies(depends_on);
CREATE INDEX IF NOT EXISTS idx_tasks_dedupe_key ON tasks(dedupe_key);
CREATE INDEX IF NOT EXISTS idx_tasks_created_at ON tasks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_status_created_at ON tasks(status, created_at);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_notifications_sent_at ON notifications(sent_at);
CREATE INDEX IF NOT EXISTS idx_task_runs_task_attempt ON task_runs(task_id, attempt);
CREATE INDEX IF NOT EXISTS idx_task_runs_status ON task_runs(status);
CREATE INDEX IF NOT EXISTS idx_task_runs_finished_at ON task_runs(finished_at);
"#;

fn ts_ms(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

fn from_ms(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

/// Open (creating if necessary) the SQLite pool.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Apply the M1 schema.
pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(SCHEMA).execute(pool).await?;
    // Additive migration for databases created before `session_id` /
    // `session_title` existed. `ALTER TABLE ... ADD COLUMN` errors if the column
    // is already present; that is the expected case for new databases, so the
    // error is ignored.
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN session_id TEXT")
        .execute(pool)
        .await;
    // Typed failure JSON. Legacy rows keep NULL and decode as `None`.
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN failure TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN session_title TEXT")
        .execute(pool)
        .await;
    // Workflow lineage for `needs = "<task>:all_finished"` fan-in. Created here
    // (not in `SCHEMA`) because the index references `root_id`, which does not
    // exist yet on a legacy database until the `ALTER TABLE` above runs.
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN parent_id TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN root_id TEXT")
        .execute(pool)
        .await;
    // Whether the run was user-started and should execute in the agent's
    // interactive TUI. Legacy rows default to headless (`0`).
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN interactive INTEGER NOT NULL DEFAULT 0")
        .execute(pool)
        .await;
    // Per-attempt execution counter. Legacy rows default to 0 attempts; the next
    // claim records attempt 1.
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0")
        .execute(pool)
        .await;
    // Earliest wall-clock time a re-enqueued task may be claimed again. NULL for
    // every ordinary task; set by the automatic-retry path so the executor can
    // honor the backoff without a sleeping task per retry.
    let _ = sqlx::query("ALTER TABLE tasks ADD COLUMN retry_at INTEGER")
        .execute(pool)
        .await;
    // Per-run token/cost usage and the configured model, stored on `task_runs`
    // (not inside the `tasks.output` blob) so `usage.stats` can aggregate it and
    // so it survives the retention pass that clears output blobs. Legacy rows
    // keep NULL and decode as `None`.
    let _ = sqlx::query("ALTER TABLE task_runs ADD COLUMN model TEXT")
        .execute(pool)
        .await;
    for column in [
        "input_tokens INTEGER",
        "output_tokens INTEGER",
        "reasoning_tokens INTEGER",
        "cache_read_tokens INTEGER",
        "cache_write_tokens INTEGER",
        "cost_usd REAL",
    ] {
        // `column` is one of the fixed literals above, never user input.
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE task_runs ADD COLUMN {column}"
        )))
        .execute(pool)
        .await;
    }
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_root_id ON tasks(root_id)")
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_task_runs_finished_at ON task_runs(finished_at)",
    )
    .execute(pool)
    .await;
    Ok(())
}

/// Insert a task, silently ignoring a duplicate dedupe key.
pub async fn insert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id, interactive, failure)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task.id.to_string())
    .bind(&task.name)
    .bind(task.status.as_str())
    .bind(task.attempt as i64)
    .bind(serde_json::to_string(&task.input)?)
    .bind(task.output.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&task.dedupe_key)
    .bind(ts_ms(task.created_at))
    .bind(task.started_at.map(ts_ms))
    .bind(task.finished_at.map(ts_ms))
    .bind(&task.error)
    .bind(&task.session_id)
    .bind(&task.session_title)
    .bind(task.parent_id.map(|id| id.to_string()))
    .bind(task.root_id.map(|id| id.to_string()))
    .bind(task.interactive as i64)
    .bind(task.failure.as_ref().map(serde_json::to_string).transpose()?)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert-or-update a task (full overwrite of mutable fields).
///
/// Production finishes go through the conditional [`finish_active_task`] (or a
/// targeted status CAS) so a cancelled row is never overwritten; this helper is
/// kept for tests that seed rows directly.
#[cfg(test)]
pub async fn upsert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tasks (id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id, interactive, failure)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             status = excluded.status,
             attempt = excluded.attempt,
             output = excluded.output,
             started_at = excluded.started_at,
             finished_at = excluded.finished_at,
             error = excluded.error,
             failure = excluded.failure,
             session_id = excluded.session_id,
             session_title = excluded.session_title",
    )
    .bind(task.id.to_string())
    .bind(&task.name)
    .bind(task.status.as_str())
    .bind(task.attempt as i64)
    .bind(serde_json::to_string(&task.input)?)
    .bind(task.output.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&task.dedupe_key)
    .bind(ts_ms(task.created_at))
    .bind(task.started_at.map(ts_ms))
    .bind(task.finished_at.map(ts_ms))
    .bind(&task.error)
    .bind(&task.session_id)
    .bind(&task.session_title)
    .bind(task.parent_id.map(|id| id.to_string()))
    .bind(task.root_id.map(|id| id.to_string()))
    .bind(task.interactive as i64)
    .bind(task.failure.as_ref().map(serde_json::to_string).transpose()?)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single task by id.
pub async fn get_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task))
}

/// Fetch the task stored under `dedupe_key`, if any. Used to make
/// `workflow.create` / `workflow.spawn` idempotent: re-submitting the same key
/// returns the canonical row instead of the transient id an
/// `INSERT OR IGNORE` would have produced.
pub async fn get_task_by_dedupe_key(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE dedupe_key = ?",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task))
}

/// List the most recent tasks (newest first), without their output blobs. Output
/// is fetched on demand with [`get_task`].
pub async fn list_tasks(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Append an event, returning its assigned monotonic id.
pub async fn insert_event(pool: &SqlitePool, event: &Event) -> anyhow::Result<i64> {
    let res = sqlx::query("INSERT INTO events (kind, payload, created_at) VALUES (?, ?, ?)")
        .bind(event.kind.as_str())
        .bind(serde_json::to_string(&event.payload)?)
        .bind(ts_ms(event.created_at))
        .execute(pool)
        .await?;
    Ok(res.last_insert_rowid())
}

/// The most recent `limit` events, chronological (ascending id).
pub async fn tail_events(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Event>> {
    let rows =
        sqlx::query("SELECT id, kind, payload, created_at FROM events ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(pool)
            .await?;
    let mut events: Vec<Event> = rows.iter().map(row_to_event).collect();
    events.reverse();
    Ok(events)
}

/// Events strictly after `id`, ascending — the replay used by resumable subscriptions.
pub async fn events_after(pool: &SqlitePool, id: i64, limit: i64) -> anyhow::Result<Vec<Event>> {
    let rows = sqlx::query(
        "SELECT id, kind, payload, created_at FROM events WHERE id > ? ORDER BY id ASC LIMIT ?",
    )
    .bind(id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_event).collect())
}

fn row_to_task(row: &SqliteRow) -> Task {
    Task {
        id: Uuid::parse_str(&row.get::<String, _>("id")).unwrap_or_default(),
        name: row.get("name"),
        status: row
            .get::<String, _>("status")
            .parse()
            .unwrap_or(TaskStatus::Pending),
        attempt: row
            .try_get::<i64, _>("attempt")
            .map(|v| v.max(0) as u32)
            .unwrap_or(0),
        input: serde_json::from_str(&row.get::<String, _>("input")).unwrap_or_default(),
        output: row
            .try_get::<Option<String>, _>("output")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok()),
        dedupe_key: row.get("dedupe_key"),
        created_at: from_ms(row.get("created_at")),
        started_at: row.get::<Option<i64>, _>("started_at").map(from_ms),
        finished_at: row.get::<Option<i64>, _>("finished_at").map(from_ms),
        error: row.get("error"),
        failure: row
            .try_get::<Option<String>, _>("failure")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok()),
        session_id: row
            .try_get::<Option<String>, _>("session_id")
            .ok()
            .flatten(),
        session_title: row
            .try_get::<Option<String>, _>("session_title")
            .ok()
            .flatten(),
        parent_id: row
            .try_get::<Option<String>, _>("parent_id")
            .ok()
            .flatten()
            .and_then(|s| Uuid::parse_str(&s).ok()),
        root_id: row
            .try_get::<Option<String>, _>("root_id")
            .ok()
            .flatten()
            .and_then(|s| Uuid::parse_str(&s).ok()),
        interactive: row
            .try_get::<i64, _>("interactive")
            .map(|v| v != 0)
            .unwrap_or(false),
    }
}

fn row_to_task_run(row: &SqliteRow) -> TaskRun {
    TaskRun {
        id: Uuid::parse_str(&row.get::<String, _>("id")).unwrap_or_default(),
        task_id: Uuid::parse_str(&row.get::<String, _>("task_id")).unwrap_or_default(),
        attempt: row.get::<i64, _>("attempt").max(0) as u32,
        status: row
            .get::<String, _>("status")
            .parse()
            .unwrap_or(RunStatus::Pending),
        agent: row.try_get::<Option<String>, _>("agent").ok().flatten(),
        model: row.try_get::<Option<String>, _>("model").ok().flatten(),
        session_id: row
            .try_get::<Option<String>, _>("session_id")
            .ok()
            .flatten(),
        started_at: row
            .try_get::<Option<i64>, _>("started_at")
            .ok()
            .flatten()
            .map(from_ms),
        finished_at: row
            .try_get::<Option<i64>, _>("finished_at")
            .ok()
            .flatten()
            .map(from_ms),
        exit_code: row.try_get::<Option<i32>, _>("exit_code").ok().flatten(),
        error: row.try_get::<Option<String>, _>("error").ok().flatten(),
        usage: usage_from_row(row),
        failure: row
            .try_get::<Option<String>, _>("failure")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok()),
    }
}

/// Rebuild a run's [`AgentUsage`] from its flat `task_runs` columns. Returns
/// `None` when every usage column is NULL (the agent reported nothing, or the
/// row predates the columns).
fn usage_from_row(row: &SqliteRow) -> Option<AgentUsage> {
    let input = row.try_get::<Option<i64>, _>("input_tokens").ok().flatten();
    let output = row
        .try_get::<Option<i64>, _>("output_tokens")
        .ok()
        .flatten();
    let reasoning = row
        .try_get::<Option<i64>, _>("reasoning_tokens")
        .ok()
        .flatten();
    let cache_read = row
        .try_get::<Option<i64>, _>("cache_read_tokens")
        .ok()
        .flatten();
    let cache_write = row
        .try_get::<Option<i64>, _>("cache_write_tokens")
        .ok()
        .flatten();
    let cost = row.try_get::<Option<f64>, _>("cost_usd").ok().flatten();
    if input.is_none()
        && output.is_none()
        && reasoning.is_none()
        && cache_read.is_none()
        && cache_write.is_none()
        && cost.is_none()
    {
        return None;
    }
    Some(AgentUsage {
        input_tokens: input.unwrap_or(0).max(0) as u64,
        output_tokens: output.unwrap_or(0).max(0) as u64,
        reasoning_tokens: reasoning.unwrap_or(0).max(0) as u64,
        cache_read_tokens: cache_read.unwrap_or(0).max(0) as u64,
        cache_write_tokens: cache_write.unwrap_or(0).max(0) as u64,
        cost_usd: cost,
    })
}

fn row_to_event(row: &SqliteRow) -> Event {
    let raw_kind = row.get::<String, _>("kind");
    let kind = match raw_kind.parse::<EventKind>() {
        Ok(kind) => kind,
        Err(()) => {
            tracing::warn!(
                kind = %raw_kind,
                "unrecognised event kind; falling back to Unknown"
            );
            EventKind::Unknown
        }
    };
    Event {
        id: row.get("id"),
        kind,
        payload: serde_json::from_str(&row.get::<String, _>("payload")).unwrap_or_default(),
        created_at: from_ms(row.get("created_at")),
    }
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

/// List schedules (newest first).
pub async fn list_schedules(pool: &SqlitePool) -> anyhow::Result<Vec<Schedule>> {
    let rows =
        sqlx::query("SELECT id, cron, task, input, enabled, last_run FROM schedules ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows.iter().map(row_to_schedule).collect())
}

/// Insert or update a schedule, preserving its job_id column.
pub async fn upsert_schedule(pool: &SqlitePool, schedule: &Schedule) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO schedules (id, cron, task, input, enabled, last_run) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET cron = excluded.cron, task = excluded.task,
             input = excluded.input, enabled = excluded.enabled, last_run = excluded.last_run",
    )
    .bind(&schedule.id)
    .bind(&schedule.cron)
    .bind(&schedule.task)
    .bind(serde_json::to_string(&schedule.input)?)
    .bind(schedule.enabled as i64)
    .bind(schedule.last_run.map(ts_ms))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_schedule_job_id(pool: &SqlitePool, id: &str, job_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE schedules SET job_id = ? WHERE id = ?")
        .bind(job_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_schedule_job_id(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT job_id FROM schedules WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>("job_id")))
}

pub async fn touch_schedule(pool: &SqlitePool, id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE schedules SET last_run = ? WHERE id = ?")
        .bind(ts_ms(Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_schedule(pool: &SqlitePool, id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM schedules WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

fn row_to_schedule(row: &SqliteRow) -> Schedule {
    Schedule {
        id: row.get("id"),
        cron: row.get("cron"),
        task: row.get("task"),
        input: serde_json::from_str(&row.get::<String, _>("input")).unwrap_or_default(),
        enabled: row.get::<i64, _>("enabled") != 0,
        last_run: row.get::<Option<i64>, _>("last_run").map(from_ms),
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

pub async fn insert_notification(
    pool: &SqlitePool,
    channel: &str,
    subject: &str,
    body: &str,
    status: &str,
) -> anyhow::Result<i64> {
    let res = sqlx::query(
        "INSERT INTO notifications (channel, subject, body, status, sent_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(channel)
    .bind(subject)
    .bind(body)
    .bind(status)
    .bind(ts_ms(Utc::now()))
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn list_notifications(
    pool: &SqlitePool,
    limit: i64,
) -> anyhow::Result<Vec<NotificationRecord>> {
    let rows = sqlx::query(
        "SELECT id, channel, subject, body, status, sent_at FROM notifications ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_notification).collect())
}

fn row_to_notification(row: &SqliteRow) -> NotificationRecord {
    NotificationRecord {
        id: row.get("id"),
        channel: row.get("channel"),
        subject: row.get("subject"),
        body: row.get("body"),
        status: row.get("status"),
        sent_at: from_ms(row.get("sent_at")),
    }
}

// ---------------------------------------------------------------------------
// Webhook deliveries
// ---------------------------------------------------------------------------

/// Record a webhook delivery. Returns `true` if this is the first time the
/// delivery was seen, `false` if it is a redelivery (already recorded).
pub async fn record_delivery(
    pool: &SqlitePool,
    provider: &str,
    delivery_id: &str,
) -> anyhow::Result<bool> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, provider, received_at)
         VALUES (?, ?, ?)",
    )
    .bind(delivery_id)
    .bind(provider)
    .bind(ts_ms(Utc::now()))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// Task queue
// ---------------------------------------------------------------------------

/// Every `pending` task, oldest first (for the startup reconciler).
pub async fn list_pending_tasks(pool: &SqlitePool) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE status = 'pending' ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Every active task (`running`/`awaiting_input`), oldest first. These are the
/// tasks a previous daemon left in flight, for the startup reconciler.
pub async fn list_active_tasks(pool: &SqlitePool) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE status IN ('running', 'awaiting_input') ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Mark an active run `interrupted` (its agent process died with the old
/// daemon). Only a non-terminal run is touched, so a repeated reconcile is a
/// no-op. Returns whether a row changed.
pub async fn interrupt_run(
    pool: &SqlitePool,
    id: Uuid,
    error: &str,
    failure: &Failure,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE task_runs SET status = 'interrupted', finished_at = ?, error = ?, failure = ? \
         WHERE id = ? AND status IN ('pending', 'running', 'awaiting_input')",
    )
    .bind(ts_ms(Utc::now()))
    .bind(error)
    .bind(serde_json::to_string(failure)?)
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Fail an active (`running`/`awaiting_input`) task. Terminal tasks are never
/// touched, so a repeated reconcile is a no-op. Returns whether a row changed.
pub async fn fail_stale_task(
    pool: &SqlitePool,
    id: Uuid,
    error: &str,
    failure: &Failure,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'failed', error = ?, failure = ?, finished_at = ? \
         WHERE id = ? AND status IN ('running', 'awaiting_input')",
    )
    .bind(error)
    .bind(serde_json::to_string(failure)?)
    .bind(ts_ms(Utc::now()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Re-enqueue an active (`running`/`awaiting_input`) task for a fresh attempt,
/// clearing the previous attempt's outcome so the next claim starts clean.
/// Returns whether a row changed.
pub async fn retry_stale_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'pending', started_at = NULL, finished_at = NULL, \
             error = NULL, failure = NULL, retry_at = NULL \
         WHERE id = ? AND status IN ('running', 'awaiting_input')",
    )
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Re-enqueue a failed task for the next automatic attempt after `retry_at`.
///
/// Flips the row back to `pending` and clears the just-finished attempt's
/// outcome (and its `retry_at`), so the dispatcher claims it once the backoff
/// deadline passes and records a fresh run under `attempt + 1`. Only a currently
/// `failed` row is touched, so a concurrent cancellation/manual retry wins.
/// Returns whether a row changed.
pub async fn schedule_task_retry(
    pool: &SqlitePool,
    id: Uuid,
    retry_at: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'pending', started_at = NULL, finished_at = NULL, \
             error = NULL, failure = NULL, output = NULL, retry_at = ? \
         WHERE id = ? AND status = 'failed'",
    )
    .bind(ts_ms(retry_at))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Re-enqueue a terminal (`succeeded`/`failed`/`cancelled`) task for a manual
/// retry. Keeps the previous attempt's run history; the next claim records
/// `attempt + 1`. Clears the outcome fields and any pending retry deadline.
/// Returns whether a row changed.
pub async fn requeue_terminal_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'pending', started_at = NULL, finished_at = NULL, \
             error = NULL, failure = NULL, output = NULL, retry_at = NULL \
         WHERE id = ? AND status IN ('succeeded', 'failed', 'cancelled')",
    )
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Cancel a non-terminal (`pending`/`running`/`awaiting_input`) task. Already
/// terminal tasks are never touched, so a root-scoped cancel cannot overwrite a
/// task that finished concurrently. Returns whether a row changed, so the caller
/// emits `TaskCancelled` only for tasks this request actually cancelled.
pub async fn cancel_active_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'cancelled', finished_at = ? \
         WHERE id = ? AND status IN ('pending', 'running', 'awaiting_input')",
    )
    .bind(ts_ms(Utc::now()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Finalize an active (`running`/`awaiting_input`) task with a terminal outcome
/// as a compare-and-set.
///
/// Unlike a full-overwrite upsert, the write is conditional on the task still
/// being active: a cancellation (or any other terminal transition) that landed while
/// the run was in flight wins, so the run's own result can never resurrect the
/// cancelled row. Returns whether the row was written; the caller must suppress
/// its `TaskFinished`/`needs`/join work when it was not.
pub async fn finish_active_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET
             status = ?,
             attempt = ?,
             output = ?,
             started_at = ?,
             finished_at = ?,
             error = ?,
             failure = ?,
             session_id = ?,
             session_title = ?
         WHERE id = ? AND status IN ('running', 'awaiting_input')",
    )
    .bind(task.status.as_str())
    .bind(task.attempt as i64)
    .bind(
        task.output
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .bind(task.started_at.map(ts_ms))
    .bind(task.finished_at.map(ts_ms))
    .bind(&task.error)
    .bind(
        task.failure
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .bind(&task.session_id)
    .bind(&task.session_title)
    .bind(task.id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Fail a `pending` task that can no longer run (its catalog definition
/// vanished). Only a pending row is touched. Returns whether a row changed.
pub async fn fail_pending_task(
    pool: &SqlitePool,
    id: Uuid,
    error: &str,
    failure: &Failure,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'failed', error = ?, failure = ?, finished_at = ? \
         WHERE id = ? AND status = 'pending'",
    )
    .bind(error)
    .bind(serde_json::to_string(failure)?)
    .bind(ts_ms(Utc::now()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Up to `limit` pending tasks, oldest first (for the parallel executor).
///
/// A task with runtime dependencies is gated on them: it is returned only once
/// every predecessor present in `task_dependencies` has reached a terminal state
/// (`succeeded`/`failed`/`cancelled`). A missing predecessor does not block, so a
/// pruned task cannot wedge a dependent. Catalog `needs` dependents have no
/// dependency rows, so their dispatch is unchanged.
pub async fn next_pending_tasks(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT t.id, t.name, t.status, t.attempt, t.input, t.dedupe_key, t.created_at, t.started_at, t.finished_at, t.error, t.failure, t.session_id, t.session_title, t.parent_id, t.root_id, t.interactive \
         FROM tasks t \
         WHERE t.status = 'pending' \
           AND (t.retry_at IS NULL OR t.retry_at <= ?) \
           AND NOT EXISTS ( \
               SELECT 1 FROM task_dependencies d \
               JOIN tasks p ON p.id = d.depends_on \
               WHERE d.task_id = t.id \
                 AND p.status NOT IN ('succeeded', 'failed', 'cancelled') \
           ) \
         ORDER BY t.created_at ASC LIMIT ?",
    )
    .bind(ts_ms(Utc::now()))
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Atomically claim a pending task for execution, recording exactly one run for
/// the attempt.
///
/// In a single transaction this flips the task to `running`, advances
/// `task.attempt` to `task.attempt + 1`, and inserts a [`TaskRun`] under that
/// attempt. Returns the new run, or `None` when the task was already claimed
/// (e.g. by another dispatcher tick) so a duplicate run is never created.
pub async fn claim_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<Option<TaskRun>> {
    let now = Utc::now();
    let attempt = task.attempt + 1;
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        "UPDATE tasks SET status = 'running', started_at = ?, attempt = ?, retry_at = NULL \
         WHERE id = ? AND status = 'pending'",
    )
    .bind(ts_ms(now))
    .bind(attempt as i64)
    .bind(task.id.to_string())
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() != 1 {
        // Already claimed: roll back and let the owner finalize its own run.
        return Ok(None);
    }
    let run = TaskRun {
        id: Uuid::new_v4(),
        task_id: task.id,
        attempt,
        status: RunStatus::Running,
        agent: None,
        model: None,
        session_id: None,
        started_at: Some(now),
        finished_at: None,
        exit_code: None,
        error: None,
        usage: None,
        failure: None,
    };
    insert_task_run(&mut *tx, &run).await?;
    tx.commit().await?;
    Ok(Some(run))
}

/// Tasks named `name` that belong to workflow root `root_id`, oldest first,
/// including their output blobs. The root task itself is included when its name
/// matches (`id = root_id`), since a directly-started task is its own root.
pub async fn list_tasks_in_root(
    pool: &SqlitePool,
    root_id: Uuid,
    name: &str,
) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE (root_id = ? OR id = ?) AND name = ? ORDER BY created_at ASC",
    )
    .bind(root_id.to_string())
    .bind(root_id.to_string())
    .bind(name)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Non-terminal (`pending`/`running`/`awaiting_input`) tasks named `name` in the
/// workflow root `root_id`. An empty result means a fan-in barrier is satisfied.
pub async fn list_active_tasks_in_root(
    pool: &SqlitePool,
    root_id: Uuid,
    name: &str,
) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE (root_id = ? OR id = ?) AND name = ? AND status IN ('pending', 'running', 'awaiting_input') ORDER BY created_at ASC",
    )
    .bind(root_id.to_string())
    .bind(root_id.to_string())
    .bind(name)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Every task belonging to workflow root `root_id` (the root row itself is
/// included via `id = root_id`), oldest first, **without** output blobs.
/// Runtime view for `workflow.inspect`.
pub async fn list_root_tasks(pool: &SqlitePool, root_id: Uuid) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive \
         FROM tasks WHERE root_id = ? OR id = ? ORDER BY created_at ASC",
    )
    .bind(root_id.to_string())
    .bind(root_id.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

// ---------------------------------------------------------------------------
// Runtime workflow dependencies (`workflow.create` / `workflow.spawn`)
// ---------------------------------------------------------------------------

/// Record that `task_id` may start only once `depends_on` reaches a terminal
/// state. Idempotent: a duplicate edge is ignored.
pub async fn insert_dependency(
    pool: &SqlitePool,
    task_id: Uuid,
    depends_on: Uuid,
    created_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on, kind, created_at)
         VALUES (?, ?, 'finished', ?)",
    )
    .bind(task_id.to_string())
    .bind(depends_on.to_string())
    .bind(ts_ms(created_at))
    .execute(pool)
    .await?;
    Ok(())
}

/// The predecessor ids `task_id` waits on, oldest first.
#[cfg(test)]
pub async fn list_dependencies(pool: &SqlitePool, task_id: Uuid) -> anyhow::Result<Vec<Uuid>> {
    let rows = sqlx::query(
        "SELECT depends_on FROM task_dependencies WHERE task_id = ? ORDER BY created_at ASC",
    )
    .bind(task_id.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| Uuid::parse_str(&row.get::<String, _>("depends_on")).ok())
        .collect())
}

/// Every `(task_id, depends_on)` dependency edge whose dependent belongs to
/// `root_id` (the root row itself is included via `id = root_id`). Powers the
/// `blocked` bucket in `workflow.inspect`.
pub async fn list_root_dependencies(
    pool: &SqlitePool,
    root_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, Uuid)>> {
    let rows = sqlx::query(
        "SELECT d.task_id, d.depends_on FROM task_dependencies d \
         JOIN tasks t ON t.id = d.task_id \
         WHERE t.root_id = ? OR t.id = ? \
         ORDER BY d.created_at ASC",
    )
    .bind(root_id.to_string())
    .bind(root_id.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                Uuid::parse_str(&row.get::<String, _>("task_id")).ok()?,
                Uuid::parse_str(&row.get::<String, _>("depends_on")).ok()?,
            ))
        })
        .collect())
}

/// Set a task's status only when it currently has `from`. Targeted, so it can
/// never overwrite `session_id`, `session_title`, `output`, or terminal fields
/// written concurrently. Returns whether a row changed.
pub async fn set_task_status_if(
    pool: &SqlitePool,
    id: Uuid,
    to: TaskStatus,
    from: TaskStatus,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE tasks SET status = ? WHERE id = ? AND status = ?")
        .bind(to.as_str())
        .bind(id.to_string())
        .bind(from.as_str())
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Persist a run's session id/title without touching `status`, `output`, or any
/// other column. `session_id` is written only when currently NULL;
/// `session_title` only when currently NULL and `title` is `Some`. Returns
/// whether a row matched.
pub async fn set_task_session(
    pool: &SqlitePool,
    id: Uuid,
    session_id: &str,
    title: Option<&str>,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET
             session_id = COALESCE(session_id, ?),
             session_title = COALESCE(session_title, ?)
         WHERE id = ?",
    )
    .bind(session_id)
    .bind(title)
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Task runs
// ---------------------------------------------------------------------------

/// Insert a run row.
///
/// Generic over the sqlx executor so `claim_task` can create the run inside its
/// transaction; passing a plain `&pool` works too.
pub async fn insert_task_run<'e, E>(executor: E, run: &TaskRun) -> anyhow::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO task_runs
            (id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure,
             input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(run.id.to_string())
    .bind(run.task_id.to_string())
    .bind(run.attempt as i64)
    .bind(run.status.as_str())
    .bind(&run.agent)
    .bind(&run.model)
    .bind(&run.session_id)
    .bind(run.started_at.map(ts_ms))
    .bind(run.finished_at.map(ts_ms))
    .bind(run.exit_code)
    .bind(&run.error)
    .bind(run.failure.as_ref().map(serde_json::to_string).transpose()?)
    .bind(run.usage.as_ref().map(|u| u.input_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.output_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.reasoning_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.cache_read_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.cache_write_tokens as i64))
    .bind(run.usage.as_ref().and_then(|u| u.cost_usd))
    .execute(executor)
    .await?;
    Ok(())
}

/// Fetch a single run by id.
#[allow(dead_code)] // consumed by the executor (#147); unit-tested here.
pub async fn get_task_run(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<TaskRun>> {
    let row = sqlx::query(
        "SELECT id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure, \
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd \
         FROM task_runs WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task_run))
}

/// A task's runs, oldest-first (attempt ascending).
#[allow(dead_code)] // consumed by the executor (#147); unit-tested here.
pub async fn list_task_runs(pool: &SqlitePool, task_id: Uuid) -> anyhow::Result<Vec<TaskRun>> {
    let rows = sqlx::query(
        "SELECT id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure, \
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd \
         FROM task_runs WHERE task_id = ? ORDER BY attempt ASC, started_at ASC",
    )
    .bind(task_id.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task_run).collect())
}

/// The task's current non-terminal run, if any (latest attempt first).
#[allow(dead_code)] // consumed by the executor (#147); unit-tested here.
pub async fn get_active_task_run(
    pool: &SqlitePool,
    task_id: Uuid,
) -> anyhow::Result<Option<TaskRun>> {
    let row = sqlx::query(
        "SELECT id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure, \
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd \
         FROM task_runs WHERE task_id = ? AND status IN ('pending', 'running', 'awaiting_input') \
         ORDER BY attempt DESC LIMIT 1",
    )
    .bind(task_id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task_run))
}

/// Every non-terminal run across all tasks, oldest-first (for the reconciler).
#[allow(dead_code)] // consumed by the startup reconciler (#148); unit-tested here.
pub async fn list_active_task_runs(pool: &SqlitePool) -> anyhow::Result<Vec<TaskRun>> {
    let rows = sqlx::query(
        "SELECT id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure, \
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd \
         FROM task_runs WHERE status IN ('pending', 'running', 'awaiting_input') \
         ORDER BY started_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task_run).collect())
}

/// Finalize a run's outcome columns by id. Returns whether a row matched.
///
/// Only the outcome fields are written; `started_at` and `attempt` are left as
/// inserted. `session_id`/`agent`/`model` use `COALESCE`, so a value persisted
/// earlier is kept when `None` is passed. Takes a whole [`TaskRun`] because the
/// executor already has one in hand when finishing an attempt.
pub async fn finalize_task_run(pool: &SqlitePool, run: &TaskRun) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE task_runs SET
             status = ?,
             session_id = COALESCE(?, session_id),
             agent = COALESCE(?, agent),
             model = COALESCE(?, model),
             finished_at = ?,
             exit_code = ?,
             error = ?,
             failure = ?,
             input_tokens = ?,
             output_tokens = ?,
             reasoning_tokens = ?,
             cache_read_tokens = ?,
             cache_write_tokens = ?,
             cost_usd = ?
         WHERE id = ?",
    )
    .bind(run.status.as_str())
    .bind(&run.session_id)
    .bind(&run.agent)
    .bind(&run.model)
    .bind(run.finished_at.map(ts_ms))
    .bind(run.exit_code)
    .bind(&run.error)
    .bind(
        run.failure
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
    )
    .bind(run.usage.as_ref().map(|u| u.input_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.output_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.reasoning_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.cache_read_tokens as i64))
    .bind(run.usage.as_ref().map(|u| u.cache_write_tokens as i64))
    .bind(run.usage.as_ref().and_then(|u| u.cost_usd))
    .bind(run.id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// Usage aggregation
// ---------------------------------------------------------------------------

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// Aggregate finished runs into `period`-sized buckets ending at `now`, with
/// overall totals. The series always has a fixed number of points (24 hours, 7
/// days, 30 days or 12 months) so a chart does not jump as data arrives; empty
/// buckets are zero. Cost is `None` for a bucket where no run reported one.
pub async fn usage_stats(
    pool: &SqlitePool,
    period: UsagePeriod,
    now: DateTime<Utc>,
) -> anyhow::Result<UsageStats> {
    let buckets = usage_bucket_bounds(period, now);
    let Some(first) = buckets.first() else {
        return Ok(UsageStats {
            period,
            ..Default::default()
        });
    };
    let start_ms = ts_ms(first.0);
    let end_ms = buckets
        .last()
        .map(|(_, end)| ts_ms(*end))
        .unwrap_or(start_ms);

    // Group in SQL by the period's bucket key so the sum is a single scan; empty
    // buckets are filled in below.
    let key_expr = match period {
        UsagePeriod::Day => "strftime('%Y-%m-%dT%H', finished_at / 1000, 'unixepoch')",
        UsagePeriod::Week | UsagePeriod::Month => {
            "strftime('%Y-%m-%d', finished_at / 1000, 'unixepoch')"
        }
        UsagePeriod::Year => "strftime('%Y-%m', finished_at / 1000, 'unixepoch')",
    };
    let sql = format!(
        "SELECT {key_expr} AS bucket, \
                COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens, \
                COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, \
                COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, \
                SUM(cost_usd) AS cost_usd, \
                COUNT(*) AS runs \
         FROM task_runs \
         WHERE finished_at IS NOT NULL AND finished_at >= ? AND finished_at < ? \
         GROUP BY bucket ORDER BY bucket"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(pool)
        .await?;

    let mut grouped: HashMap<String, (AgentUsage, u64)> = HashMap::new();
    for row in &rows {
        let key: String = row.get("bucket");
        let usage = AgentUsage {
            input_tokens: row.get::<i64, _>("input_tokens").max(0) as u64,
            output_tokens: row.get::<i64, _>("output_tokens").max(0) as u64,
            reasoning_tokens: row.get::<i64, _>("reasoning_tokens").max(0) as u64,
            cache_read_tokens: row.get::<i64, _>("cache_read_tokens").max(0) as u64,
            cache_write_tokens: row.get::<i64, _>("cache_write_tokens").max(0) as u64,
            cost_usd: row.try_get::<Option<f64>, _>("cost_usd").ok().flatten(),
        };
        let runs = row.get::<i64, _>("runs").max(0) as u64;
        let entry = grouped.entry(key).or_default();
        entry.0.merge(&usage);
        entry.1 += runs;
    }

    let mut totals = UsageTotals::default();
    let mut out = Vec::with_capacity(buckets.len());
    for (start, end) in buckets {
        let key = usage_bucket_key(period, start);
        let (usage, runs) = grouped.remove(&key).unwrap_or_default();
        totals.usage.merge(&usage);
        totals.runs += runs;
        out.push(UsageBucket {
            start,
            end,
            usage,
            runs,
        });
    }
    Ok(UsageStats {
        period,
        totals,
        buckets: out,
    })
}

/// The old-to-new bucket boundaries for a period, ending at `now`.
fn usage_bucket_bounds(
    period: UsagePeriod,
    now: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    match period {
        UsagePeriod::Day => {
            let end = floor_to(now, HOUR_MS) + ChronoDuration::hours(1);
            (0..24)
                .map(|i| {
                    let start = end - ChronoDuration::hours(24 - i);
                    (start, start + ChronoDuration::hours(1))
                })
                .collect()
        }
        UsagePeriod::Week => day_buckets(now, 7),
        UsagePeriod::Month => day_buckets(now, 30),
        UsagePeriod::Year => month_buckets(now, 12),
    }
}

/// The bucket key SQLite's `strftime` groups by, mirrored in Rust so a bucket
/// can be matched without a second query.
fn usage_bucket_key(period: UsagePeriod, start: DateTime<Utc>) -> String {
    match period {
        UsagePeriod::Day => start.format("%Y-%m-%dT%H").to_string(),
        UsagePeriod::Week | UsagePeriod::Month => start.format("%Y-%m-%d").to_string(),
        UsagePeriod::Year => start.format("%Y-%m").to_string(),
    }
}

/// Truncate `now` down to a multiple of `step_ms` since the Unix epoch.
fn floor_to(now: DateTime<Utc>, step_ms: i64) -> DateTime<Utc> {
    let ms = now.timestamp_millis().div_euclid(step_ms) * step_ms;
    DateTime::from_timestamp_millis(ms).unwrap_or(now)
}

fn day_buckets(now: DateTime<Utc>, days: i64) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let end = floor_to(now, DAY_MS) + ChronoDuration::days(1);
    (0..days)
        .map(|i| {
            let start = end - ChronoDuration::days(days - i);
            (start, start + ChronoDuration::days(1))
        })
        .collect()
}

/// The first instant of `now`'s UTC calendar month.
fn month_floor(now: DateTime<Utc>) -> DateTime<Utc> {
    let date =
        NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap_or_else(|| now.date_naive());
    Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN))
}

/// `base` shifted by `delta` whole months (clamped to the first of the month).
fn add_months(base: DateTime<Utc>, delta: i32) -> DateTime<Utc> {
    let total = base.year() * 12 + base.month0() as i32 + delta;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    let date = NaiveDate::from_ymd_opt(year, month as u32, 1).unwrap_or_else(|| base.date_naive());
    Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN))
}

fn month_buckets(now: DateTime<Utc>, months: i32) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let end = add_months(month_floor(now), 1);
    (0..months)
        .map(|i| {
            let start = add_months(end, -(months - i));
            (start, add_months(start, 1))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// What a [`prune`] pass removed (and whether it reclaimed disk space).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneStats {
    pub outputs_cleared: u64,
    pub tasks_deleted: u64,
    pub runs_deleted: u64,
    pub dependencies_deleted: u64,
    pub events_deleted: u64,
    pub notifications_deleted: u64,
    pub webhook_deliveries_deleted: u64,
    pub vacuumed: bool,
}

/// Delete rows older than `days` (0 disables) while always keeping the newest
/// `min_tasks` task rows, then optionally reclaim disk space.
pub async fn prune(
    pool: &SqlitePool,
    days: u64,
    min_tasks: u64,
    vacuum: bool,
) -> anyhow::Result<PruneStats> {
    let mut stats = PruneStats::default();
    if days == 0 {
        return Ok(stats);
    }
    let cutoff = ts_ms(Utc::now() - ChronoDuration::days(days as i64));
    // Clear blobs first: cheap space reclaim, keeps task metadata.
    stats.outputs_cleared = sqlx::query(
        "UPDATE tasks SET output = NULL WHERE output IS NOT NULL \
         AND finished_at IS NOT NULL AND finished_at < ?",
    )
    .bind(cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    // Then delete old terminal rows, always keeping the newest `min_tasks`.
    stats.tasks_deleted = sqlx::query(
        "DELETE FROM tasks WHERE finished_at IS NOT NULL AND finished_at < ? \
         AND id NOT IN (SELECT id FROM tasks ORDER BY created_at DESC LIMIT ?)",
    )
    .bind(cutoff)
    .bind(min_tasks as i64)
    .execute(pool)
    .await?
    .rows_affected();
    // No foreign keys are enabled in SQLite, so cascade manually: any run whose
    // owning task row is gone is dead history.
    stats.runs_deleted =
        sqlx::query("DELETE FROM task_runs WHERE task_id NOT IN (SELECT id FROM tasks)")
            .execute(pool)
            .await?
            .rows_affected();
    // Dependency rows are meaningless once either endpoint is gone.
    stats.dependencies_deleted = sqlx::query(
        "DELETE FROM task_dependencies \
         WHERE task_id NOT IN (SELECT id FROM tasks) \
            OR depends_on NOT IN (SELECT id FROM tasks)",
    )
    .execute(pool)
    .await?
    .rows_affected();
    stats.events_deleted = sqlx::query("DELETE FROM events WHERE created_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    stats.notifications_deleted = sqlx::query("DELETE FROM notifications WHERE sent_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    stats.webhook_deliveries_deleted =
        sqlx::query("DELETE FROM webhook_deliveries WHERE received_at < ?")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected();
    if vacuum
        && (stats.outputs_cleared
            + stats.tasks_deleted
            + stats.runs_deleted
            + stats.dependencies_deleted
            + stats.events_deleted
            + stats.notifications_deleted
            + stats.webhook_deliveries_deleted)
            > 0
    {
        // VACUUM must not run inside a transaction; sqlx executes it in autocommit.
        if let Err(e) = sqlx::query("VACUUM").execute(pool).await {
            tracing::warn!(error = %e, "VACUUM failed; database is pruned but not compacted");
        } else {
            stats.vacuumed = true;
        }
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(pool)
            .await;
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// A git worktree favetto has created and not yet removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub task_id: Uuid,
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

/// Insert or refresh the tracking row for a task's worktree.
///
/// `INSERT OR REPLACE`: a task id maps to at most one worktree, and a leftover
/// directory may be reused by `create_worktree`.
pub async fn record_worktree(pool: &SqlitePool, wt: &WorktreeRecord) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO worktrees (task_id, repo, path, branch, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(wt.task_id.to_string())
    .bind(wt.repo.to_string_lossy().into_owned())
    .bind(wt.path.to_string_lossy().into_owned())
    .bind(&wt.branch)
    .bind(ts_ms(wt.created_at))
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop the tracking row after a worktree is removed.
pub async fn forget_worktree(pool: &SqlitePool, task_id: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM worktrees WHERE task_id = ?")
        .bind(task_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Every tracked worktree, oldest first.
pub async fn list_worktrees(pool: &SqlitePool) -> anyhow::Result<Vec<WorktreeRecord>> {
    let rows = sqlx::query(
        "SELECT task_id, repo, path, branch, created_at FROM worktrees ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_worktree).collect())
}

fn row_to_worktree(row: &SqliteRow) -> WorktreeRecord {
    WorktreeRecord {
        task_id: Uuid::parse_str(&row.get::<String, _>("task_id")).unwrap_or_default(),
        repo: PathBuf::from(row.get::<String, _>("repo")),
        path: PathBuf::from(row.get::<String, _>("path")),
        branch: row.get("branch"),
        created_at: from_ms(row.get("created_at")),
    }
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;

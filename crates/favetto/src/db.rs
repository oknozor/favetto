//! SQLite persistence via `sqlx`.
//!
//! Migrations are inline (`CREATE TABLE IF NOT EXISTS`) and applied at startup.
//!
//! Timestamps are stored as Unix epoch milliseconds (INTEGER) and JSON blobs as TEXT;
//! the conversion happens at the boundary so the domain model stays clean.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, NotificationRecord, Schedule, Task, TaskStatus};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    status      TEXT NOT NULL,
    input       TEXT NOT NULL,
    output      TEXT,
    dedupe_key  TEXT UNIQUE,
    created_at  INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    error       TEXT,
    session_id  TEXT,
    session_title TEXT,
    parent_id   TEXT,
    root_id     TEXT
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

CREATE INDEX IF NOT EXISTS idx_tasks_created_at ON tasks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_status_created_at ON tasks(status, created_at);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_notifications_sent_at ON notifications(sent_at);
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
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_root_id ON tasks(root_id)")
        .execute(pool)
        .await;
    Ok(())
}

/// Insert a task, silently ignoring a duplicate dedupe key.
pub async fn insert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task.id.to_string())
    .bind(&task.name)
    .bind(task.status.as_str())
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert-or-update a task (full overwrite of mutable fields).
pub async fn upsert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tasks (id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             status = excluded.status,
             output = excluded.output,
             started_at = excluded.started_at,
             finished_at = excluded.finished_at,
             error = excluded.error,
             session_id = excluded.session_id,
             session_title = excluded.session_title",
    )
    .bind(task.id.to_string())
    .bind(&task.name)
    .bind(task.status.as_str())
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single task by id.
pub async fn get_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id \
         FROM tasks WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task))
}

/// List the most recent tasks (newest first), without their output blobs. Output
/// is fetched on demand with [`get_task`].
pub async fn list_tasks(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, input, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id \
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
    }
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

/// Mark tasks left `running` or `awaiting_input` by a previous daemon instance as
/// failed — they were interrupted and the agent process is gone. Returns the
/// number reconciled.
pub async fn fail_interrupted_tasks(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'failed', error = 'interrupted by daemon restart', finished_at = ? \
         WHERE status IN ('running', 'awaiting_input')",
    )
    .bind(ts_ms(Utc::now()))
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Up to `limit` pending tasks, oldest first (for the parallel executor).
pub async fn next_pending_tasks(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, input, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id \
         FROM tasks WHERE status = 'pending' ORDER BY created_at ASC LIMIT ?",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
}

/// Atomically claim a pending task for execution. Returns false if it was already
/// claimed (e.g. by another dispatcher tick).
pub async fn claim_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'running', started_at = ? WHERE id = ? AND status = 'pending'",
    )
    .bind(ts_ms(Utc::now()))
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
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
        "SELECT id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id \
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
        "SELECT id, name, status, input, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id \
         FROM tasks WHERE (root_id = ? OR id = ?) AND name = ? AND status IN ('pending', 'running', 'awaiting_input') ORDER BY created_at ASC",
    )
    .bind(root_id.to_string())
    .bind(root_id.to_string())
    .bind(name)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_task).collect())
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
// Retention
// ---------------------------------------------------------------------------

/// What a [`prune`] pass removed (and whether it reclaimed disk space).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneStats {
    pub outputs_cleared: u64,
    pub tasks_deleted: u64,
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

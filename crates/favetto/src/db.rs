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
mod tests {
    use super::*;
    use chrono::Utc;
    use favetto_core::model::Event;

    #[tokio::test]
    async fn event_replay_resumes_from_cursor() {
        let dir = std::env::temp_dir().join(format!("favetto-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("test.db")).await.unwrap();
        migrate(&pool).await.unwrap();

        for i in 0..5 {
            let ev = Event {
                id: 0,
                kind: EventKind::Unknown,
                payload: serde_json::json!({ "n": i }),
                created_at: Utc::now(),
            };
            let id = insert_event(&pool, &ev).await.unwrap();
            assert_eq!(id, i + 1);
        }

        // Replay after cursor 2 replays events 3, 4, 5 in ascending order.
        let replay = events_after(&pool, 2, 100).await.unwrap();
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].id, 3);
        assert_eq!(replay[2].id, 5);

        // Tail returns the newest, in ascending order.
        let tail = tail_events(&pool, 3).await.unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].id, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn row_to_event_falls_back_to_unknown() {
        let (dir, pool) = scratch_pool("unknown-kind").await;
        // A kind an older or newer build wrote that this build does not know.
        sqlx::query(
            "INSERT INTO events (kind, payload, created_at) VALUES ('not_a_kind', '{}', ?)",
        )
        .bind(ts_ms(Utc::now()))
        .execute(&pool)
        .await
        .unwrap();

        let events = events_after(&pool, 0, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Unknown);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn webhook_delivery_is_recorded_once() {
        let dir = std::env::temp_dir().join(format!("favetto-delivery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("test.db")).await.unwrap();
        migrate(&pool).await.unwrap();

        assert!(record_delivery(&pool, "github", "delivery-1")
            .await
            .unwrap());
        assert!(!record_delivery(&pool, "github", "delivery-1")
            .await
            .unwrap());
        assert!(record_delivery(&pool, "github", "delivery-2")
            .await
            .unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn task_session_id_round_trips() {
        let dir = std::env::temp_dir().join(format!("favetto-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("test.db")).await.unwrap();
        migrate(&pool).await.unwrap();

        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: Some("ses_123".to_string()),
            session_title: Some("Fix the widget".to_string()),
            parent_id: None,
            root_id: None,
        };
        upsert_task(&pool, &task).await.unwrap();
        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.session_id.as_deref(), Some("ses_123"));
        assert_eq!(got.session_title.as_deref(), Some("Fix the widget"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn migrate_adds_session_title_to_legacy_database() {
        let dir = std::env::temp_dir().join(format!("favetto-legacy-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("test.db")).await.unwrap();

        // A pre-migration database: the tasks table predates `session_title`.
        sqlx::query(
            "CREATE TABLE tasks (
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
                session_id  TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Running the migration twice is a no-op the second time.
        migrate(&pool).await.unwrap();
        migrate(&pool).await.unwrap();

        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: Some("ses_legacy".to_string()),
            session_title: Some("Legacy title".to_string()),
            parent_id: Some(Uuid::new_v4()),
            root_id: Some(Uuid::new_v4()),
        };
        upsert_task(&pool, &task).await.unwrap();
        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.session_title.as_deref(), Some("Legacy title"));
        assert_eq!(got.parent_id, task.parent_id);
        assert_eq!(got.root_id, task.root_id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn scratch_pool(tag: &str) -> (std::path::PathBuf, SqlitePool) {
        let dir = std::env::temp_dir().join(format!(
            "favetto-{tag}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("test.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        (dir, pool)
    }

    fn task_at(
        created_at: DateTime<Utc>,
        finished_at: Option<DateTime<Utc>>,
        output: Option<serde_json::Value>,
    ) -> Task {
        Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({}),
            output,
            dedupe_key: None,
            created_at,
            started_at: Some(created_at),
            finished_at,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        }
    }

    #[tokio::test]
    async fn list_tasks_omits_output_but_get_task_keeps_it() {
        let (dir, pool) = scratch_pool("list-omits-output").await;

        let task = task_at(
            Utc::now(),
            Some(Utc::now()),
            Some(serde_json::json!({ "output": "x".repeat(50_000) })),
        );
        upsert_task(&pool, &task).await.unwrap();

        let listed = list_tasks(&pool, 500).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].output.is_none(), "list must not carry output");

        let fetched = get_task(&pool, task.id).await.unwrap().unwrap();
        assert!(
            fetched.output.is_some(),
            "tasks.get must keep the output blob"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn open_enables_wal() {
        let (dir, pool) = scratch_pool("wal").await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn migrate_creates_task_indexes() {
        let (dir, pool) = scratch_pool("indexes").await;
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
                .fetch_all(&pool)
                .await
                .unwrap();
        for expected in [
            "idx_tasks_created_at",
            "idx_tasks_status_created_at",
            "idx_tasks_root_id",
            "idx_events_created_at",
            "idx_notifications_sent_at",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing index `{expected}` in {names:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prune_keeps_newest_min_tasks_and_deletes_old_rows() {
        let (dir, pool) = scratch_pool("prune-tasks").await;
        let old = Utc::now() - ChronoDuration::days(60);
        // Four terminal tasks, oldest first. `min_tasks = 2` keeps the newest two.
        let mut ids = Vec::new();
        for i in 0..4 {
            let created = old + ChronoDuration::hours(i);
            let task = task_at(created, Some(created), None);
            ids.push(task.id);
            upsert_task(&pool, &task).await.unwrap();
        }
        // An old event and notification, plus one fresh of each.
        sqlx::query("INSERT INTO events (kind, payload, created_at) VALUES ('unknown', '{}', ?)")
            .bind(ts_ms(old))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO notifications (channel, subject, body, status, sent_at) VALUES ('c', 's', 'b', 'sent', ?)")
            .bind(ts_ms(old))
            .execute(&pool)
            .await
            .unwrap();

        let stats = prune(&pool, 30, 2, false).await.unwrap();
        assert_eq!(stats.tasks_deleted, 2);
        assert_eq!(stats.events_deleted, 1);
        assert_eq!(stats.notifications_deleted, 1);
        assert!(!stats.vacuumed);

        let remaining = list_tasks(&pool, 100).await.unwrap();
        assert_eq!(remaining.len(), 2);
        let remaining_ids: Vec<Uuid> = remaining.iter().map(|t| t.id).collect();
        assert!(remaining_ids.contains(&ids[2]));
        assert!(remaining_ids.contains(&ids[3]));

        // `days = 0` is a no-op.
        let noop = prune(&pool, 0, 0, true).await.unwrap();
        assert_eq!(noop, PruneStats::default());
        assert_eq!(list_tasks(&pool, 100).await.unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prune_clears_old_outputs() {
        let (dir, pool) = scratch_pool("prune-outputs").await;
        let old = Utc::now() - ChronoDuration::days(60);
        let task = task_at(
            old,
            Some(old),
            Some(serde_json::json!({ "big": "x".repeat(1000) })),
        );
        upsert_task(&pool, &task).await.unwrap();

        // A high `min_tasks` keeps the row, but its old output is still cleared.
        let stats = prune(&pool, 30, 1000, false).await.unwrap();
        assert_eq!(stats.tasks_deleted, 0);
        assert_eq!(stats.outputs_cleared, 1);

        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert!(got.output.is_none());
        assert_eq!(list_tasks(&pool, 100).await.unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn task_awaiting_input_round_trips() {
        let (dir, pool) = scratch_pool("awaiting-roundtrip").await;
        let mut task = task_at(Utc::now(), None, None);
        task.status = TaskStatus::AwaitingInput;
        upsert_task(&pool, &task).await.unwrap();

        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::AwaitingInput);
        // The wire string is stable.
        assert_eq!(got.status.as_str(), "awaiting_input");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn task_lineage_round_trips() {
        let (dir, pool) = scratch_pool("lineage-roundtrip").await;
        let root = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let mut task = task_at(Utc::now(), None, None);
        task.name = "target".to_string();
        task.parent_id = Some(parent);
        task.root_id = Some(root);
        upsert_task(&pool, &task).await.unwrap();

        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.parent_id, Some(parent));
        assert_eq!(got.root_id, Some(root));

        // The root task itself has no `root_id` but is its own root.
        let mut root_task = task_at(Utc::now(), Some(Utc::now()), None);
        root_task.name = "target".to_string();
        root_task.status = TaskStatus::Succeeded;
        upsert_task(&pool, &root_task).await.unwrap();
        assert_eq!(root_task.root_or_self(), root_task.id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_task_status_if_is_conditional() {
        let (dir, pool) = scratch_pool("status-if").await;
        let mut task = task_at(Utc::now(), None, None);
        task.status = TaskStatus::Running;
        upsert_task(&pool, &task).await.unwrap();

        assert!(set_task_status_if(
            &pool,
            task.id,
            TaskStatus::AwaitingInput,
            TaskStatus::Running
        )
        .await
        .unwrap());
        assert_eq!(
            get_task(&pool, task.id).await.unwrap().unwrap().status,
            TaskStatus::AwaitingInput
        );

        // Wrong source status: no-op.
        assert!(!set_task_status_if(
            &pool,
            task.id,
            TaskStatus::AwaitingInput,
            TaskStatus::Running
        )
        .await
        .unwrap());
        assert_eq!(
            get_task(&pool, task.id).await.unwrap().unwrap().status,
            TaskStatus::AwaitingInput
        );

        assert!(set_task_status_if(
            &pool,
            task.id,
            TaskStatus::Running,
            TaskStatus::AwaitingInput
        )
        .await
        .unwrap());
        assert_eq!(
            get_task(&pool, task.id).await.unwrap().unwrap().status,
            TaskStatus::Running
        );

        // Unknown id: no row matched.
        assert!(!set_task_status_if(
            &pool,
            Uuid::new_v4(),
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
        )
        .await
        .unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_tasks_in_root_scopes_by_root_and_name() {
        let (dir, pool) = scratch_pool("root-queries").await;
        let root_a = Uuid::new_v4();
        let root_b = Uuid::new_v4();

        // Two `target` children of root A (one terminal, one active) plus one of
        // root B, and an unrelated name in root A.
        let mut done = task_at(
            Utc::now(),
            Some(Utc::now()),
            Some(serde_json::json!({ "ok": true })),
        );
        done.name = "target".to_string();
        done.root_id = Some(root_a);
        done.parent_id = Some(root_a);
        let mut running = task_at(Utc::now(), None, None);
        running.name = "target".to_string();
        running.status = TaskStatus::Running;
        running.root_id = Some(root_a);
        running.parent_id = Some(root_a);
        let mut other_root = task_at(Utc::now(), Some(Utc::now()), None);
        other_root.name = "target".to_string();
        other_root.root_id = Some(root_b);
        other_root.parent_id = Some(root_b);
        let mut noise = task_at(Utc::now(), Some(Utc::now()), None);
        noise.name = "unrelated".to_string();
        noise.root_id = Some(root_a);
        for task in [&done, &running, &other_root, &noise] {
            upsert_task(&pool, task).await.unwrap();
        }

        let all = list_tasks_in_root(&pool, root_a, "target").await.unwrap();
        assert_eq!(all.len(), 2);
        // Output blobs are included for the aggregate.
        assert!(all.iter().any(|t| t.id == done.id && t.output.is_some()));

        let active = list_active_tasks_in_root(&pool, root_a, "target")
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, running.id);

        // A terminal-only root has an empty barrier.
        let root_c = Uuid::new_v4();
        let mut only_done = task_at(Utc::now(), Some(Utc::now()), None);
        only_done.name = "target".to_string();
        only_done.root_id = Some(root_c);
        upsert_task(&pool, &only_done).await.unwrap();
        assert!(list_active_tasks_in_root(&pool, root_c, "target")
            .await
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_task_session_never_touches_status() {
        let (dir, pool) = scratch_pool("set-session").await;
        let mut task = task_at(Utc::now(), None, None);
        task.status = TaskStatus::AwaitingInput;
        task.output = Some(serde_json::json!({ "keep": true }));
        upsert_task(&pool, &task).await.unwrap();

        assert!(set_task_session(&pool, task.id, "ses_1", Some("Title"))
            .await
            .unwrap());
        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::AwaitingInput);
        assert_eq!(got.session_id.as_deref(), Some("ses_1"));
        assert_eq!(got.session_title.as_deref(), Some("Title"));
        assert_eq!(got.output, Some(serde_json::json!({ "keep": true })));

        // A second call must not overwrite the already-stored values (COALESCE).
        set_task_session(&pool, task.id, "ses_2", Some("Other"))
            .await
            .unwrap();
        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.session_id.as_deref(), Some("ses_1"));
        assert_eq!(got.session_title.as_deref(), Some("Title"));

        // Unknown id: no row matched.
        assert!(!set_task_session(&pool, Uuid::new_v4(), "ses_x", None)
            .await
            .unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fail_interrupted_tasks_marks_running_and_awaiting_input() {
        let (dir, pool) = scratch_pool("interrupted").await;
        let mut running = task_at(Utc::now(), None, None);
        running.status = TaskStatus::Running;
        let mut awaiting = task_at(Utc::now(), None, None);
        awaiting.status = TaskStatus::AwaitingInput;
        let done = task_at(Utc::now(), Some(Utc::now()), None);
        for task in [&running, &awaiting, &done] {
            upsert_task(&pool, task).await.unwrap();
        }

        assert_eq!(fail_interrupted_tasks(&pool).await.unwrap(), 2);

        for id in [running.id, awaiting.id] {
            let got = get_task(&pool, id).await.unwrap().unwrap();
            assert_eq!(got.status, TaskStatus::Failed);
            assert_eq!(got.error.as_deref(), Some("interrupted by daemon restart"));
        }
        // A terminal task is untouched.
        assert_eq!(
            get_task(&pool, done.id).await.unwrap().unwrap().status,
            TaskStatus::Succeeded
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn worktree_record_round_trips() {
        let (dir, pool) = scratch_pool("worktree-roundtrip").await;
        let now = Utc::now();
        let first = WorktreeRecord {
            task_id: Uuid::new_v4(),
            repo: PathBuf::from("/repo/one"),
            path: PathBuf::from("/data/worktrees/one"),
            branch: "favetto/one-1234".to_string(),
            created_at: now - ChronoDuration::hours(1),
        };
        let second = WorktreeRecord {
            task_id: Uuid::new_v4(),
            repo: PathBuf::from("/repo/two"),
            path: PathBuf::from("/data/worktrees/two"),
            branch: "favetto/two-5678".to_string(),
            created_at: now,
        };
        record_worktree(&pool, &second).await.unwrap();
        record_worktree(&pool, &first).await.unwrap();

        let listed = list_worktrees(&pool).await.unwrap();
        assert_eq!(listed.len(), 2);
        // Oldest first; timestamps round-trip at millisecond precision.
        assert_eq!(listed[0].task_id, first.task_id);
        assert_eq!(listed[0].repo, first.repo);
        assert_eq!(listed[0].path, first.path);
        assert_eq!(listed[0].branch, first.branch);
        assert_eq!(
            listed[0].created_at.timestamp_millis(),
            first.created_at.timestamp_millis()
        );
        assert_eq!(listed[1].task_id, second.task_id);

        // Re-recording the same task id replaces, not appends.
        let mut replacement = first.clone();
        replacement.branch = "favetto/one-9999".to_string();
        record_worktree(&pool, &replacement).await.unwrap();
        let listed = list_worktrees(&pool).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].branch, "favetto/one-9999");

        // Forgetting removes exactly one row.
        forget_worktree(&pool, first.task_id).await.unwrap();
        let listed = list_worktrees(&pool).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].task_id, second.task_id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn migrate_creates_worktrees_table() {
        let (dir, pool) = scratch_pool("worktrees-table").await;
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            names.iter().any(|n| n == "worktrees"),
            "missing worktrees table in {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

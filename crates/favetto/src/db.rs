//! SQLite persistence via `sqlx`.
//!
//! Migrations are inline (`CREATE TABLE IF NOT EXISTS`) and applied at startup.
//!
//! Timestamps are stored as Unix epoch milliseconds (INTEGER) and JSON blobs as TEXT;
//! the conversion happens at the boundary so the domain model stays clean.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
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
    session_title TEXT
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
        .create_if_missing(true);
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
    Ok(())
}

/// Insert a task, silently ignoring a duplicate dedupe key.
pub async fn insert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert-or-update a task (full overwrite of mutable fields).
pub async fn upsert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tasks (id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single task by id.
pub async fn get_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title \
         FROM tasks WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_task))
}

/// List the most recent tasks (newest first).
pub async fn list_tasks(pool: &SqlitePool) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title \
         FROM tasks ORDER BY created_at DESC LIMIT 500",
    )
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
            .get::<Option<String>, _>("output")
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
    }
}

fn row_to_event(row: &SqliteRow) -> Event {
    Event {
        id: row.get("id"),
        kind: row
            .get::<String, _>("kind")
            .parse()
            .unwrap_or(EventKind::Synthetic),
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

/// Mark tasks left `running` by a previous daemon instance as failed — they were
/// interrupted and the agent process is gone. Returns the number reconciled.
pub async fn fail_interrupted_tasks(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "UPDATE tasks SET status = 'failed', error = 'interrupted by daemon restart', finished_at = ? \
         WHERE status = 'running'",
    )
    .bind(ts_ms(Utc::now()))
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Up to `limit` pending tasks, oldest first (for the parallel executor).
pub async fn next_pending_tasks(pool: &SqlitePool, limit: i64) -> anyhow::Result<Vec<Task>> {
    let rows = sqlx::query(
        "SELECT id, name, status, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title \
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
                kind: EventKind::Synthetic,
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
        };
        upsert_task(&pool, &task).await.unwrap();
        let got = get_task(&pool, task.id).await.unwrap().unwrap();
        assert_eq!(got.session_title.as_deref(), Some("Legacy title"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

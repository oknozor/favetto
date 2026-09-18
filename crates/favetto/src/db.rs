//! SQLite persistence via `sqlx`.
//!
//! M1 keeps migrations inline (`CREATE TABLE IF NOT EXISTS`). A dedicated migration
//! tool (refinery or `sqlx::migrate!`) is introduced in M2 once the schema grows the
//! `mcp_servers` / `mcp_tool_perms` / `hooks` tables.
//!
//! Timestamps are stored as Unix epoch milliseconds (INTEGER) and JSON blobs as TEXT;
//! the conversion happens at the boundary so the domain model stays clean.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    skill       TEXT NOT NULL,
    status      TEXT NOT NULL,
    input       TEXT NOT NULL,
    output      TEXT,
    dedupe_key  TEXT UNIQUE,
    created_at  INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    error       TEXT
);

CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_id ON events(id);
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
    Ok(())
}

/// Insert a task, silently ignoring a duplicate dedupe key.
pub async fn insert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, skill, status, input, output, dedupe_key, created_at, started_at, finished_at, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(task.id.to_string())
    .bind(&task.skill)
    .bind(task.status.as_str())
    .bind(serde_json::to_string(&task.input)?)
    .bind(task.output.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&task.dedupe_key)
    .bind(ts_ms(task.created_at))
    .bind(task.started_at.map(ts_ms))
    .bind(task.finished_at.map(ts_ms))
    .bind(&task.error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert-or-update a task (full overwrite of mutable fields).
pub async fn upsert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tasks (id, skill, status, input, output, dedupe_key, created_at, started_at, finished_at, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             status = excluded.status,
             output = excluded.output,
             started_at = excluded.started_at,
             finished_at = excluded.finished_at,
             error = excluded.error",
    )
    .bind(task.id.to_string())
    .bind(&task.skill)
    .bind(task.status.as_str())
    .bind(serde_json::to_string(&task.input)?)
    .bind(task.output.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&task.dedupe_key)
    .bind(ts_ms(task.created_at))
    .bind(task.started_at.map(ts_ms))
    .bind(task.finished_at.map(ts_ms))
    .bind(&task.error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single task by id.
pub async fn get_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT id, skill, status, input, output, dedupe_key, created_at, started_at, finished_at, error \
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
        "SELECT id, skill, status, input, output, dedupe_key, created_at, started_at, finished_at, error \
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
    let rows = sqlx::query("SELECT id, kind, payload, created_at FROM events ORDER BY id DESC LIMIT ?")
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
        skill: row.get("skill"),
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

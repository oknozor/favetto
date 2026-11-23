//! SQLite persistence via `sqlx`.
//!
//! Migrations are inline (`CREATE TABLE IF NOT EXISTS`) and applied at startup.
//!
//! Timestamps are stored as Unix epoch milliseconds (INTEGER) and JSON blobs as TEXT;
//! the conversion happens at the boundary so the domain model stays clean.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use favetto_core::Timestamp;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};
use time::{Date, Duration as TimeDuration, Month, PrimitiveDateTime, Time};
use uuid::Uuid;

use favetto_core::agent_state::{
    AgentCost, AgentUsage, UsageBucket, UsagePeriod, UsageStats, UsageTotals,
};
use favetto_core::model::{
    Event, EventKind, Failure, NotificationRecord, RepoSource, RunStatus, Schedule, Task, TaskRun,
    TaskStatus,
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
    agent       TEXT,
    provider    TEXT,
    model       TEXT,
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

-- Known repositories whose `.favetto/tasks/` catalogs the daemon loads. `path`
-- is the absolute daemon-host directory; `name` is the namespace key used in
-- `repo:<name>/<path>` task identities and is unique. `source` records how the
-- repo entered the registry: `manual` (a `[[repos.known]]` seed or `repos.add`)
-- or `scanned` (found under `[repos].base_dir`). Only scanned rows are reclaimed
-- when their directory vanishes.
CREATE TABLE IF NOT EXISTS known_repos (
    path     TEXT PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE,
    added_at INTEGER NOT NULL,
    source   TEXT NOT NULL DEFAULT 'manual'
);

-- Small key/value store for daemon-wide settings that survive restarts (the
-- active repo is key `active_repo`).
CREATE TABLE IF NOT EXISTS daemon_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Username/password accounts for remote login. `password_hash` is an argon2id
-- value produced by `favetto_core::auth::hash_password`; plaintext passwords are
-- never stored.
CREATE TABLE IF NOT EXISTS users (
    username      TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
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
    cost_usd            REAL,
    cost_amount         REAL,
    cost_currency       TEXT,
    context_used        INTEGER,
    context_size        INTEGER
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

-- Durable metadata for every ACP session Favetto creates. The agent remains the
-- source of truth for conversation history: this table stores identity and
-- lifecycle only, never transcript content, environment variables, or
-- credentials. `id` is Favetto's stable local id (the key its RPCs use);
-- `external_session_id` is the agent's own ACP session id and is only unique
-- within one configured agent, hence the composite index below. `task_id` and
-- `run_id` are optional because interactive sessions are not task-backed.
CREATE TABLE IF NOT EXISTS agent_sessions (
    id                  TEXT PRIMARY KEY,
    agent               TEXT NOT NULL,
    external_session_id TEXT,
    cwd                 TEXT NOT NULL,
    task_id             TEXT,
    run_id              TEXT,
    title               TEXT,
    status              TEXT NOT NULL,
    error               TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_dependencies_task_id ON task_dependencies(task_id);
CREATE INDEX IF NOT EXISTS idx_task_dependencies_depends_on ON task_dependencies(depends_on);
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_sessions_agent_external
    ON agent_sessions(agent, external_session_id)
    WHERE external_session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_agent_sessions_updated_at ON agent_sessions(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_agent_sessions_task_id ON agent_sessions(task_id);
CREATE INDEX IF NOT EXISTS idx_tasks_dedupe_key ON tasks(dedupe_key);
CREATE INDEX IF NOT EXISTS idx_tasks_created_at ON tasks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_status_created_at ON tasks(status, created_at);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_notifications_sent_at ON notifications(sent_at);
CREATE INDEX IF NOT EXISTS idx_task_runs_task_attempt ON task_runs(task_id, attempt);
CREATE INDEX IF NOT EXISTS idx_task_runs_status ON task_runs(status);
CREATE INDEX IF NOT EXISTS idx_task_runs_finished_at ON task_runs(finished_at);
"#;

fn ts_ms(dt: Timestamp) -> i64 {
    dt.unix_timestamp_millis()
}

fn from_ms(ms: i64) -> Timestamp {
    Timestamp::from_unix_timestamp_millis(ms).unwrap_or_else(Timestamp::now)
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
    // Per-launch agent/provider/model overrides. Legacy rows keep NULL and
    // resolve from the catalog definition and global config as before.
    for column in ["agent TEXT", "provider TEXT", "model TEXT"] {
        // `column` is one of the fixed literals above, never user input.
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE tasks ADD COLUMN {column}"
        )))
        .execute(pool)
        .await;
    }
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
        "cost_amount REAL",
        "cost_currency TEXT",
        "context_used INTEGER",
        "context_size INTEGER",
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
    // Registry origin. Legacy rows default to `manual`, so a database created
    // before the base-directory scan existed never has its repos reclaimed.
    let _ = sqlx::query("ALTER TABLE known_repos ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'")
        .execute(pool)
        .await;
    Ok(())
}

/// Insert a task, silently ignoring a duplicate dedupe key.
pub async fn insert_task(pool: &SqlitePool, task: &Task) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tasks (id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id, interactive, agent, provider, model, failure)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(&task.agent)
    .bind(&task.provider)
    .bind(&task.model)
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
        "INSERT INTO tasks (id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, session_id, session_title, parent_id, root_id, interactive, agent, provider, model, failure)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
    .bind(&task.agent)
    .bind(&task.provider)
    .bind(&task.model)
    .bind(task.failure.as_ref().map(serde_json::to_string).transpose()?)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a single task by id.
pub async fn get_task(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<Task>> {
    let row = sqlx::query(
        "SELECT t.id, t.name, t.status, t.attempt, t.input, t.output, t.dedupe_key, t.created_at, t.started_at, t.finished_at, t.error, t.failure, t.session_id, t.session_title, t.parent_id, t.root_id, t.interactive, t.agent, t.provider, t.model, \
                r.input_tokens, r.output_tokens, r.reasoning_tokens, r.cache_read_tokens, r.cache_write_tokens, \
                r.cost_usd, r.cost_amount, r.cost_currency, r.context_used, r.context_size \
         FROM tasks t \
         LEFT JOIN task_runs r ON r.task_id = t.id AND r.attempt = t.attempt \
         WHERE t.id = ?",
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
        "SELECT id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
        "SELECT t.id, t.name, t.status, t.attempt, t.input, t.dedupe_key, t.created_at, t.started_at, t.finished_at, t.error, t.failure, t.session_id, t.session_title, t.parent_id, t.root_id, t.interactive, t.agent, t.provider, t.model, \
                r.input_tokens, r.output_tokens, r.reasoning_tokens, r.cache_read_tokens, r.cache_write_tokens, \
                r.cost_usd, r.cost_amount, r.cost_currency, r.context_used, r.context_size \
         FROM tasks t \
         LEFT JOIN task_runs r ON r.task_id = t.id AND r.attempt = t.attempt \
         ORDER BY t.created_at DESC LIMIT ?",
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
        agent: row.try_get::<Option<String>, _>("agent").ok().flatten(),
        provider: row.try_get::<Option<String>, _>("provider").ok().flatten(),
        model: row.try_get::<Option<String>, _>("model").ok().flatten(),
        latest_usage: usage_from_row(row),
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
    let cost_usd = row.try_get::<Option<f64>, _>("cost_usd").ok().flatten();
    let cost_amount = row.try_get::<Option<f64>, _>("cost_amount").ok().flatten();
    let cost_currency = row
        .try_get::<Option<String>, _>("cost_currency")
        .ok()
        .flatten();
    let context_used = row.try_get::<Option<i64>, _>("context_used").ok().flatten();
    let context_size = row.try_get::<Option<i64>, _>("context_size").ok().flatten();
    // New rows store the currency-qualified amount separately so a non-USD unit
    // is never mistaken for dollars. Legacy rows only have `cost_usd`, which was
    // always USD.
    let cost = match cost_amount {
        Some(amount) => Some(AgentCost::new(
            amount,
            cost_currency.unwrap_or_else(|| AgentCost::USD.to_string()),
        )),
        None => cost_usd.map(AgentCost::usd),
    };
    if input.is_none()
        && output.is_none()
        && reasoning.is_none()
        && cache_read.is_none()
        && cache_write.is_none()
        && cost.is_none()
        && context_used.is_none()
        && context_size.is_none()
    {
        return None;
    }
    Some(AgentUsage {
        input_tokens: input.unwrap_or(0).max(0) as u64,
        output_tokens: output.unwrap_or(0).max(0) as u64,
        reasoning_tokens: reasoning.unwrap_or(0).max(0) as u64,
        cache_read_tokens: cache_read.unwrap_or(0).max(0) as u64,
        cache_write_tokens: cache_write.unwrap_or(0).max(0) as u64,
        cost_usd,
        cost,
        context_used: context_used.map(|v| v.max(0) as u64),
        context_size: context_size.map(|v| v.max(0) as u64),
    })
}

/// Token and context values for the flat `task_runs` columns.
///
/// A context-only `usage_update` has no billable counters. Storing `0` for them
/// would make the run indistinguishable from a genuine zero-token run and would
/// pollute `usage.stats`; all five token columns are therefore NULL unless the
/// usage carries at least one counter. Reported zeros within a run that does
/// have counters are preserved as `0`.
fn usage_token_columns(usage: Option<&AgentUsage>) -> [Option<i64>; 5] {
    match usage {
        Some(usage) if usage.has_token_counts() => [
            Some(usage.input_tokens as i64),
            Some(usage.output_tokens as i64),
            Some(usage.reasoning_tokens as i64),
            Some(usage.cache_read_tokens as i64),
            Some(usage.cache_write_tokens as i64),
        ],
        _ => [None; 5],
    }
}

fn usage_context_columns(usage: Option<&AgentUsage>) -> (Option<i64>, Option<i64>) {
    match usage {
        Some(usage) => (
            usage.context_used.map(|v| v as i64),
            usage.context_size.map(|v| v as i64),
        ),
        None => (None, None),
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

pub async fn clear_schedule_job_id(pool: &SqlitePool, id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE schedules SET job_id = NULL WHERE id = ?")
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
        .bind(ts_ms(Timestamp::now()))
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
        next_run: None,
    }
}

// ---------------------------------------------------------------------------
// Known repos + daemon state
// ---------------------------------------------------------------------------

/// A persisted known-repo row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub name: String,
    pub path: String,
    pub source: RepoSource,
}

/// Parse a `known_repos.source` text column; anything but `scanned` is `manual`.
fn parse_repo_source(raw: &str) -> RepoSource {
    match raw {
        "scanned" => RepoSource::Scanned,
        _ => RepoSource::Manual,
    }
}

/// The lowercase text form of a [`RepoSource`].
fn repo_source_str(source: RepoSource) -> &'static str {
    match source {
        RepoSource::Manual => "manual",
        RepoSource::Scanned => "scanned",
    }
}

/// List known repositories, sorted by name.
pub async fn list_repos(pool: &SqlitePool) -> anyhow::Result<Vec<RepoRow>> {
    let rows = sqlx::query("SELECT name, path, source FROM known_repos ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| RepoRow {
            name: r.get::<String, _>("name"),
            path: r.get::<String, _>("path"),
            source: parse_repo_source(&r.get::<String, _>("source")),
        })
        .collect())
}

/// The `name` of the known repo registered at `path`, if any.
pub async fn repo_name_for_path(pool: &SqlitePool, path: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT name FROM known_repos WHERE path = ?")
        .bind(path)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("name")))
}

/// Insert a known repo. Fails on a duplicate `name` or `path`.
pub async fn insert_repo(
    pool: &SqlitePool,
    name: &str,
    path: &str,
    source: RepoSource,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO known_repos (name, path, added_at, source) VALUES (?, ?, ?, ?)")
        .bind(name)
        .bind(path)
        .bind(ts_ms(Timestamp::now()))
        .bind(repo_source_str(source))
        .execute(pool)
        .await?;
    Ok(())
}

/// Change a repo's registry origin (e.g. promote a scanned repo to `manual`
/// when it is explicitly added or seeded).
pub async fn set_repo_source(
    pool: &SqlitePool,
    name: &str,
    source: RepoSource,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE known_repos SET source = ? WHERE name = ?")
        .bind(repo_source_str(source))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete a known repo by name. Returns whether a row was removed.
pub async fn delete_repo(pool: &SqlitePool, name: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM known_repos WHERE name = ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The daemon-wide active repo name, if set.
pub async fn get_active_repo(pool: &SqlitePool) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM daemon_state WHERE key = 'active_repo'")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

/// Persist (or clear, when `name` is `None`) the daemon-wide active repo.
pub async fn set_active_repo(pool: &SqlitePool, name: Option<&str>) -> anyhow::Result<()> {
    match name {
        Some(name) => {
            sqlx::query(
                "INSERT INTO daemon_state (key, value) VALUES ('active_repo', ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(name)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM daemon_state WHERE key = 'active_repo'")
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// A daemon user account. The password hash is never exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub username: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Create a user, or replace the password of an existing one.
///
/// `created_at` is preserved on update; `updated_at` always advances.
pub async fn upsert_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> anyhow::Result<()> {
    let now = ts_ms(Timestamp::now());
    sqlx::query(
        "INSERT INTO users (username, password_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(username) DO UPDATE SET
             password_hash = excluded.password_hash,
             updated_at = excluded.updated_at",
    )
    .bind(username)
    .bind(password_hash)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored argon2id hash for `username`, if the account exists.
pub async fn get_user_hash(pool: &SqlitePool, username: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT password_hash FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("password_hash")))
}

/// The number of configured users.
pub async fn count_users(pool: &SqlitePool) -> anyhow::Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM users")
        .fetch_one(pool)
        .await?;
    Ok(row.get::<i64, _>("n"))
}

/// Every user, ordered by username.
pub async fn list_users(pool: &SqlitePool) -> anyhow::Result<Vec<UserRecord>> {
    let rows = sqlx::query("SELECT username, created_at, updated_at FROM users ORDER BY username")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| UserRecord {
            username: row.get("username"),
            created_at: from_ms(row.get("created_at")),
            updated_at: from_ms(row.get("updated_at")),
        })
        .collect())
}

/// Delete `username`. Returns whether a row was removed.
pub async fn remove_user(pool: &SqlitePool, username: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM users WHERE username = ?")
        .bind(username)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
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
    .bind(ts_ms(Timestamp::now()))
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
    .bind(ts_ms(Timestamp::now()))
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
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
    .bind(ts_ms(Timestamp::now()))
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
    .bind(ts_ms(Timestamp::now()))
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
    retry_at: Timestamp,
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
    .bind(ts_ms(Timestamp::now()))
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
    .bind(ts_ms(Timestamp::now()))
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
        "SELECT t.id, t.name, t.status, t.attempt, t.input, t.dedupe_key, t.created_at, t.started_at, t.finished_at, t.error, t.failure, t.session_id, t.session_title, t.parent_id, t.root_id, t.interactive, t.agent, t.provider, t.model \
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
    .bind(ts_ms(Timestamp::now()))
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
    let now = Timestamp::now();
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
        "SELECT id, name, status, attempt, input, output, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
        "SELECT id, name, status, attempt, input, dedupe_key, created_at, started_at, finished_at, error, failure, session_id, session_title, parent_id, root_id, interactive, agent, provider, model \
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
    created_at: Timestamp,
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

// ---------------------------------------------------------------------------
// Task runs
// ---------------------------------------------------------------------------

/// Split a usage's reported cost into the legacy USD column, the
/// currency-qualified amount, and its unit. Non-USD amounts leave `cost_usd`
/// NULL so they can never be summed with dollars or rendered as USD.
fn usage_cost_columns(usage: Option<&AgentUsage>) -> (Option<f64>, Option<f64>, Option<String>) {
    let Some(cost) = usage.and_then(|usage| usage.reported_cost()) else {
        return (None, None, None);
    };
    let usd = cost.is_usd().then_some(cost.amount);
    (usd, Some(cost.amount), Some(cost.currency))
}

/// Insert a run row.
///
/// Generic over the sqlx executor so `claim_task` can create the run inside its
/// transaction; passing a plain `&pool` works too.
pub async fn insert_task_run<'e, E>(executor: E, run: &TaskRun) -> anyhow::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let (cost_usd, cost_amount, cost_currency) = usage_cost_columns(run.usage.as_ref());
    let tokens = usage_token_columns(run.usage.as_ref());
    let (context_used, context_size) = usage_context_columns(run.usage.as_ref());
    sqlx::query(
        "INSERT INTO task_runs
            (id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure,
             input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd,
             cost_amount, cost_currency, context_used, context_size)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(tokens[0])
    .bind(tokens[1])
    .bind(tokens[2])
    .bind(tokens[3])
    .bind(tokens[4])
    .bind(cost_usd)
    .bind(cost_amount)
    .bind(cost_currency)
    .bind(context_used)
    .bind(context_size)
    .execute(executor)
    .await?;
    Ok(())
}

/// Fetch a single run by id.
#[allow(dead_code)] // consumed by the executor (#147); unit-tested here.
pub async fn get_task_run(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Option<TaskRun>> {
    let row = sqlx::query(
        "SELECT id, task_id, attempt, status, agent, model, session_id, started_at, finished_at, exit_code, error, failure, \
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd, \
                cost_amount, cost_currency, context_used, context_size \
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
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd, \
                cost_amount, cost_currency, context_used, context_size \
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
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd, \
                cost_amount, cost_currency, context_used, context_size \
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
                input_tokens, output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens, cost_usd, \
                cost_amount, cost_currency, context_used, context_size \
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
    let (cost_usd, cost_amount, cost_currency) = usage_cost_columns(run.usage.as_ref());
    let tokens = usage_token_columns(run.usage.as_ref());
    let (context_used, context_size) = usage_context_columns(run.usage.as_ref());
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
             cost_usd = ?,
             cost_amount = ?,
             cost_currency = ?,
             context_used = ?,
             context_size = ?
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
    .bind(tokens[0])
    .bind(tokens[1])
    .bind(tokens[2])
    .bind(tokens[3])
    .bind(tokens[4])
    .bind(cost_usd)
    .bind(cost_amount)
    .bind(cost_currency)
    .bind(context_used)
    .bind(context_size)
    .bind(run.id.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

// ---------------------------------------------------------------------------
// Durable agent sessions
// ---------------------------------------------------------------------------

/// Lifecycle of a durable ACP session record.
///
/// `Loading` and `Ready` describe a live or loadable session; `Ended` means the
/// agent session closed normally, and `Failed` means it could not be started,
/// loaded, or completed. An unrecognised stored value decodes as `Failed` so a
/// newer schema can never make an unknown record look resumable.
#[allow(dead_code)] // wired into session launch/history by the following steps
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionStatus {
    /// The session exists but has not reported readiness yet (including an ACP
    /// `session/load` in progress).
    Loading,
    /// The session is live and accepting prompts, or is durably loadable.
    Ready,
    /// The session closed normally; its metadata is kept for history.
    Ended,
    /// Launch, load, or the agent itself failed.
    Failed,
}

#[allow(dead_code)] // wired into session launch/history by the following steps
impl AgentSessionStatus {
    /// Stable string form stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentSessionStatus::Loading => "loading",
            AgentSessionStatus::Ready => "ready",
            AgentSessionStatus::Ended => "ended",
            AgentSessionStatus::Failed => "failed",
        }
    }

    /// Decode a stored status. Anything unknown is treated as `Failed`.
    fn from_db(raw: &str) -> Self {
        match raw {
            "loading" => Self::Loading,
            "ready" => Self::Ready,
            "ended" => Self::Ended,
            "failed" => Self::Failed,
            other => {
                tracing::warn!(
                    status = %other,
                    "unrecognised agent session status; falling back to Failed"
                );
                Self::Failed
            }
        }
    }
}

/// Durable metadata for one ACP session created by Favetto.
///
/// `id` is Favetto's local routing key and is never the agent's ACP session id.
/// `external_session_id` belongs to `agent` and is only meaningful within that
/// configuration, since different ACP servers may reuse ids. There is
/// deliberately no transcript, environment, or credential column: conversation
/// history stays with the ACP agent.
#[allow(dead_code)] // wired into session launch/history by the following steps
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRecord {
    /// Stable Favetto-local session id.
    pub id: String,
    /// Configured `[agents.<name>]` key that owns the session.
    pub agent: String,
    /// The agent's own ACP session id, once it reports one.
    pub external_session_id: Option<String>,
    /// Working directory the agent was launched in.
    pub cwd: PathBuf,
    /// Catalog task this session belongs to, if task-backed.
    pub task_id: Option<String>,
    /// Execution attempt this session belongs to, if task-backed.
    pub run_id: Option<String>,
    /// Display title, when the agent reports one.
    pub title: Option<String>,
    /// Current lifecycle state.
    pub status: AgentSessionStatus,
    /// Last error, for a `Failed` record.
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[allow(dead_code)] // wired into session launch/history by the following steps
impl AgentSessionRecord {
    /// Build a record that has not reported readiness yet.
    pub fn loading(
        id: impl Into<String>,
        agent: impl Into<String>,
        cwd: impl Into<PathBuf>,
        now: Timestamp,
    ) -> Self {
        Self {
            id: id.into(),
            agent: agent.into(),
            external_session_id: None,
            cwd: cwd.into(),
            task_id: None,
            run_id: None,
            title: None,
            status: AgentSessionStatus::Loading,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Insert a new durable session record. Fails if the local id already exists.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn insert_agent_session(
    pool: &SqlitePool,
    session: &AgentSessionRecord,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO agent_sessions
            (id, agent, external_session_id, cwd, task_id, run_id, title, status, error,
             created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&session.id)
    .bind(&session.agent)
    .bind(&session.external_session_id)
    .bind(session.cwd.to_string_lossy().into_owned())
    .bind(&session.task_id)
    .bind(&session.run_id)
    .bind(&session.title)
    .bind(session.status.as_str())
    .bind(&session.error)
    .bind(ts_ms(session.created_at))
    .bind(ts_ms(session.updated_at))
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace a session's mutable metadata. `created_at` is preserved.
///
/// Returns whether a row matched. This is the general update used when several
/// fields change together; the targeted setters below are convenient for the
/// common lifecycle transitions.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn update_agent_session(
    pool: &SqlitePool,
    session: &AgentSessionRecord,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE agent_sessions SET
             agent = ?,
             external_session_id = ?,
             cwd = ?,
             task_id = ?,
             run_id = ?,
             title = ?,
             status = ?,
             error = ?,
             updated_at = ?
         WHERE id = ?",
    )
    .bind(&session.agent)
    .bind(&session.external_session_id)
    .bind(session.cwd.to_string_lossy().into_owned())
    .bind(&session.task_id)
    .bind(&session.run_id)
    .bind(&session.title)
    .bind(session.status.as_str())
    .bind(&session.error)
    .bind(ts_ms(session.updated_at))
    .bind(&session.id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Update only the lifecycle status (and error) of a session.
#[allow(dead_code)] // used by the durable session store and its tests
pub async fn set_agent_session_status(
    pool: &SqlitePool,
    id: &str,
    status: AgentSessionStatus,
    error: Option<&str>,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result =
        sqlx::query("UPDATE agent_sessions SET status = ?, error = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(error)
            .bind(ts_ms(updated_at))
            .bind(id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() == 1)
}

/// Record the agent's ACP session id and mark the session ready in one write.
///
/// The two values must advance together: a row that is `ready` without an
/// external id cannot be resumed, and must never be advertised as resumable.
/// Returns whether a row matched.
pub async fn mark_agent_session_ready(
    pool: &SqlitePool,
    id: &str,
    external_session_id: &str,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE agent_sessions
         SET external_session_id = ?, status = 'ready', error = NULL, updated_at = ?
         WHERE id = ?",
    )
    .bind(external_session_id)
    .bind(ts_ms(updated_at))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Mark a live session closed, keeping its metadata for history.
///
/// Only a session that was still live (`loading`/`ready`) transitions, so a
/// recorded failure or a repeated close is left untouched.
pub async fn close_agent_session(
    pool: &SqlitePool,
    id: &str,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE agent_sessions SET status = 'ended', updated_at = ?
         WHERE id = ? AND status IN ('loading', 'ready')",
    )
    .bind(ts_ms(updated_at))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Mark a session failed with the reason, keeping its metadata for history.
///
/// Only a session that was still live (`loading`/`ready`) transitions, so an
/// already-ended session is never rewritten as failed.
pub async fn fail_agent_session(
    pool: &SqlitePool,
    id: &str,
    error: &str,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE agent_sessions SET status = 'failed', error = ?, updated_at = ?
         WHERE id = ? AND status IN ('loading', 'ready')",
    )
    .bind(error)
    .bind(ts_ms(updated_at))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Persist the agent's ACP session id on a run as soon as it is known.
///
/// The run is finalized later with `COALESCE`, so this early write is the
/// durable record a crash between launch and finalization leaves behind.
pub async fn set_task_run_session_id(
    pool: &SqlitePool,
    run_id: &str,
    session_id: &str,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE task_runs SET session_id = ? WHERE id = ?")
        .bind(session_id)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// Record the agent's own ACP session id once it is known. The id is scoped to
/// the record's configured agent by the schema's composite unique index.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn set_agent_session_external_id(
    pool: &SqlitePool,
    id: &str,
    external_session_id: &str,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE agent_sessions SET external_session_id = ?, updated_at = ? WHERE id = ?",
    )
    .bind(external_session_id)
    .bind(ts_ms(updated_at))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Update the display title reported by the agent.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn set_agent_session_title(
    pool: &SqlitePool,
    id: &str,
    title: Option<&str>,
    updated_at: Timestamp,
) -> anyhow::Result<bool> {
    let result = sqlx::query("UPDATE agent_sessions SET title = ?, updated_at = ? WHERE id = ?")
        .bind(title)
        .bind(ts_ms(updated_at))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// Fetch one durable session by its Favetto-local id.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn get_agent_session(
    pool: &SqlitePool,
    id: &str,
) -> anyhow::Result<Option<AgentSessionRecord>> {
    let row = sqlx::query(
        "SELECT id, agent, external_session_id, cwd, task_id, run_id, title, status, error,
                created_at, updated_at
         FROM agent_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_agent_session))
}

/// Find a session by the agent-scoped external ACP id.
///
/// ACP servers are independent, so the same external id may exist under two
/// different configured agents; `agent` selects the namespace.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn get_agent_session_by_external(
    pool: &SqlitePool,
    agent: &str,
    external_session_id: &str,
) -> anyhow::Result<Option<AgentSessionRecord>> {
    let row = sqlx::query(
        "SELECT id, agent, external_session_id, cwd, task_id, run_id, title, status, error,
                created_at, updated_at
         FROM agent_sessions WHERE agent = ? AND external_session_id = ?",
    )
    .bind(agent)
    .bind(external_session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_agent_session))
}

/// List durable sessions, most recently updated first.
#[allow(dead_code)] // wired into session launch/history by the following steps
pub async fn list_agent_sessions(
    pool: &SqlitePool,
    limit: i64,
) -> anyhow::Result<Vec<AgentSessionRecord>> {
    let rows = sqlx::query(
        "SELECT id, agent, external_session_id, cwd, task_id, run_id, title, status, error,
                created_at, updated_at
         FROM agent_sessions
         ORDER BY updated_at DESC, created_at DESC, id ASC
         LIMIT ?",
    )
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_agent_session).collect())
}

/// List durable sessions belonging to one task, most recently updated first.
///
/// The query is backed by `idx_agent_sessions_task_id` and is intentionally
/// scoped: callers that restore a run's conversation must never enumerate the
/// global session history.
pub async fn list_agent_sessions_for_task(
    pool: &SqlitePool,
    task_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<AgentSessionRecord>> {
    let rows = sqlx::query(
        "SELECT id, agent, external_session_id, cwd, task_id, run_id, title, status, error,
                created_at, updated_at
         FROM agent_sessions
         WHERE task_id = ?
         ORDER BY updated_at DESC, created_at DESC, id ASC
         LIMIT ?",
    )
    .bind(task_id)
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_agent_session).collect())
}

#[allow(dead_code)] // wired into session launch/history by the following steps
fn row_to_agent_session(row: &SqliteRow) -> AgentSessionRecord {
    AgentSessionRecord {
        id: row.get("id"),
        agent: row.get("agent"),
        external_session_id: row
            .try_get::<Option<String>, _>("external_session_id")
            .ok()
            .flatten(),
        cwd: PathBuf::from(row.get::<String, _>("cwd")),
        task_id: row.try_get::<Option<String>, _>("task_id").ok().flatten(),
        run_id: row.try_get::<Option<String>, _>("run_id").ok().flatten(),
        title: row.try_get::<Option<String>, _>("title").ok().flatten(),
        status: AgentSessionStatus::from_db(&row.get::<String, _>("status")),
        error: row.try_get::<Option<String>, _>("error").ok().flatten(),
        created_at: from_ms(row.get("created_at")),
        updated_at: from_ms(row.get("updated_at")),
    }
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
    now: Timestamp,
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
        let cost_usd = row.try_get::<Option<f64>, _>("cost_usd").ok().flatten();
        let usage = AgentUsage {
            input_tokens: row.get::<i64, _>("input_tokens").max(0) as u64,
            output_tokens: row.get::<i64, _>("output_tokens").max(0) as u64,
            reasoning_tokens: row.get::<i64, _>("reasoning_tokens").max(0) as u64,
            cache_read_tokens: row.get::<i64, _>("cache_read_tokens").max(0) as u64,
            cache_write_tokens: row.get::<i64, _>("cache_write_tokens").max(0) as u64,
            cost_usd,
            cost: cost_usd.map(AgentCost::usd),
            context_used: None,
            context_size: None,
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
fn usage_bucket_bounds(period: UsagePeriod, now: Timestamp) -> Vec<(Timestamp, Timestamp)> {
    match period {
        UsagePeriod::Day => {
            let end = floor_to(now, HOUR_MS) + TimeDuration::hours(1);
            (0..24)
                .map(|i| {
                    let start = end - TimeDuration::hours(24 - i);
                    (start, start + TimeDuration::hours(1))
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
fn usage_bucket_key(period: UsagePeriod, start: Timestamp) -> String {
    let start = start.to_offset_datetime();
    match period {
        UsagePeriod::Day => start
            .format(time::macros::format_description!(
                "[year]-[month]-[day]T[hour]"
            ))
            .unwrap_or_default(),
        UsagePeriod::Week | UsagePeriod::Month => start
            .format(time::macros::format_description!("[year]-[month]-[day]"))
            .unwrap_or_default(),
        UsagePeriod::Year => start
            .format(time::macros::format_description!("[year]-[month]"))
            .unwrap_or_default(),
    }
}

/// Truncate `now` down to a multiple of `step_ms` since the Unix epoch.
fn floor_to(now: Timestamp, step_ms: i64) -> Timestamp {
    let ms = now.unix_timestamp_millis().div_euclid(step_ms) * step_ms;
    Timestamp::from_unix_timestamp_millis(ms).unwrap_or(now)
}

fn day_buckets(now: Timestamp, days: i64) -> Vec<(Timestamp, Timestamp)> {
    let end = floor_to(now, DAY_MS) + TimeDuration::days(1);
    (0..days)
        .map(|i| {
            let start = end - TimeDuration::days(days - i);
            (start, start + TimeDuration::days(1))
        })
        .collect()
}

/// The first instant of `now`'s UTC calendar month.
fn month_floor(now: Timestamp) -> Timestamp {
    let dt = now.to_offset_datetime();
    let date = Date::from_calendar_date(dt.year(), dt.month(), 1).unwrap_or_else(|_| dt.date());
    PrimitiveDateTime::new(date, Time::MIDNIGHT)
        .assume_utc()
        .into()
}

/// `base` shifted by `delta` whole months (clamped to the first of the month).
fn add_months(base: Timestamp, delta: i32) -> Timestamp {
    let dt = base.to_offset_datetime();
    let total = dt.year() * 12 + i32::from(u8::from(dt.month())) - 1 + delta;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    let month = Month::try_from(month as u8).unwrap_or_else(|_| dt.month());
    let date = Date::from_calendar_date(year, month, 1).unwrap_or_else(|_| dt.date());
    PrimitiveDateTime::new(date, Time::MIDNIGHT)
        .assume_utc()
        .into()
}

fn month_buckets(now: Timestamp, months: i32) -> Vec<(Timestamp, Timestamp)> {
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
    let cutoff = ts_ms(Timestamp::now() - TimeDuration::days(days as i64));
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
    pub created_at: Timestamp,
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

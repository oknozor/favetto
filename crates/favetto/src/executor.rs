//! Task executor: drains the pending task queue and runs each task through its
//! configured external agent, plus reacts to `TaskFinished` events to start tasks
//! that depend on them (`needs = "other:finished"`). A root-scoped fan-in
//! (`needs = "other:all_finished"`) starts once after every `other` run in the
//! same workflow root reaches a terminal state.
//!
//! Concurrency and isolation are configurable (`[executor]`):
//!
//! - `parallel = false` (default): one task at a time.
//! - `parallel = true`: up to `max_concurrency` tasks run at once. When a task
//!   runs inside a git repository it gets its own `git worktree` (`worktree =
//!   true`), so tasks don't step on each other. Tasks that do **not** get a
//!   worktree are serialized per working directory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use sqlx::SqlitePool;
use tokio::process::Command;
use tokio::sync::{OwnedMutexGuard, Semaphore};
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

use crate::agents::{resolve_session_title, Agent, TITLE_POLL_ATTEMPTS, TITLE_POLL_INTERVAL};
use crate::config::ExecutorSettings;
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::tasks::{needs_parts, NeedsKind, TaskDef};

/// The result of an agent run. `error` is `Some` for a non-zero exit or a
/// missing session; `session_id`/`session_title` are carried either way so a
/// failed run keeps its reattach handle and its title.
struct RunOutcome {
    output: serde_json::Value,
    session_id: Option<String>,
    session_title: Option<String>,
    error: Option<String>,
}

/// Fold a finished (or failed-to-start) agent run onto the task row. Returns
/// whether the run succeeded. Session info is assigned on both arms.
fn record_run_outcome(task: &mut Task, outcome: anyhow::Result<RunOutcome>) -> bool {
    match outcome {
        Ok(run) => {
            task.session_id = run.session_id;
            task.session_title = run.session_title;
            if let Some(err) = run.error {
                task.status = TaskStatus::Failed;
                task.error = Some(err);
                false
            } else {
                task.status = TaskStatus::Succeeded;
                task.output = Some(run.output);
                task.error = None;
                true
            }
        }
        Err(e) => {
            task.status = TaskStatus::Failed;
            task.error = Some(e.to_string());
            false
        }
    }
}

/// Spawn the dispatcher and the dependency listener.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
    let cfg = state.config.read().unwrap().executor.clone();

    let dep_state = state.clone();
    tokio::spawn(async move {
        let mut rx = dep_state.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(ServerPush::Event(ev)) => {
                    if ev.kind == EventKind::TaskFinished {
                        start_dependents(&dep_state, &ev).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "executor dependency listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let dispatcher = tokio::spawn(async move {
        dispatch_loop(state, cfg).await;
    });
    dispatcher
}

/// Repeatedly claim pending tasks and run them, honoring the concurrency limit and
/// per-directory serialization.
async fn dispatch_loop(state: Arc<State>, cfg: ExecutorSettings) {
    let semaphore = Arc::new(Semaphore::new(cfg.concurrency()));
    let dirs = DirLocks::default();
    let mut interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;
        let limit = (cfg.concurrency() as i64) * 4;
        let pending = match db::next_pending_tasks(&state.db, limit).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "failed to poll task queue");
                continue;
            }
        };

        for task in pending {
            // Only dispatch if a slot is free (don't block the loop).
            let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                break;
            };

            let Some(def) = lookup_def(&state, &task.name) else {
                fail_task(&state, task, "task not found in the catalog").await;
                continue;
            };

            // Required input variables must be present even for unattended starts
            // (hooks / needs / spawn / schedule / raw RPC), which never prompt.
            if let Some(missing) = missing_required_vars(&def, &task.input).first() {
                fail_task(
                    &state,
                    task,
                    &format!("task '{}' requires input variable '{missing}'", def.name),
                )
                .await;
                continue;
            }

            let base = resolve_base_dir(&def, &task);
            let plan = match make_plan(&state, &cfg, &base, &task).await {
                Ok(p) => p,
                Err(e) => {
                    fail_task(&state, task, &format!("worktree setup failed: {e}")).await;
                    continue;
                }
            };

            // Reserve the directory so same-dir tasks serialize.
            let guard = if plan.needs_lock {
                match dirs.try_lock(&base) {
                    Some(g) => Some(g),
                    None => continue, // another task owns this directory
                }
            } else {
                None
            };

            if !db::claim_task(&state.db, task.id).await.unwrap_or(false) {
                if let Some(wt) = &plan.worktree {
                    remove_worktree(&state.db, task.id, &wt.repo, &wt.path, &wt.branch).await;
                }
                continue; // already claimed
            }

            tracing::info!(
                task_id = %task.id,
                task = %task.name,
                cwd = %plan.cwd.display(),
                worktree = plan.worktree.is_some(),
                "executing task"
            );

            let state = state.clone();
            let def = def.clone();
            tokio::spawn(async move {
                run_one(&state, task, def, plan, guard).await;
                drop(permit);
            });
        }
    }
}

/// Workflow lineage for a task enqueued by another task. Empty for a task
/// started directly (manual, scheduled, RPC, hook, webhook).
#[derive(Debug, Clone, Copy, Default)]
struct Lineage {
    parent_id: Option<Uuid>,
    root_id: Option<Uuid>,
}

impl Lineage {
    /// The lineage of a task spawned/depended-on by `parent` (which may be the
    /// root itself); its workflow root is inherited, falling back to the parent.
    fn child_of(parent: &Task) -> Self {
        Self {
            parent_id: Some(parent.id),
            root_id: Some(parent.root_or_self()),
        }
    }
}

/// Enqueue a task (idle), announcing it on the bus and emitting `TaskIdle`. The
/// task is its own workflow root (no lineage).
pub async fn enqueue_task(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
) -> anyhow::Result<Task> {
    enqueue_with_lineage(state, name, input, dedupe_key, Lineage::default()).await
}

/// Enqueue a task with workflow lineage, announcing it on the bus and emitting
/// `TaskIdle`.
async fn enqueue_with_lineage(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
    lineage: Lineage,
) -> anyhow::Result<Task> {
    let task = Task {
        id: Uuid::new_v4(),
        name: name.clone(),
        status: TaskStatus::Pending,
        input,
        output: None,
        dedupe_key,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
        parent_id: lineage.parent_id,
        root_id: lineage.root_id,
    };
    db::insert_task(&state.db, &task).await?;
    crate::metrics::inc_tasks();
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            EventKind::TaskIdle,
            serde_json::json!({ "name": name, "task_id": task.id }),
        )
        .await;
    Ok(task)
}

async fn run_one(
    state: &Arc<State>,
    mut task: Task,
    def: TaskDef,
    plan: Plan,
    _dir_guard: Option<OwnedMutexGuard<()>>,
) {
    // `claim_task` already persisted Running; mirror it on our copy.
    task.status = TaskStatus::Running;
    task.started_at = Some(Utc::now());
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            EventKind::TaskStarted,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;

    let config = state.config.read().unwrap().clone();

    let prompt = render_task_prompt(&def, &task);

    let agent_name = def.agent.clone().or_else(|| config.agent.default.clone());
    let outcome = match agent_name.as_deref() {
        Some(name) => run_agent_task(state, &task, name, &def, &prompt, &plan.cwd).await,
        None => Err(anyhow::anyhow!(
            "task '{}' has no agent: set `agent` in the task or `[agent].default` in the config",
            def.name
        )),
    };

    let success = record_run_outcome(&mut task, outcome);
    task.finished_at = Some(Utc::now());

    let _ = db::upsert_task(&state.db, &task).await;
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));

    // If the CLI had not written the title yet, keep polling in the background and
    // push the update when it appears, so an open TUI fills the cell live.
    if task.session_id.is_some() && task.session_title.is_none() {
        let worktree_removed = plan.worktree.is_some() && !config.executor.keep_worktree;
        let backfill_cwd = if worktree_removed {
            plan.worktree
                .as_ref()
                .map(|w| w.repo.clone())
                .unwrap_or_else(|| plan.cwd.clone())
        } else {
            plan.cwd.clone()
        };
        if let Some(agent) = agent_name.as_deref().and_then(|n| state.registry.get(n)) {
            if agent.has_session_titles() {
                if let Some(sid) = task.session_id.clone() {
                    spawn_title_backfill(state.clone(), task.id, agent, sid, backfill_cwd);
                }
            }
        }
    }

    let kind = if success {
        EventKind::TaskCompleted
    } else {
        EventKind::TaskFailed
    };
    state
        .emit_event(kind, serde_json::json!({ "task_id": task.id }))
        .await;
    state
        .emit_event(
            EventKind::TaskFinished,
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": success }),
        )
        .await;

    if success {
        if let Some(spawn_task) = def.spawn.clone() {
            if let Err(e) = spawn_from_manifest(state, &task, &def, &plan.cwd).await {
                tracing::warn!(task = %task.name, error = %e, "failed to spawn tasks from handoff");
            }
            // The parent's own `TaskFinished` was emitted above, before the
            // manifest was read, so a fan-in on the spawned child is evaluated
            // here instead. This is also what lets an empty manifest (`[]`)
            // resolve the barrier.
            evaluate_join_barriers(state, &spawn_task, task.root_or_self(), Some(task.id)).await;
        }
    }

    // Reclaim the worktree only after the handoff above has been read. For an
    // isolated run the `spawn_file` lives inside the worktree, so removing it
    // earlier made the manifest unreadable and silently dropped `spawn` tasks.
    if let Some(wt) = &plan.worktree {
        if config.executor.keep_worktree {
            tracing::info!(path = %wt.path.display(), "worktree kept");
        } else {
            remove_worktree(&state.db, task.id, &wt.repo, &wt.path, &wt.branch).await;
        }
    }
}

/// Render a task definition's prompt against the task's `input`/`task`/`prev`
/// context. Shared by the executor (before an agent run) and the server (when
/// seeding a fresh interactive session for a catalog task) so both surfaces
/// expand `{{ input.* }}` identically.
pub(crate) fn render_task_prompt(def: &TaskDef, task: &Task) -> String {
    crate::template::render(&def.prompt, &render_context(task))
}

/// Build the template context for a task's prompt and handoff path. `prev` is
/// populated from `input._prev`, which the `needs` dependency listener attaches.
fn render_context(task: &Task) -> serde_json::Value {
    let mut ctx = serde_json::json!({
        "task": {
            "id": task.id.to_string(),
            "name": task.name,
        },
        "input": task.input,
    });
    if let Some(prev) = task.input.get("_prev") {
        ctx["prev"] = prev.clone();
    }
    ctx
}

/// Largest char boundary `<= idx` (clamped to the string length).
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= idx` (clamped to the string length).
fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Cap `s` to `max` bytes on UTF-8 boundaries, keeping the head and the tail.
/// Returns the (possibly truncated) string and whether truncation happened.
fn truncate_text(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max || max == 0 {
        return (s.to_string(), false);
    }
    let head_budget = max * 3 / 4;
    let tail_budget = max - head_budget;
    let head_end = floor_char_boundary(s, head_budget);
    let tail_start = ceil_char_boundary(s, s.len() - tail_budget);
    let removed = s.len() - head_end - (s.len() - tail_start);
    (
        format!(
            "{}\n… [truncated {removed} bytes] …\n{}",
            &s[..head_end],
            &s[tail_start..]
        ),
        true,
    )
}

/// Build the persisted `task.output` value from a finished run. Caps the raw
/// text, records the original size, and drops the default parser's
/// `{"text": raw}` duplicate.
fn build_task_output(
    raw: &str,
    parsed: &serde_json::Value,
    agent: &str,
    session_id: Option<String>,
    session_title: Option<String>,
    max: usize,
) -> serde_json::Value {
    let (capped_raw, truncated) = truncate_text(raw, max);
    // The default parser wraps the raw text as `{"text": raw}`; that is a
    // byte-for-byte duplicate of `output`, so store `null` instead.
    let result = if parsed
        .as_object()
        .map(|o| o.len() == 1 && o.get("text").and_then(|v| v.as_str()) == Some(raw))
        .unwrap_or(false)
    {
        serde_json::Value::Null
    } else {
        let serialized = parsed.to_string();
        if serialized.len() > max {
            let (preview, _) = truncate_text(&serialized, max);
            serde_json::json!({ "preview": preview, "truncated": true })
        } else {
            parsed.clone()
        }
    };
    serde_json::json!({
        "agent": agent,
        "session_id": session_id,
        "session_title": session_title,
        "output_bytes": raw.len(),
        "truncated": truncated,
        "output": capped_raw,
        "result": result,
    })
}

/// Read the task's `spawn_file` (rendered template), parse it as a JSON array,
/// and enqueue one `spawn` task per element (the element becomes its `input`). An
/// empty array is a no-op. A non-array JSON value is treated as a single element.
async fn spawn_from_manifest(
    state: &Arc<State>,
    task: &Task,
    def: &TaskDef,
    cwd: &Path,
) -> anyhow::Result<()> {
    let spawn_task = def
        .spawn
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task '{}' has no `spawn` task", def.name))?;
    let spawn_file = def
        .spawn_file
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task '{}' sets `spawn` but no `spawn_file`", def.name))?;

    let rendered = crate::template::render(spawn_file, &render_context(task));
    let path = if Path::new(&rendered).is_absolute() {
        PathBuf::from(&rendered)
    } else {
        cwd.join(&rendered)
    };

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read spawn_file {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("spawn_file {} is not valid JSON: {e}", path.display()))?;

    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    };
    if items.is_empty() {
        tracing::info!(task = %task.name, "spawn manifest is empty; nothing to enqueue");
        return Ok(());
    }

    let lineage = Lineage::child_of(task);
    for (i, item) in items.into_iter().enumerate() {
        let dedupe = format!("spawn:{}:{i}", task.id);
        match enqueue_with_lineage(state, spawn_task.to_string(), item, Some(dedupe), lineage).await
        {
            Ok(enqueued) => {
                tracing::info!(task = %task.name, spawned = %enqueued.id, "spawned task");
            }
            Err(e) => tracing::warn!(error = %e, "failed to enqueue spawned task"),
        }
    }
    Ok(())
}

/// Run a task through its configured external agent as a real PTY session bound
/// to the task, so it can be reattached from the TUI and its output captured.
/// Returns the produced output plus the agent's own session id and its session
/// title (both optional), even when the run failed.
async fn run_agent_task(
    state: &Arc<State>,
    task: &Task,
    agent_name: &str,
    def: &TaskDef,
    prompt: &str,
    cwd: &Path,
) -> anyhow::Result<RunOutcome> {
    let agent = state.registry.get_checked(agent_name)?;

    let ctx = crate::agents::AgentContext {
        cwd: Some(cwd.to_path_buf()),
        provider: def.provider.clone(),
        model: def.model.clone(),
        prompt: Some(prompt.to_string()),
        rows: 40,
        cols: 120,
        git_signing: def.sign,
        ..Default::default()
    };
    let info = state.agents.start(
        agent_name,
        agent.clone(),
        Some(task.id.to_string()),
        crate::agents::Invocation::Headless {
            prompt,
            provider: def.provider.as_deref(),
            model: def.model.as_deref(),
        },
        ctx,
    )?;

    // Resolve the title as soon as the CLI reports its session id instead of
    // waiting for the run to end, so an open TUI fills the `SESSION` cell live.
    let title_watch = if agent.has_session_titles() {
        let st = state.clone();
        let ag = agent.clone();
        let live = info.id.clone();
        let tid = task.id;
        let watch_cwd = cwd.to_path_buf();
        Some(tokio::spawn(async move {
            watch_title_while_running(&st, tid, ag, &live, &watch_cwd).await
        }))
    } else {
        None
    };

    let (detect, quiet) = {
        let cfg = state.config.read().unwrap();
        (
            cfg.executor.detect_awaiting_input,
            Duration::from_millis(cfg.executor.awaiting_input_quiet_ms),
        )
    };
    let code = if detect {
        crate::attention::watch(state, &info.id, agent.clone(), Some(task.id), quiet).await
    } else {
        state.agents.wait(&info.id).await
    };
    let raw = state.agents.output(&info.id);
    let mut result = agent.parse_output(&raw, code);
    // The manager also captures the id live from the PTY; prefer it if the
    // implementation's parser did not find one.
    if result.session_id.is_none() {
        result.session_id = state.agents.external_session_id(&info.id);
    }
    // Prefer the title resolved (and already published) while the run was still
    // in progress. Only fall back to a fresh bounded lookup when the watcher
    // never saw a session id, so the retry budget is never spent twice in a row.
    let session_title = match title_watch {
        Some(handle) => match handle.await {
            Ok(TitleWatch::Observed(title)) => title,
            Ok(TitleWatch::Skipped) | Err(_) => {
                resolve_title_fallback(agent.clone(), result.session_id.clone(), cwd).await
            }
        },
        None => resolve_title_fallback(agent.clone(), result.session_id.clone(), cwd).await,
    };
    // The run's PTY only carried machine output (e.g. JSON events); drop it once
    // its session id is captured, since reattaching launches a fresh interactive
    // TUI on that session. Agents without resume keep the PTY so its final screen
    // can still be replayed.
    if result.session_id.is_some() && agent.capabilities().resume {
        let _ = state.agents.close(&info.id);
    }
    let max = state.config.read().unwrap().executor.max_output_bytes;
    let output = build_task_output(
        &result.raw,
        &result.output,
        agent_name,
        result.session_id.clone(),
        session_title.clone(),
        max,
    );
    let error = match result.exit_code {
        Some(0) => None,
        Some(c) => Some(format!("agent '{agent_name}' exited with {c}:\n{raw}")),
        None => Some(format!("agent '{agent_name}' session disappeared")),
    };
    Ok(RunOutcome {
        output,
        session_id: result.session_id,
        session_title,
        error,
    })
}

/// Background title poll window (after the synchronous poll gave up): ~60 s.
const BACKFILL_ATTEMPTS: u32 = 30;
const BACKFILL_INTERVAL: Duration = Duration::from_secs(2);

/// How often to check whether a running agent has reported its session id yet.
const SESSION_ID_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The completion-time safety net: resolve the title from the parsed session id
/// with a bounded retry before the worktree is torn down.
async fn resolve_title_fallback(
    agent: Arc<dyn Agent>,
    session_id: Option<String>,
    cwd: &Path,
) -> Option<String> {
    let sid = session_id?;
    resolve_session_title(agent, &sid, cwd, TITLE_POLL_ATTEMPTS, TITLE_POLL_INTERVAL).await
}

/// What a mid-run title watch produced.
pub(crate) enum TitleWatch {
    /// The live session id was seen; the title budget was spent (`Some` when a
    /// title was found/stored, `None` when it never appeared).
    Observed(Option<String>),
    /// The session exited before any external session id was captured.
    Skipped,
}

/// While a run is in progress, wait for its live external session id, then
/// resolve and persist the title (publishing `TaskUpdated`) immediately instead
/// of waiting for the run to end.
pub(crate) async fn watch_title_while_running(
    state: &Arc<State>,
    task_id: Uuid,
    agent: Arc<dyn Agent>,
    live_session: &str,
    cwd: &Path,
) -> TitleWatch {
    loop {
        if let Some(sid) = state.agents.external_session_id(live_session) {
            let title = backfill_title(
                state,
                task_id,
                agent,
                sid,
                cwd.to_path_buf(),
                TITLE_POLL_ATTEMPTS,
                TITLE_POLL_INTERVAL,
            )
            .await;
            return TitleWatch::Observed(title);
        }
        if !state.agents.is_running(live_session) {
            return TitleWatch::Skipped;
        }
        tokio::time::sleep(SESSION_ID_POLL_INTERVAL).await;
    }
}

/// Poll for a missing session title in the background and persist the first one
/// that appears, publishing a `TaskUpdated` so an open TUI fills the cell live.
fn spawn_title_backfill(
    state: Arc<State>,
    task_id: Uuid,
    agent: Arc<dyn Agent>,
    session_id: String,
    cwd: PathBuf,
) {
    tokio::spawn(async move {
        backfill_title(
            &state,
            task_id,
            agent,
            session_id,
            cwd,
            BACKFILL_ATTEMPTS,
            BACKFILL_INTERVAL,
        )
        .await;
    });
}

/// The body of [`spawn_title_backfill`], split out so it can be awaited in tests.
/// Returns the stored title, if any. Persists the session id when the row that
/// the live run is attached to has not recorded it yet.
async fn backfill_title(
    state: &Arc<State>,
    task_id: Uuid,
    agent: Arc<dyn Agent>,
    session_id: String,
    cwd: PathBuf,
    attempts: u32,
    interval: Duration,
) -> Option<String> {
    let Ok(Some(mut task)) = db::get_task(&state.db, task_id).await else {
        return None;
    };
    // Already complete: don't do a redundant write/publish.
    if task.session_id.as_deref() == Some(session_id.as_str()) && task.session_title.is_some() {
        return task.session_title;
    }
    if task.session_id.is_none() {
        task.session_id = Some(session_id.clone());
    }
    if task.session_title.is_none() {
        if let Some(title) =
            resolve_session_title(agent, &session_id, &cwd, attempts, interval).await
        {
            task.session_title = Some(title);
        }
    }
    let stored = task.session_title.clone();
    if db::upsert_task(&state.db, &task).await.is_ok() {
        state
            .bus
            .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    }
    stored
}

/// Mark a task failed (used when it can't even be started).
async fn fail_task(state: &State, task: Task, error: &str) {
    let mut task = task;
    task.status = TaskStatus::Failed;
    task.error = Some(error.to_string());
    task.finished_at = Some(Utc::now());
    let _ = db::upsert_task(&state.db, &task).await;
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            EventKind::TaskFinished,
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": false }),
        )
        .await;
}

fn lookup_def(state: &State, name: &str) -> Option<TaskDef> {
    state
        .catalog
        .read()
        .unwrap()
        .iter()
        .find(|d| d.name == name)
        .cloned()
}

/// Names of the task's `required` input variables that are absent, JSON `null`,
/// or an empty string in `input`. Optional omitted/empty values are left alone.
pub fn missing_required_vars(def: &TaskDef, input: &serde_json::Value) -> Vec<String> {
    def.vars
        .iter()
        .filter(|v| v.required)
        .filter(|v| match input.get(&v.name) {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.is_empty(),
            _ => false,
        })
        .map(|v| v.name.clone())
        .collect()
}

/// The directory a task starts from: per-run `input.cwd`, else the task's `cwd`,
/// else the daemon's working directory.
fn resolve_base_dir(def: &TaskDef, task: &Task) -> PathBuf {
    task.input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| def.cwd.as_ref().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A concrete execution directory plan.
struct Plan {
    cwd: PathBuf,
    /// Serialize same-directory tasks (no worktree isolation).
    needs_lock: bool,
    worktree: Option<Worktree>,
}

/// What a [`prune_worktrees`] pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorktreePruneStats {
    pub removed: usize,
    pub orphans: usize,
    pub repos_pruned: usize,
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
}

/// Choose the task's working directory: a fresh git worktree when parallel +
/// `worktree` and the base dir is inside a repository, otherwise the base dir.
async fn make_plan(
    state: &State,
    cfg: &ExecutorSettings,
    base: &Path,
    task: &Task,
) -> anyhow::Result<Plan> {
    if cfg.parallel && cfg.worktree {
        if let Some(repo) = git_toplevel(base).await {
            let path = worktree_path(state, cfg, &repo, task);
            let branch = branch_name(task);
            create_worktree(&repo, &path, &branch).await?;
            let record = db::WorktreeRecord {
                task_id: task.id,
                repo: repo.clone(),
                path: path.clone(),
                branch: branch.clone(),
                created_at: Utc::now(),
            };
            if let Err(e) = db::record_worktree(&state.db, &record).await {
                tracing::warn!(
                    error = %e,
                    task_id = %task.id,
                    "failed to record worktree; retention will not track it"
                );
            }
            return Ok(Plan {
                cwd: path.clone(),
                needs_lock: false,
                worktree: Some(Worktree { repo, path, branch }),
            });
        }
    }
    Ok(Plan {
        cwd: base.to_path_buf(),
        needs_lock: true,
        worktree: None,
    })
}

async fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn branch_name(task: &Task) -> String {
    let mut slug: String = task
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(40);
    let short = task.id.to_string();
    format!("favetto/{slug}-{}", &short[..8])
}

fn worktree_path(state: &State, cfg: &ExecutorSettings, repo: &Path, task: &Task) -> PathBuf {
    worktree_root(cfg, &state.data_dir, repo).join(task.id.to_string())
}

/// Resolve the configured worktree root. A leading `~` is expanded to the home
/// directory; an absolute path is used as-is; a non-`~` relative path (the
/// documented "repo-relative" form) is resolved against the repo root. Unset
/// falls back to `<data_dir>/worktrees`.
fn worktree_root(cfg: &ExecutorSettings, data_dir: &Path, repo: &Path) -> PathBuf {
    let root = cfg
        .worktree_dir
        .clone()
        .map(crate::paths::expand_tilde)
        .unwrap_or_else(|| data_dir.join("worktrees"));
    if root.is_absolute() {
        root
    } else {
        repo.join(root)
    }
}

async fn create_worktree(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(()); // reuse a leftover worktree for this task
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--force", "-B", branch])
        .arg(path)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn remove_worktree(pool: &SqlitePool, task_id: Uuid, repo: &Path, path: &Path, branch: &str) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await;
    if let Err(e) = out {
        tracing::warn!(error = %e, path = %path.display(), "failed to remove worktree");
    }
    // Drop the per-task branch as well (the task opted out of keeping work).
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", branch])
        .output()
        .await;
    let _ = db::forget_worktree(pool, task_id).await;
}

/// A terminal task is finished: its worktree may be reclaimed by retention.
fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Succeeded | TaskStatus::Failed)
}

/// Pick the tracked worktrees the retention policy should remove.
///
/// - a record whose task row no longer exists is an orphan and is removed;
/// - a record whose task is not terminal is always kept;
/// - a terminal task's worktree is removed when `keep_worktree` is false, or
///   when it is older than `cutoff` (age-based, newest `min_worktrees`
///   age-eligible ones are kept);
/// - `cutoff == None` (i.e. `days == 0`) disables age-based removal.
fn select_prunable(
    records: &[db::WorktreeRecord],
    tasks: &HashMap<Uuid, (TaskStatus, Option<chrono::DateTime<Utc>>)>,
    cutoff: Option<chrono::DateTime<Utc>>,
    keep_worktree: bool,
    min_worktrees: usize,
) -> Vec<db::WorktreeRecord> {
    let mut unconditional: Vec<db::WorktreeRecord> = Vec::new();
    let mut age_candidates: Vec<(chrono::DateTime<Utc>, db::WorktreeRecord)> = Vec::new();

    for record in records {
        match tasks.get(&record.task_id) {
            // Task row already pruned: orphan, always reclaim it.
            None => unconditional.push(record.clone()),
            // Active (`pending`/`running`/`awaiting_input`): never touched.
            Some((status, _)) if !is_terminal(*status) => {}
            Some((_, finished_at)) => {
                if !keep_worktree {
                    unconditional.push(record.clone());
                    continue;
                }
                if let Some(cutoff) = cutoff {
                    let when = finished_at.unwrap_or(record.created_at);
                    if when < cutoff {
                        age_candidates.push((when, record.clone()));
                    }
                }
            }
        }
    }

    // Keep the newest `min_worktrees` age-eligible worktrees.
    age_candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut selected = unconditional;
    selected.extend(
        age_candidates
            .into_iter()
            .skip(min_worktrees)
            .map(|(_, record)| record),
    );
    // A task id cannot be removed twice even if a future rule overlaps.
    let mut seen = HashSet::new();
    selected.retain(|record| seen.insert(record.task_id));
    selected
}

/// Remove tracked worktrees the retention policy has expired, drop their
/// branches, and `git worktree prune` each repo. Never fatal.
pub async fn prune_worktrees(state: &State) -> anyhow::Result<WorktreePruneStats> {
    let cfg = state.config.read().unwrap().executor.clone();
    let retention = cfg.worktree_retention.clone();
    let records = db::list_worktrees(&state.db).await?;

    let mut tasks = HashMap::new();
    for r in &records {
        if let Ok(Some(t)) = db::get_task(&state.db, r.task_id).await {
            tasks.insert(r.task_id, (t.status, t.finished_at));
        }
    }

    let cutoff =
        (retention.days > 0).then(|| Utc::now() - chrono::Duration::days(retention.days as i64));
    let selected = select_prunable(
        &records,
        &tasks,
        cutoff,
        cfg.keep_worktree,
        retention.min_worktrees as usize,
    );

    let mut stats = WorktreePruneStats::default();
    let mut repos: Vec<PathBuf> = Vec::new();
    for wt in selected {
        if !tasks.contains_key(&wt.task_id) {
            stats.orphans += 1;
        }
        remove_worktree(&state.db, wt.task_id, &wt.repo, &wt.path, &wt.branch).await;
        if !repos.contains(&wt.repo) {
            repos.push(wt.repo.clone());
        }
        stats.removed += 1;
    }
    for repo in repos {
        stats.repos_pruned += 1;
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "prune"])
            .output()
            .await;
    }
    Ok(stats)
}

/// Per-directory locks, so tasks without worktree isolation serialize.
#[derive(Clone, Default)]
struct DirLocks(Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>);

impl DirLocks {
    fn try_lock(&self, dir: &Path) -> Option<OwnedMutexGuard<()>> {
        let lock = self
            .0
            .lock()
            .unwrap()
            .entry(dir.to_path_buf())
            .or_default()
            .clone();
        lock.try_lock_owned().ok()
    }
}

/// React to a `TaskFinished` event: start the `:finished` dependents (once per
/// finished instance) and resolve any `:all_finished` fan-in barriers targeting
/// the finished task's name.
///
/// `needs` (and `spawn`) values are relative-path names, so a dependency on a
/// task in a subfolder is written as `pipelines/plan:finished`.
async fn start_dependents(state: &Arc<State>, ev: &Event) {
    let Some(name) = ev.payload.get("name").and_then(|n| n.as_str()) else {
        return;
    };

    let catalog = state.catalog.read().unwrap().clone();
    let mut finished: Vec<String> = Vec::new();
    let mut joins: Vec<String> = Vec::new();
    for def in &catalog {
        let Some(needs) = def.needs.as_deref() else {
            continue;
        };
        let (source, kind) = needs_parts(needs);
        if source != name {
            continue;
        }
        match kind {
            NeedsKind::Finished => finished.push(def.name.clone()),
            NeedsKind::AllFinished => joins.push(def.name.clone()),
        }
    }

    // Resolve the finished task row once; both dependency kinds need it.
    let prev = match ev
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => db::get_task(&state.db, id).await.ok().flatten(),
        None => None,
    };

    if !finished.is_empty() {
        // Attach the finished task's result as `input._prev` so dependent prompts
        // can reach it as `{{ prev.output }}` / `{{ prev.session_id }}`.
        let input = match &prev {
            Some(prev) => serde_json::json!({
                "_prev": {
                    "name": prev.name,
                    "task_id": prev.id.to_string(),
                    "status": prev.status.as_str(),
                    "output": prev.output,
                    "session_id": prev.session_id,
                }
            }),
            None => serde_json::json!({}),
        };
        let lineage = prev.as_ref().map(Lineage::child_of).unwrap_or_default();
        for dep in finished {
            tracing::info!(task = %dep, trigger = %name, "auto-starting dependent task");
            let dedupe = format!("needs:{}:{}", dep, ev.id);
            if let Err(e) =
                enqueue_with_lineage(state, dep, input.clone(), Some(dedupe), lineage).await
            {
                tracing::warn!(error = %e, "failed to enqueue dependent task");
            }
        }
    }

    if !joins.is_empty() {
        let Some(prev) = &prev else {
            return;
        };
        let root = prev.root_or_self();
        for dep in joins {
            maybe_start_join(state, &dep, name, root, Some(prev.id)).await;
        }
    }
}

/// After a `spawn` run completes — including an empty manifest — resolve the
/// `:all_finished` barriers that target the spawned child task. The parent's own
/// `TaskFinished` fires before the manifest is read, so this path is what makes
/// an empty fan-out (`[]`) still start its join.
async fn evaluate_join_barriers(
    state: &Arc<State>,
    target: &str,
    root_id: Uuid,
    parent_id: Option<Uuid>,
) {
    let joins: Vec<String> = state
        .catalog
        .read()
        .unwrap()
        .iter()
        .filter(|d| {
            d.needs.as_deref().is_some_and(|needs| {
                let (source, kind) = needs_parts(needs);
                source == target && kind == NeedsKind::AllFinished
            })
        })
        .map(|d| d.name.clone())
        .collect();
    for dep in joins {
        maybe_start_join(state, &dep, target, root_id, parent_id).await;
    }
}

/// Start `dep` once if every task named `target` in the workflow root `root_id`
/// is terminal. The barrier is derived from the database, so it is race-free and
/// survives a daemon restart; `dedupe = "join:{dep}:{root_id}"` keeps concurrent
/// last-finishers and event replays from enqueueing it twice.
async fn maybe_start_join(
    state: &Arc<State>,
    dep: &str,
    target: &str,
    root_id: Uuid,
    parent_id: Option<Uuid>,
) {
    match db::list_active_tasks_in_root(&state.db, root_id, target).await {
        Ok(active) if active.is_empty() => {}
        Ok(_) => return, // siblings are still pending/running
        Err(e) => {
            tracing::warn!(error = %e, root = %root_id, target, "failed to check fan-in barrier");
            return;
        }
    }

    let tasks = match db::list_tasks_in_root(&state.db, root_id, target).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(error = %e, root = %root_id, target, "failed to read fan-in results");
            return;
        }
    };
    let input = join_context(state, root_id, target, &tasks).await;
    tracing::info!(task = %dep, trigger = %target, root = %root_id, count = tasks.len(), "auto-starting fan-in task");
    let dedupe = format!("join:{dep}:{root_id}");
    if let Err(e) = enqueue_with_lineage(
        state,
        dep.to_string(),
        input,
        Some(dedupe),
        Lineage {
            parent_id,
            root_id: Some(root_id),
        },
    )
    .await
    {
        tracing::warn!(error = %e, "failed to enqueue fan-in task");
    }
}

/// Build the `input._prev` aggregate exposed to a fan-in successor as
/// `{{ prev.* }}`: the root, the target name, counts, and one entry per run.
async fn join_context(
    state: &Arc<State>,
    root_id: Uuid,
    target: &str,
    tasks: &[Task],
) -> serde_json::Value {
    let root_name = db::get_task(&state.db, root_id)
        .await
        .ok()
        .flatten()
        .map(|t| t.name);
    let count = tasks.len();
    let succeeded = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Succeeded)
        .count();
    let failed = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
        .count();
    let cancelled = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Cancelled)
        .count();
    let entries: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "task_id": t.id.to_string(),
                "name": t.name,
                "status": t.status.as_str(),
                "success": t.status == TaskStatus::Succeeded,
                "session_id": t.session_id,
                "input": t.input,
                "output": t.output,
            })
        })
        .collect();
    serde_json::json!({
        "_prev": {
            "kind": "all_finished",
            "root": {
                "task_id": root_id.to_string(),
                "name": root_name,
            },
            "target": target,
            "count": count,
            "succeeded": succeeded,
            "failed": failed,
            "cancelled": cancelled,
            "tasks": entries,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentManager, AgentRegistry};
    use crate::config::FavettoConfig;
    use crate::event_bus::EventBus;
    use crate::tasks::{TaskVar, VarType};
    use crate::webhooks::WebhookSecrets;
    use favetto_core::auth::Token;
    use std::sync::RwLock;

    fn def_with_vars() -> TaskDef {
        let var = |name: &str, required: bool| TaskVar {
            name: name.to_string(),
            prompt: "p".to_string(),
            default: None,
            required,
            multiline: false,
            var_type: VarType::String,
            choices: None,
        };
        TaskDef {
            name: "t".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            sign: None,
            vars: vec![var("required_one", true), var("optional_one", false)],
            prompt: "hello".to_string(),
        }
    }

    #[test]
    fn missing_required_vars_detects_absent_null_and_empty() {
        let def = def_with_vars();
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({})),
            ["required_one"]
        );
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": null })),
            ["required_one"]
        );
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "" })),
            ["required_one"]
        );
        // Present values (including typed ones) satisfy the requirement.
        assert!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty()
        );
        assert!(missing_required_vars(&def, &serde_json::json!({ "required_one": 3 })).is_empty());
        // Optional vars never appear.
        assert!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty()
        );
    }

    #[test]
    fn worktree_root_expands_tilde_and_resolves_relative() {
        let home = dirs::home_dir().expect("home directory");
        let data_dir = Path::new("/data");
        let repo = Path::new("/repo");
        let root_for = |dir: Option<&str>| {
            let cfg = ExecutorSettings {
                worktree_dir: dir.map(PathBuf::from),
                ..Default::default()
            };
            worktree_root(&cfg, data_dir, repo)
        };

        // `~` expands to the home directory before the absolute/relative decision,
        // so it is never joined onto the repo root.
        assert_eq!(
            root_for(Some("~/.local/share/favetto/worktrees")),
            home.join(".local/share/favetto/worktrees")
        );

        // A non-`~` relative value stays repo-relative.
        assert_eq!(
            root_for(Some(".favetto/worktrees")),
            Path::new("/repo/.favetto/worktrees")
        );

        // An absolute value is used as-is.
        assert_eq!(
            root_for(Some("/abs/worktrees")),
            Path::new("/abs/worktrees")
        );

        // Unset falls back to `<data_dir>/worktrees`.
        assert_eq!(root_for(None), Path::new("/data/worktrees"));
    }

    #[test]
    fn render_context_exposes_task_input_and_prev() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "triage".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({
                "issue_id": 7,
                "_prev": { "output": "done", "name": "triage" },
            }),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        let ctx = render_context(&task);
        assert_eq!(crate::template::render("{{ task.name }}", &ctx), "triage");
        assert_eq!(crate::template::render("{{ input.issue_id }}", &ctx), "7");
        assert_eq!(crate::template::render("{{ prev.output }}", &ctx), "done");
    }

    #[tokio::test]
    async fn unavailable_agent_fails_with_clear_error() {
        let dir = std::env::temp_dir().join(format!("favetto-executor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        let registry = AgentRegistry::from_config_with(&cfg, &|_| false).unwrap();

        let state = Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.clone(),
            dir.clone(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ));

        let task = Task {
            id: Uuid::new_v4(),
            name: "oneshot".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        let def = def_with_vars();
        let err = run_agent_task(&state, &task, "opencode", &def, "prompt", Path::new("/tmp"))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent 'opencode' is not installed"), "{err}");
        assert!(err.contains("not found on PATH"), "{err}");
    }

    fn task_with_session(session_id: Option<&str>, session_title: Option<&str>) -> Task {
        Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: session_id.map(str::to_string),
            session_title: session_title.map(str::to_string),
            parent_id: None,
            root_id: None,
        }
    }

    #[test]
    fn failed_run_preserves_session_info() {
        let mut task = task_with_session(None, None);
        let success = record_run_outcome(
            &mut task,
            Ok(RunOutcome {
                output: serde_json::json!({}),
                session_id: Some("ses_1".to_string()),
                session_title: Some("Fix the widget".to_string()),
                error: Some("exit 1".to_string()),
            }),
        );
        assert!(!success);
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.session_id.as_deref(), Some("ses_1"));
        assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
        assert!(task.error.as_deref().unwrap().contains("exit 1"));
        assert!(task.output.is_none());
    }

    #[test]
    fn successful_run_records_output_and_session() {
        let mut task = task_with_session(None, None);
        task.error = Some("stale".to_string());
        let success = record_run_outcome(
            &mut task,
            Ok(RunOutcome {
                output: serde_json::json!({ "ok": true }),
                session_id: Some("ses_1".to_string()),
                session_title: Some("Fix the widget".to_string()),
                error: None,
            }),
        );
        assert!(success);
        assert_eq!(task.status, TaskStatus::Succeeded);
        assert_eq!(task.output, Some(serde_json::json!({ "ok": true })));
        assert_eq!(task.session_id.as_deref(), Some("ses_1"));
        assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
        assert!(task.error.is_none());
    }

    #[test]
    fn run_without_agent_still_fails() {
        let mut task = task_with_session(Some("ses_keep"), Some("Keep"));
        let success = record_run_outcome(&mut task, Err(anyhow::anyhow!("no agent")));
        assert!(!success);
        assert_eq!(task.status, TaskStatus::Failed);
        // A pre-run failure never drops an already-resolved session.
        assert_eq!(task.session_id.as_deref(), Some("ses_keep"));
        assert_eq!(task.session_title.as_deref(), Some("Keep"));
        assert!(task.error.as_deref().unwrap().contains("no agent"));
    }

    #[test]
    fn terminal_outcome_overrides_awaiting_input_status() {
        let mut task = task_with_session(None, None);
        task.status = TaskStatus::AwaitingInput;
        let success = record_run_outcome(
            &mut task,
            Ok(RunOutcome {
                output: serde_json::json!({}),
                session_id: None,
                session_title: None,
                error: None,
            }),
        );
        assert!(success);
        assert_eq!(task.status, TaskStatus::Succeeded);

        let mut failed = task_with_session(None, None);
        failed.status = TaskStatus::AwaitingInput;
        let success = record_run_outcome(
            &mut failed,
            Ok(RunOutcome {
                output: serde_json::json!({}),
                session_id: None,
                session_title: None,
                error: Some("exit 1".to_string()),
            }),
        );
        assert!(!success);
        assert_eq!(failed.status, TaskStatus::Failed);
    }

    /// A fake `opencode session list --format json` executable that ignores its
    /// arguments and prints `json`.
    #[cfg(unix)]
    fn session_list_script(dir: &Path, json: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("opencode-session-list.sh");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{json}'\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A fake `opencode` that reports a live session id and keeps running unless
    /// invoked as `session list`, where it prints `title_json`.
    #[cfg(unix)]
    fn live_session_script(dir: &Path, title_json: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("opencode-live.sh");
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in\n  *\"session list\"*) printf '%s\\n' '{title_json}' ;;\n  *) printf '%s\\n' '{{\"sessionID\":\"ses_live\"}}' ; sleep 30 ;;\nesac\n"
        );
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A `State` whose `opencode` command is `command` (a title-lookup fixture).
    #[cfg(unix)]
    async fn title_state(dir: &Path, command: &Path) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        cfg.agents.insert(
            "opencode".to_string(),
            crate::config::AgentConfig {
                command: command.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.to_path_buf(),
            dir.to_path_buf(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_title_fills_blank_session_title() {
        let dir = std::env::temp_dir().join(format!("favetto-backfill-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
        let state = title_state(&dir, &script).await;

        let task = task_with_session(Some("ses_1"), None);
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get("opencode").unwrap();
        backfill_title(
            &state,
            task.id,
            agent,
            "ses_1".to_string(),
            dir.clone(),
            3,
            Duration::from_millis(1),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.session_title.as_deref(), Some("One shot title"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_title_does_not_overwrite_existing_title() {
        let dir = std::env::temp_dir().join(format!("favetto-backfill-keep-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
        let state = title_state(&dir, &script).await;

        let task = task_with_session(Some("ses_1"), Some("Existing"));
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get("opencode").unwrap();
        backfill_title(
            &state,
            task.id,
            agent,
            "ses_1".to_string(),
            dir.clone(),
            2,
            Duration::from_millis(1),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.session_title.as_deref(), Some("Existing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_title_fallback_reads_session_title() {
        let dir = std::env::temp_dir().join(format!("favetto-fallback-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"Fallback title"}]"#);
        let state = title_state(&dir, &script).await;
        let agent = state.registry.get("opencode").unwrap();

        assert_eq!(
            resolve_title_fallback(agent.clone(), Some("ses_1".to_string()), &dir)
                .await
                .as_deref(),
            Some("Fallback title")
        );
        // Without a parsed session id there is nothing to look up.
        assert_eq!(resolve_title_fallback(agent, None, &dir).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watch_title_while_running_skips_unknown_session() {
        let dir = std::env::temp_dir().join(format!("favetto-watch-missing-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, "[]");
        let state = title_state(&dir, &script).await;
        let agent = state.registry.get("opencode").unwrap();

        let watch = tokio::time::timeout(
            Duration::from_secs(2),
            watch_title_while_running(&state, Uuid::new_v4(), agent, "no-such-session", &dir),
        )
        .await
        .expect("watcher returns promptly for an unknown session");
        assert!(matches!(watch, TitleWatch::Skipped));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The acceptance test for #55: the title is resolved and persisted while the
    /// run is still in progress (not only at completion).
    #[cfg(unix)]
    #[tokio::test]
    async fn title_is_stored_while_run_is_still_in_progress() {
        let dir = std::env::temp_dir().join(format!("favetto-live-title-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = live_session_script(&dir, r#"[{"id":"ses_live","title":"Live title"}]"#);
        let state = title_state(&dir, &script).await;

        let task = task_with_session(None, None);
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get("opencode").unwrap();
        let ctx = crate::agents::AgentContext {
            cwd: Some(dir.clone()),
            prompt: Some("hi".to_string()),
            rows: 40,
            cols: 120,
            ..Default::default()
        };
        let info = state
            .agents
            .start(
                "opencode",
                agent.clone(),
                Some(task.id.to_string()),
                crate::agents::Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                ctx,
            )
            .unwrap();

        let watch = tokio::time::timeout(
            Duration::from_secs(10),
            watch_title_while_running(&state, task.id, agent, &info.id, &dir),
        )
        .await
        .expect("title resolves while the run is in progress");
        match watch {
            TitleWatch::Observed(Some(title)) => assert_eq!(title, "Live title"),
            _ => panic!("expected the watcher to observe a title"),
        }

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.session_id.as_deref(), Some("ses_live"));
        assert_eq!(stored.session_title.as_deref(), Some("Live title"));
        // The script is still sleeping: the run has not finished yet.
        assert!(state.agents.is_running(&info.id));

        let _ = state.agents.close(&info.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_text_keeps_head_and_tail_on_char_boundaries() {
        // Multi-byte input: truncation must not split a UTF-8 char.
        let s = "é".repeat(400);
        let (out, truncated) = truncate_text(&s, 100);
        assert!(truncated);
        assert!(out.starts_with('é'), "head preserved");
        assert!(out.ends_with('é'), "tail preserved");
        assert!(out.contains("truncated"), "marker present: {out}");
        // Head + tail stay within the cap; only the marker is added on top.
        assert!(out.len() <= 100 + 60, "capped: {} bytes", out.len());
        // A short input is returned unchanged.
        let (short, truncated) = truncate_text("hello", 100);
        assert_eq!(short, "hello");
        assert!(!truncated);
    }

    #[test]
    fn build_task_output_caps_and_dedupes() {
        let raw = "x".repeat(10_000);
        let parsed = serde_json::json!({ "text": raw });
        let value = build_task_output(&raw, &parsed, "opencode", None, None, 1000);
        assert_eq!(value["result"], serde_json::Value::Null);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["output_bytes"], 10_000);
        assert_eq!(value["agent"], "opencode");
        // The whole stored blob stays in the same order of magnitude as the cap.
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(encoded.len() < 2 * 1000, "bounded blob: {encoded}");
    }

    #[test]
    fn build_task_output_keeps_structured_result() {
        let raw = "done";
        let parsed = serde_json::json!([{ "event": "x" }]);
        let value = build_task_output(raw, &parsed, "opencode", Some("ses_1".into()), None, 1000);
        assert_eq!(value["truncated"], false);
        assert_eq!(value["output"], "done");
        assert_eq!(value["result"], parsed);
        assert_eq!(value["session_id"], "ses_1");
    }

    #[test]
    fn branch_name_slugifies_folder_qualified_task() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "pipelines/plan".to_string(),
            status: TaskStatus::Pending,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        let short = &task.id.to_string()[..8];
        assert_eq!(
            branch_name(&task),
            format!("favetto/pipelines-plan-{short}")
        );
    }

    fn wt_record(task_id: Uuid, created_at: chrono::DateTime<Utc>) -> db::WorktreeRecord {
        db::WorktreeRecord {
            task_id,
            repo: PathBuf::from("/repo"),
            path: PathBuf::from(format!("/data/worktrees/{task_id}")),
            branch: format!("favetto/t-{}", &task_id.to_string()[..8]),
            created_at,
        }
    }

    fn selected_ids(selected: &[db::WorktreeRecord]) -> HashSet<Uuid> {
        selected.iter().map(|r| r.task_id).collect()
    }

    #[test]
    fn terminal_status_classification() {
        assert!(is_terminal(TaskStatus::Succeeded));
        assert!(is_terminal(TaskStatus::Failed));
        assert!(!is_terminal(TaskStatus::Pending));
        assert!(!is_terminal(TaskStatus::Running));
        assert!(!is_terminal(TaskStatus::AwaitingInput));
    }

    #[test]
    fn select_prunable_keeps_active_and_fresh() {
        let now = Utc::now();
        let cutoff = Some(now - chrono::Duration::days(30));
        let fresh = wt_record(Uuid::new_v4(), now);
        let active = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

        let mut tasks = HashMap::new();
        tasks.insert(fresh.task_id, (TaskStatus::Succeeded, Some(now)));
        tasks.insert(active.task_id, (TaskStatus::Running, None));

        let selected = select_prunable(&[fresh.clone(), active.clone()], &tasks, cutoff, true, 0);
        assert!(selected.is_empty(), "got {selected:?}");
    }

    #[test]
    fn select_prunable_removes_expired_terminal() {
        let now = Utc::now();
        let cutoff = Some(now - chrono::Duration::days(30));
        let old = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

        let mut tasks = HashMap::new();
        tasks.insert(
            old.task_id,
            (
                TaskStatus::Succeeded,
                Some(now - chrono::Duration::days(60)),
            ),
        );

        let selected = select_prunable(std::slice::from_ref(&old), &tasks, cutoff, true, 0);
        assert_eq!(selected_ids(&selected), HashSet::from([old.task_id]));
    }

    #[test]
    fn select_prunable_honors_min_worktrees() {
        let now = Utc::now();
        let cutoff = Some(now - chrono::Duration::days(30));
        // Three expired, terminal worktrees; the newest one is kept.
        let newest = wt_record(Uuid::new_v4(), now - chrono::Duration::days(40));
        let middle = wt_record(Uuid::new_v4(), now - chrono::Duration::days(50));
        let oldest = wt_record(Uuid::new_v4(), now - chrono::Duration::days(60));

        let mut tasks = HashMap::new();
        for (record, finished) in [(&newest, 40), (&middle, 50), (&oldest, 60)] {
            tasks.insert(
                record.task_id,
                (
                    TaskStatus::Failed,
                    Some(now - chrono::Duration::days(finished)),
                ),
            );
        }

        let selected = select_prunable(
            &[newest.clone(), middle.clone(), oldest.clone()],
            &tasks,
            cutoff,
            true,
            1,
        );
        assert_eq!(
            selected_ids(&selected),
            HashSet::from([middle.task_id, oldest.task_id])
        );
        assert!(!selected_ids(&selected).contains(&newest.task_id));
    }

    #[test]
    fn select_prunable_removes_orphans_and_non_kept() {
        let now = Utc::now();
        let cutoff = Some(now - chrono::Duration::days(30));
        let orphan = wt_record(Uuid::new_v4(), now);
        let not_kept = wt_record(Uuid::new_v4(), now);

        let mut tasks = HashMap::new();
        // The orphan has no task row; the other finished just now.
        tasks.insert(not_kept.task_id, (TaskStatus::Succeeded, Some(now)));

        let selected = select_prunable(
            &[orphan.clone(), not_kept.clone()],
            &tasks,
            cutoff,
            false,
            0,
        );
        assert_eq!(
            selected_ids(&selected),
            HashSet::from([orphan.task_id, not_kept.task_id])
        );
    }

    #[test]
    fn select_prunable_days_zero_keeps_kept_worktrees() {
        let now = Utc::now();
        let kept = wt_record(Uuid::new_v4(), now - chrono::Duration::days(365));
        let orphan = wt_record(Uuid::new_v4(), now);

        let mut tasks = HashMap::new();
        tasks.insert(kept.task_id, (TaskStatus::Succeeded, None));

        // `cutoff == None` disables age-based removal, but orphans still go.
        let selected = select_prunable(&[kept.clone(), orphan.clone()], &tasks, None, true, 0);
        assert_eq!(selected_ids(&selected), HashSet::from([orphan.task_id]));
    }

    /// End-to-end: a catalog task declaring `[[vars]]` has the collected `input`
    /// substituted into its prompt before the agent is launched. Exercises the
    /// same `render_context` + `template::render` path as `run_one`, with a fake
    /// agent that records the argument vector it received.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_one_renders_input_vars_into_the_agent_prompt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("favetto-render-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let capture = dir.join("captured-args.txt");
        let script = dir.join("fake-opencode.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n",
                capture.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        cfg.agents.insert(
            "opencode".to_string(),
            crate::config::AgentConfig {
                command: script.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();

        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let state = Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.clone(),
            dir.clone(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ));

        let def = crate::tasks::parse_task_md(
            "favetto/open_github_issue",
            "agent = \"opencode\"\nmodel = \"deepseek-v4-flash\"\n\
             [[vars]]\nname = \"issue_description\"\nprompt = \"Describe\"\nrequired = true\nmultiline = true\n\
             [[vars]]\nname = \"repo\"\nprompt = \"Repo\"\nrequired = true\n\
             ---\nTarget repository: `{{ input.repo }}`.\n\nDescription: {{ input.issue_description }}\n",
        )
        .unwrap();

        let task = Task {
            id: Uuid::new_v4(),
            name: def.name.clone(),
            status: TaskStatus::Running,
            input: serde_json::json!({
                "repo": "acme/widgets",
                "issue_description": "the widget is broken",
            }),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        let plan = Plan {
            cwd: dir.clone(),
            needs_lock: false,
            worktree: None,
        };
        run_one(&state, task, def, plan, None).await;

        let captured = std::fs::read_to_string(&capture).unwrap_or_default();
        assert!(
            captured.contains("acme/widgets"),
            "input.repo was not rendered into the prompt: {captured}"
        );
        assert!(
            captured.contains("the widget is broken"),
            "input.issue_description was not rendered into the prompt: {captured}"
        );
        assert!(
            !captured.contains("{{ input."),
            "a raw placeholder survived into the agent prompt: {captured}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: an isolated run reads its `spawn_file` from the worktree
    /// *before* reclaiming it. The worktree used to be removed before the
    /// handoff was read, so the manifest was already gone and the `spawn` tasks
    /// were silently dropped.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_one_reads_spawn_file_before_reclaiming_worktree() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("favetto-spawn-wt-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&repo).unwrap();

        let run_git = |dir: PathBuf, args: Vec<String>| async move {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(&args)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };

        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            run_git(repo.clone(), args.into_iter().map(String::from).collect()).await;
        }
        std::fs::write(repo.join("README.md"), "seed").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-qm", "init"]] {
            run_git(repo.clone(), args.into_iter().map(String::from).collect()).await;
        }

        let branch = "favetto/spawn-test";
        run_git(
            repo.clone(),
            vec![
                "worktree".into(),
                "add".into(),
                "--force".into(),
                "-B".into(),
                branch.into(),
                worktree.to_string_lossy().into_owned(),
            ],
        )
        .await;

        // The agent's handoff lives inside the worktree.
        let handoff = worktree.join(".favetto").join("handoff");
        std::fs::create_dir_all(&handoff).unwrap();
        std::fs::write(handoff.join("manifest.json"), r#"[{"issue_id":7}]"#).unwrap();

        // A fake agent that succeeds without doing anything.
        let script = root.join("fake-agent.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        cfg.agents.insert(
            "opencode".to_string(),
            crate::config::AgentConfig {
                command: script.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();

        let pool = crate::db::open(&root.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let state = Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            root.clone(),
            root.clone(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ));

        let def = crate::tasks::parse_task_md(
            "favetto/triage_issues",
            "agent = \"opencode\"\nspawn = \"favetto/plan_issue\"\n\
             spawn_file = \".favetto/handoff/manifest.json\"\n---\nTriage the issues.\n",
        )
        .unwrap();

        let task = Task {
            id: Uuid::new_v4(),
            name: def.name.clone(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
        };
        db::insert_task(&state.db, &task).await.unwrap();

        let plan = Plan {
            cwd: worktree.clone(),
            needs_lock: false,
            worktree: Some(Worktree {
                repo: repo.clone(),
                path: worktree.clone(),
                branch: branch.to_string(),
            }),
        };
        let spawner_id = task.id;
        run_one(&state, task, def, plan, None).await;

        let pending = db::next_pending_tasks(&state.db, 10).await.unwrap();
        let spawned = pending
            .iter()
            .find(|t| {
                t.name == "favetto/plan_issue"
                    && t.input.get("issue_id").and_then(|v| v.as_i64()) == Some(7)
            })
            .unwrap_or_else(|| {
                panic!(
                    "spawn handoff was not enqueued; pending = {:?}",
                    pending
                        .iter()
                        .map(|t| (t.name.clone(), t.input.clone()))
                        .collect::<Vec<_>>()
                )
            });
        // The child carries workflow lineage: its parent is the spawner and the
        // spawner (started directly) is its own root.
        assert_eq!(spawned.parent_id, Some(spawner_id));
        assert_eq!(spawned.root_id, Some(spawner_id));
        assert!(
            !worktree.exists(),
            "the worktree should be reclaimed after the handoff is read"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `State` whose live catalog is `catalog`, with no usable agent.
    async fn join_state(dir: &Path, catalog: Vec<TaskDef>) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let cfg = FavettoConfig::default();
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.to_path_buf(),
            dir.to_path_buf(),
            Arc::new(RwLock::new(catalog)),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ))
    }

    /// A catalog task with the given `needs` value.
    fn needs_def(name: &str, needs: &str) -> TaskDef {
        crate::tasks::parse_task_md(
            name,
            &format!("agent = \"x\"\nneeds = {needs:?}\n---\nbody\n"),
        )
        .unwrap()
    }

    /// A task row with explicit lineage. `root = None` makes it its own root.
    fn lineage_task(
        name: &str,
        status: TaskStatus,
        root: Option<Uuid>,
        parent: Option<Uuid>,
    ) -> Task {
        Task {
            id: Uuid::new_v4(),
            name: name.to_string(),
            status,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
            parent_id: parent,
            root_id: root,
        }
    }

    fn finished_event(name: &str, id: Uuid) -> Event {
        Event {
            id: 1,
            kind: EventKind::TaskFinished,
            payload: serde_json::json!({
                "name": name,
                "task_id": id.to_string(),
                "success": true,
            }),
            created_at: Utc::now(),
        }
    }

    async fn pending_named(state: &State, name: &str) -> Vec<Task> {
        db::next_pending_tasks(&state.db, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(|t| t.name == name)
            .collect()
    }

    #[tokio::test]
    async fn finished_dependency_fires_per_instance_with_lineage() {
        let dir = std::env::temp_dir().join(format!("favetto-needs-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = join_state(&dir, vec![needs_def("follower", "target:finished")]).await;

        let mut target = lineage_task("target", TaskStatus::Succeeded, None, None);
        target.output = Some(serde_json::json!({ "ok": true }));
        db::insert_task(&state.db, &target).await.unwrap();

        start_dependents(&state, &finished_event("target", target.id)).await;

        let followers = pending_named(&state, "follower").await;
        assert_eq!(followers.len(), 1);
        assert_eq!(
            followers[0].input["_prev"]["task_id"],
            target.id.to_string()
        );
        assert_eq!(followers[0].input["_prev"]["output"]["ok"], true);
        // A dependency is a child of its predecessor; the predecessor is its own
        // root here, so the root is inherited.
        assert_eq!(followers[0].parent_id, Some(target.id));
        assert_eq!(followers[0].root_id, Some(target.id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn join_waits_for_all_siblings_then_fires_once() {
        let dir = std::env::temp_dir().join(format!("favetto-join-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;

        let root = lineage_task("root", TaskStatus::Succeeded, None, None);
        db::insert_task(&state.db, &root).await.unwrap();
        let a = lineage_task(
            "target",
            TaskStatus::Succeeded,
            Some(root.id),
            Some(root.id),
        );
        let b = lineage_task("target", TaskStatus::Running, Some(root.id), Some(root.id));
        db::insert_task(&state.db, &a).await.unwrap();
        db::insert_task(&state.db, &b).await.unwrap();

        start_dependents(&state, &finished_event("target", a.id)).await;
        assert!(
            pending_named(&state, "join").await.is_empty(),
            "the join fired while a sibling was still running"
        );

        let mut b_done = b.clone();
        b_done.status = TaskStatus::Succeeded;
        db::upsert_task(&state.db, &b_done).await.unwrap();
        start_dependents(&state, &finished_event("target", b.id)).await;

        let joins = pending_named(&state, "join").await;
        assert_eq!(joins.len(), 1);
        let prev = &joins[0].input["_prev"];
        assert_eq!(prev["kind"], "all_finished");
        assert_eq!(prev["target"], "target");
        assert_eq!(prev["count"], 2);
        assert_eq!(prev["succeeded"], 2);
        assert_eq!(prev["failed"], 0);
        assert_eq!(prev["root"]["task_id"], root.id.to_string());
        assert_eq!(prev["root"]["name"], "root");
        assert_eq!(prev["tasks"].as_array().unwrap().len(), 2);
        // The join inherits the root and records the last finisher as parent.
        assert_eq!(joins[0].parent_id, Some(b.id));
        assert_eq!(joins[0].root_id, Some(root.id));

        // Replaying the same finish event cannot enqueue the join twice.
        start_dependents(&state, &finished_event("target", b.id)).await;
        assert_eq!(pending_named(&state, "join").await.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn joins_are_root_scoped() {
        let dir = std::env::temp_dir().join(format!("favetto-join-roots-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;

        let root1 = lineage_task("root1", TaskStatus::Succeeded, None, None);
        let root2 = lineage_task("root2", TaskStatus::Succeeded, None, None);
        db::insert_task(&state.db, &root1).await.unwrap();
        db::insert_task(&state.db, &root2).await.unwrap();
        let a1 = lineage_task(
            "target",
            TaskStatus::Succeeded,
            Some(root1.id),
            Some(root1.id),
        );
        let b2 = lineage_task(
            "target",
            TaskStatus::Running,
            Some(root2.id),
            Some(root2.id),
        );
        db::insert_task(&state.db, &a1).await.unwrap();
        db::insert_task(&state.db, &b2).await.unwrap();

        // Root 1's only target finished: its join starts, root 2 is untouched.
        start_dependents(&state, &finished_event("target", a1.id)).await;
        let joins = pending_named(&state, "join").await;
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].root_id, Some(root1.id));

        // A stale finish event while root 2's target is still running must not
        // resolve root 2's barrier (the check is derived from the database).
        start_dependents(&state, &finished_event("target", b2.id)).await;
        assert_eq!(pending_named(&state, "join").await.len(), 1);

        let mut b2_done = b2.clone();
        b2_done.status = TaskStatus::Succeeded;
        db::upsert_task(&state.db, &b2_done).await.unwrap();
        start_dependents(&state, &finished_event("target", b2.id)).await;
        let joins = pending_named(&state, "join").await;
        assert_eq!(joins.len(), 2);
        let roots: Vec<Uuid> = joins.iter().filter_map(|t| t.root_id).collect();
        assert!(roots.contains(&root1.id));
        assert!(roots.contains(&root2.id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_fan_out_resolves_barrier() {
        let dir = std::env::temp_dir().join(format!("favetto-join-empty-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;
        let root = lineage_task("root", TaskStatus::Succeeded, None, None);
        db::insert_task(&state.db, &root).await.unwrap();

        // No `target` run was ever spawned: the empty barrier still resolves.
        evaluate_join_barriers(&state, "target", root.id, Some(root.id)).await;
        let joins = pending_named(&state, "join").await;
        assert_eq!(joins.len(), 1);
        let prev = &joins[0].input["_prev"];
        assert_eq!(prev["count"], 0);
        assert_eq!(prev["tasks"].as_array().unwrap().len(), 0);
        assert_eq!(prev["succeeded"], 0);

        // The dedupe key keeps a re-evaluation from enqueueing another join.
        evaluate_join_barriers(&state, "target", root.id, Some(root.id)).await;
        assert_eq!(pending_named(&state, "join").await.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failed_and_cancelled_children_satisfy_barrier() {
        let dir = std::env::temp_dir().join(format!("favetto-join-fail-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = join_state(&dir, vec![needs_def("join", "target:all_finished")]).await;
        let root = lineage_task("root", TaskStatus::Succeeded, None, None);
        db::insert_task(&state.db, &root).await.unwrap();
        let failed = lineage_task("target", TaskStatus::Failed, Some(root.id), Some(root.id));
        let cancelled = lineage_task(
            "target",
            TaskStatus::Cancelled,
            Some(root.id),
            Some(root.id),
        );
        db::insert_task(&state.db, &failed).await.unwrap();
        db::insert_task(&state.db, &cancelled).await.unwrap();

        start_dependents(&state, &finished_event("target", failed.id)).await;
        let joins = pending_named(&state, "join").await;
        assert_eq!(joins.len(), 1);
        let prev = &joins[0].input["_prev"];
        assert_eq!(prev["count"], 2);
        assert_eq!(prev["succeeded"], 0);
        assert_eq!(prev["failed"], 1);
        assert_eq!(prev["cancelled"], 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

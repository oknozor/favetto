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
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use sqlx::SqlitePool;
use tokio::process::Command;
use tokio::sync::{OwnedMutexGuard, Semaphore};
use uuid::Uuid;

use favetto_core::model::{
    AgentUsage, Event, EventKind, Failure, FailureKind, RunStatus, RunSummary, Task, TaskRun,
    TaskStatus,
};

use crate::agents::{resolve_session_title, Agent, TITLE_POLL_ATTEMPTS, TITLE_POLL_INTERVAL};
use crate::config::{ExecutorSettings, RetrySettings, StaleRunPolicy};
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::tasks::{needs_parts, NeedsKind, TaskDef};

/// The result of an agent run. `failure` is `Some` for a non-zero exit or a
/// missing session; `session_id`/`session_title` are carried either way so a
/// failed run keeps its reattach handle and its title.
struct RunOutcome {
    output: serde_json::Value,
    session_id: Option<String>,
    session_title: Option<String>,
    /// The agent process exit code, when one was observed. Recorded on the run.
    exit_code: Option<i32>,
    /// Token/cost usage the agent reported for the run, when any.
    usage: Option<AgentUsage>,
    /// `Some` for a non-zero exit / vanished session / failed turn.
    failure: Option<Failure>,
    /// The interactive TUI process is still alive after its turn finished. Its
    /// worktree must be kept, since the session still runs inside it.
    alive: bool,
}

/// A run that failed before producing a [`RunOutcome`], tagged with its kind.
/// Used for failures raised before the agent is launched (no agent configured,
/// agent unavailable, PTY spawn fault).
struct RunError {
    kind: FailureKind,
    message: String,
}

impl RunError {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Fold a finished (or failed-to-start) agent run onto the task row. Returns
/// whether the run succeeded. Session info is assigned on both arms.
fn record_run_outcome(task: &mut Task, outcome: Result<RunOutcome, RunError>) -> bool {
    match outcome {
        Ok(run) => {
            task.session_id = run.session_id;
            task.session_title = run.session_title;
            match run.failure {
                Some(failure) => {
                    task.status = TaskStatus::Failed;
                    task.error = Some(failure.message.clone());
                    task.failure = Some(failure);
                    false
                }
                None => {
                    task.status = TaskStatus::Succeeded;
                    task.output = Some(run.output);
                    task.error = None;
                    task.failure = None;
                    true
                }
            }
        }
        Err(e) => {
            task.status = TaskStatus::Failed;
            task.error = Some(e.message.clone());
            task.failure = Some(Failure::new(e.kind, e.message));
            false
        }
    }
}

/// Recover the token/cost usage a finished run reported. Structured agents embed
/// their folded [`RunSummary`] in `output`, so prefer its `usage`; interactive
/// runs have no machine output, so fall back to the folded live-state usage
/// (`live`). Returns `None` when nothing was reported, so a run with no usage
/// stores NULL and stays out of `SUM(...)`.
fn reported_usage(output: &serde_json::Value, live: Option<AgentUsage>) -> Option<AgentUsage> {
    let mut usage = serde_json::from_value::<RunSummary>(output.clone())
        .map(|summary| summary.usage)
        .unwrap_or_default();
    if usage.is_empty() {
        if let Some(live) = live {
            usage = live;
        }
    }
    (!usage.is_empty()).then_some(usage)
}

/// Persist an automatic retry for a failed attempt and announce the re-enqueue.
///
/// The task is flipped back to `pending` with a `retry_at` deadline (the
/// configured backoff after the attempt that just failed); the dispatcher claims
/// it once the deadline passes and records the next run under `attempt + 1`. The
/// failed run history is kept. Returns `true` when a retry was scheduled, so the
/// caller suppresses the terminal events for this attempt.
async fn schedule_auto_retry(state: &State, task: &Task, retry: &RetrySettings) -> bool {
    let Some(failure) = task.failure.as_ref() else {
        return false;
    };
    if !retry.should_retry(task.attempt, failure) {
        return false;
    }
    let delay = retry.backoff_delay(task.attempt);
    let at =
        Utc::now() + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());
    match db::schedule_task_retry(&state.db, task.id, at).await {
        Ok(true) => {
            tracing::info!(
                task_id = %task.id,
                attempt = task.attempt,
                retry_in_ms = delay.as_millis(),
                "scheduling automatic retry"
            );
            if let Ok(Some(fresh)) = db::get_task(&state.db, task.id).await {
                state
                    .bus
                    .publish(ServerPush::TaskUpdated(Box::new(fresh.summary())));
                state
                    .emit_event(
                        EventKind::TaskIdle,
                        serde_json::json!({ "name": fresh.name, "task_id": fresh.id }),
                    )
                    .await;
            }
            true
        }
        Ok(false) => false,
        Err(e) => {
            tracing::warn!(task_id = %task.id, error = %e, "failed to schedule automatic retry");
            false
        }
    }
}

/// What a startup [`reconcile`] pass changed. All zero on a clean start or on a
/// second, idempotent pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Active runs marked `interrupted`.
    pub runs_interrupted: u64,
    /// Stale tasks failed (`stale_run = "fail"`).
    pub tasks_failed: u64,
    /// Stale tasks re-enqueued (`stale_run = "retry"`).
    pub tasks_retried: u64,
    /// Pending tasks failed because their catalog definition vanished.
    pub tasks_invalidated: u64,
}

/// The error recorded when a previous daemon left a run in flight. Kept stable:
/// `stale_run = "fail"` reproduces the message the old
/// `db::fail_interrupted_tasks` wrote.
const INTERRUPTED_ERROR: &str = "interrupted by daemon restart";

/// Reconcile state left behind by a previous daemon instance. Run at startup
/// before the executor claims anything, and safe to run repeatedly: a second
/// pass changes nothing.
///
/// 1. Every active run (`pending`/`running`/`awaiting_input`) becomes
///    `interrupted` — its agent process died with the old daemon.
/// 2. Each active task's fate follows [`ExecutorSettings::stale_run`]: `fail`
///    (default) marks it failed, `retry` re-enqueues it for a fresh attempt.
/// 3. Pending tasks whose catalog definition no longer exists are failed with
///    [`FailureKind::InvalidInput`] instead of waiting for the dispatcher to
///    reject them lazily.
///
/// Terminal tasks are never touched.
pub async fn reconcile(state: &State) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let interrupted = Failure::new(FailureKind::Infrastructure, INTERRUPTED_ERROR);

    for run in db::list_active_task_runs(&state.db).await? {
        if db::interrupt_run(&state.db, run.id, INTERRUPTED_ERROR, &interrupted).await? {
            report.runs_interrupted += 1;
        }
    }

    // Active tasks are reconciled independently of their run rows so a legacy
    // database (a `running` task with no run) is still recovered.
    for task in db::list_active_tasks(&state.db).await? {
        match state.config.executor.stale_run {
            StaleRunPolicy::Fail => {
                if db::fail_stale_task(&state.db, task.id, INTERRUPTED_ERROR, &interrupted).await? {
                    report.tasks_failed += 1;
                }
            }
            StaleRunPolicy::Retry => {
                if db::retry_stale_task(&state.db, task.id).await? {
                    report.tasks_retried += 1;
                }
            }
        }
    }

    let known: HashSet<String> = state
        .catalog
        .read()
        .iter()
        .map(|def| def.name.clone())
        .collect();
    for task in db::list_pending_tasks(&state.db).await? {
        if known.contains(&task.name) {
            continue;
        }
        let message = format!("task '{}' not found in the catalog", task.name);
        let failure = Failure::new(FailureKind::InvalidInput, message.clone());
        if db::fail_pending_task(&state.db, task.id, &message, &failure).await? {
            report.tasks_invalidated += 1;
        }
    }

    Ok(report)
}

/// Spawn the dispatcher and the dependency listener.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
    let cfg = state.config.executor.clone();

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
                fail_task(
                    &state,
                    task,
                    FailureKind::InvalidInput,
                    "task not found in the catalog",
                )
                .await;
                continue;
            };

            // Required input variables must be present even for unattended starts
            // (hooks / needs / spawn / schedule / raw RPC), which never prompt.
            if let Some(missing) = missing_required_vars(&def, &task.input).first() {
                fail_task(
                    &state,
                    task,
                    FailureKind::InvalidInput,
                    &format!("task '{}' requires input variable '{missing}'", def.name),
                )
                .await;
                continue;
            }

            let base = resolve_base_dir(&def, &task);
            let plan =
                match make_plan(&state, &cfg, &base, &task, def.worktree.unwrap_or(true)).await {
                    Ok(p) => p,
                    Err(e) => {
                        fail_task(
                            &state,
                            task,
                            FailureKind::Infrastructure,
                            &format!("worktree setup failed: {e}"),
                        )
                        .await;
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

            let Some(run) = db::claim_task(&state.db, &task).await.unwrap_or(None) else {
                if let Some(wt) = &plan.worktree {
                    remove_worktree(&state.db, task.id, &wt.repo, &wt.path, &wt.branch).await;
                }
                continue; // already claimed
            };

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
                run_one(&state, task, run, def, plan, guard).await;
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
/// task is its own workflow root (no lineage). `interactive` marks a
/// user-started run that should execute in the agent's real TUI.
pub async fn enqueue_task(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
    interactive: bool,
) -> anyhow::Result<Task> {
    enqueue_with_lineage(
        state,
        name,
        input,
        dedupe_key,
        Lineage::default(),
        interactive,
    )
    .await
}

/// Enqueue a task with workflow lineage, announcing it on the bus and emitting
/// `TaskIdle`.
async fn enqueue_with_lineage(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
    lineage: Lineage,
    interactive: bool,
) -> anyhow::Result<Task> {
    enqueue_with_id(
        state,
        Uuid::new_v4(),
        name,
        input,
        dedupe_key,
        lineage,
        interactive,
    )
    .await
}

/// Enqueue a task under a caller-assigned id, announcing it on the bus and
/// emitting `TaskIdle`. `workflow.create` needs a stable id so dependency rows
/// can reference it before the insert.
async fn enqueue_with_id(
    state: &State,
    id: Uuid,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
    lineage: Lineage,
    interactive: bool,
) -> anyhow::Result<Task> {
    let task = Task {
        id,
        name: name.clone(),
        status: TaskStatus::Pending,
        attempt: 0,
        input,
        output: None,
        dedupe_key,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        failure: None,
        session_id: None,
        session_title: None,
        parent_id: lineage.parent_id,
        root_id: lineage.root_id,
        interactive,
    };
    db::insert_task(&state.db, &task).await?;
    state.metrics.inc_tasks();
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

/// Args for [`enqueue_dynamic`]: one runtime node with a caller-assigned id,
/// lineage, and per-instance dependencies.
pub struct DynamicNode {
    pub id: Uuid,
    pub name: String,
    pub input: serde_json::Value,
    pub dedupe_key: Option<String>,
    pub root_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub depends_on: Vec<Uuid>,
}

/// Enqueue a runtime task with a caller-assigned id and per-instance
/// dependencies. Used by `workflow.create`, which resolves every node's id (and
/// therefore validates the whole DAG) before inserting anything.
pub async fn enqueue_dynamic(state: &State, node: DynamicNode) -> anyhow::Result<Task> {
    let lineage = Lineage {
        parent_id: node.parent_id,
        root_id: Some(node.root_id),
    };
    let task = enqueue_with_id(
        state,
        node.id,
        node.name,
        node.input,
        node.dedupe_key,
        lineage,
        false,
    )
    .await?;
    record_dependencies(state, task.id, &node.depends_on).await?;
    Ok(task)
}

/// Add one runtime task to an existing root (or make it its own root when
/// `root_id` is `None`), recording per-instance dependencies. Used by
/// `workflow.spawn`.
pub async fn spawn_dynamic(
    state: &State,
    name: String,
    input: serde_json::Value,
    root_id: Option<Uuid>,
    depends_on: &[Uuid],
    dedupe_key: Option<String>,
) -> anyhow::Result<Task> {
    // `INSERT OR IGNORE` on the dedupe key would leave us with a transient id;
    // look the canonical row up first so a re-submission is a pure no-op.
    if let Some(key) = &dedupe_key {
        if let Some(existing) = db::get_task_by_dedupe_key(&state.db, key).await? {
            return Ok(existing);
        }
    }
    let lineage = Lineage {
        parent_id: root_id,
        root_id,
    };
    let task = enqueue_with_id(
        state,
        Uuid::new_v4(),
        name,
        input,
        dedupe_key,
        lineage,
        false,
    )
    .await?;
    record_dependencies(state, task.id, depends_on).await?;
    Ok(task)
}

/// Persist the dependency edges from a dynamic task to its predecessors.
async fn record_dependencies(
    state: &State,
    task_id: Uuid,
    depends_on: &[Uuid],
) -> anyhow::Result<()> {
    let now = Utc::now();
    for predecessor in depends_on {
        db::insert_dependency(&state.db, task_id, *predecessor, now).await?;
    }
    Ok(())
}

async fn run_one(
    state: &Arc<State>,
    mut task: Task,
    run: TaskRun,
    def: TaskDef,
    plan: Plan,
    _dir_guard: Option<OwnedMutexGuard<()>>,
) {
    // `claim_task` already persisted Running and the attempt; mirror both on our
    // copy so the task update we publish and persist matches the run.
    task.status = TaskStatus::Running;
    task.attempt = run.attempt;
    task.started_at = run.started_at.or_else(|| Some(Utc::now()));
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    state
        .emit_event(
            EventKind::TaskStarted,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;

    let config = state.config.as_ref().clone();

    let prompt = render_task_prompt(&def, &task);

    let agent_name = def.agent.clone().or_else(|| config.agent.default.clone());
    let outcome: Result<RunOutcome, RunError> = match agent_name.as_deref() {
        Some(name) => run_agent_task(state, &task, name, &def, &prompt, &plan.cwd)
            .await
            .map_err(|e| RunError::new(FailureKind::Infrastructure, e.to_string())),
        None => Err(RunError::new(
            FailureKind::InvalidInput,
            format!(
                "task '{}' has no agent: set `agent` in the task or `[agent].default` in the config",
                def.name
            ),
        )),
    };

    // An interactive run that finished its turn keeps its TUI (and therefore its
    // worktree) alive so the Agent panel can still attach to it. A headless run
    // may also have a concurrently attached interactive panel session bound to
    // the task; that session still lives in the worktree, so keep it too.
    let session_alive = matches!(&outcome, Ok(run) if run.alive)
        || state.agents.has_live_interactive(&task.id.to_string());
    let exit_code = outcome.as_ref().ok().and_then(|run| run.exit_code);
    // Take the reported usage out before `record_run_outcome` consumes the outcome.
    let usage = outcome.as_ref().ok().and_then(|run| run.usage.clone());
    let success = record_run_outcome(&mut task, outcome);
    task.finished_at = Some(Utc::now());

    // Compare-and-set the terminal outcome: only write while the task is still
    // active. A `tasks.cancel`/`workflow.cancel` that landed while the agent ran
    // leaves the row `cancelled`; this run's own result must neither resurrect the
    // row nor fire its terminal events.
    let finished = match db::finish_active_task(&state.db, &task).await {
        Ok(finished) => finished,
        Err(e) => {
            tracing::warn!(task_id = %task.id, error = %e, "failed to record task outcome");
            false
        }
    };

    // Finalize the attempt's run. Task-level output/session stay authoritative for
    // `tasks.get`; the run is the history. A run whose task was cancelled
    // concurrently is recorded `cancelled` too, so the two rows agree.
    let mut finished_run = run;
    finished_run.status = if !finished {
        RunStatus::Cancelled
    } else if success {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    finished_run.session_id = task.session_id.clone();
    finished_run.finished_at = task.finished_at;
    finished_run.exit_code = exit_code;
    finished_run.error = task.error.clone();
    finished_run.failure = task.failure.clone();
    // Attribute the run and persist its per-attempt usage. `agent_name` was
    // resolved above; `model` is the task's configured model, if any.
    finished_run.agent = agent_name.clone();
    finished_run.model = def.model.clone();
    finished_run.usage = usage;
    let _ = db::finalize_task_run(&state.db, &finished_run).await;

    if !finished {
        // The task was cancelled out from under the run: the cancellation owns the
        // row and has already announced itself. Suppress the terminal events and any
        // `spawn`/`needs`/join successor, but still reclaim the worktree below.
        tracing::info!(
            task_id = %task.id,
            "run finished after task cancellation; leaving the cancelled row untouched"
        );
    } else {
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

        // A failure eligible for an automatic retry re-enqueues the task for a fresh
        // attempt instead of ending it: the attempt is not terminal, so the terminal
        // events (and any `spawn`) wait for a later attempt.
        let retrying = !success && schedule_auto_retry(state, &task, &config.executor.retry).await;

        if !retrying {
            let kind = if success {
                EventKind::TaskCompleted
            } else {
                EventKind::TaskFailed
            };
            state
                .emit_event(kind, serde_json::json!({ "task_id": task.id }))
                .await;
            state
                .emit_event(EventKind::TaskFinished, finished_payload(&task, success))
                .await;

            if success {
                if let Some(spawn_task) = def.spawn.clone() {
                    if let Err(e) = spawn_from_manifest(state, &task, &def, &plan.cwd).await {
                        tracing::warn!(task = %task.name, error = %e, "failed to spawn tasks from handoff");
                    }
                    // The parent's own `TaskFinished` was emitted above, before the
                    // manifest was read, so a fan-in on the spawned child is evaluated
                    // here instead. This is also what lets an empty manifest (`[]`)
                    // resolve the barrier. A new-root spawn starts independent workflows,
                    // so the parent's root has no barrier of its own to resolve for them.
                    if !def.spawn_new_root {
                        evaluate_join_barriers(
                            state,
                            &spawn_task,
                            task.root_or_self(),
                            Some(task.id),
                        )
                        .await;
                    }
                }
            }
        }
    }

    // Reclaim the worktree only after the handoff above has been read. For an
    // isolated run the `spawn_file` lives inside the worktree, so removing it
    // earlier made the manifest unreadable and silently dropped `spawn` tasks.
    //
    // This block is outside the `if finished` arm above on purpose: a run that
    // was cancelled out from under the executor (`!finished`, e.g. the daemon
    // killed the row or a `tasks.cancel` landed) still reaches it and cleans up,
    // so a cancellation cannot strand the branch or worktree.
    if let Some(wt) = &plan.worktree {
        if config.executor.keep_worktree {
            tracing::info!(path = %wt.path.display(), "worktree kept");
        } else if session_alive {
            // The interactive session still runs inside this worktree; retention
            // (or a later disconnect) reclaims it once the task is terminal.
            tracing::info!(
                path = %wt.path.display(),
                "worktree kept while the interactive session is still live"
            );
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

/// Byte budget for one finished task's `output` when it is embedded in a
/// dependent's `input._prev`.
///
/// The rendered prompt is handed to the agent as a single command-line
/// argument, and Linux caps one argument at 128 KiB (`MAX_ARG_STRLEN`). When it
/// is exceeded `execve` fails with `E2BIG`, which the PTY spawner surfaces as an
/// opaque `fatal runtime error: assertion failed: output.write(&bytes).is_ok()`
/// abort. Keep the embedded payload well under that limit.
const PREV_OUTPUT_BYTES: usize = 48 * 1024;

/// Overall byte budget for the `_prev.tasks` array of an `:all_finished` fan-in,
/// so N runs cannot multiply a full-size output into an oversized prompt.
const PREV_TASKS_BYTES: usize = 64 * 1024;

/// Smallest meaningful per-task slice of [`PREV_TASKS_BYTES`].
const MIN_PREV_OUTPUT_BYTES: usize = 512;

/// Cap on `envelope.summary` so a runaway agent cannot make the summary huge.
const ENVELOPE_SUMMARY_BYTES: usize = 512;

/// Smallest meaningful per-field slice when bounding envelope arrays.
const MIN_ENVELOPE_FIELD_BYTES: usize = 256;

/// Keep the tail of `s` on a UTF-8 boundary, so a truncated summary reads as the
/// final (usually most useful) part of an agent's output.
fn tail_truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = ceil_char_boundary(s, s.len() - max);
    format!("…{}", &s[start..])
}

/// Synthesise a summary from the tail of the capped raw output when the agent
/// offers no structured envelope.
fn synthesized_summary(capped_raw: &str) -> String {
    let last = capped_raw
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("");
    tail_truncate(last, ENVELOPE_SUMMARY_BYTES)
}

/// Byte cap for the `summary` carried by a `TaskFinished` event. Reuses the
/// envelope cap so an event can never be larger than a `_prev` summary.
const FINISHED_SUMMARY_BYTES: usize = ENVELOPE_SUMMARY_BYTES;

/// The optional bounded summary for a finish: the result envelope's `summary`
/// when the run produced output, else the human-readable error (a task that never
/// started has no output). `None` when there is nothing to say.
fn finished_summary(task: &Task) -> Option<String> {
    let text = task
        .output
        .as_ref()
        .and_then(|o| o.get("envelope"))
        .and_then(|e| e.get("summary"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            task.error
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })?;
    Some(tail_truncate(text, FINISHED_SUMMARY_BYTES))
}

/// Additive `TaskFinished` payload: the legacy `{name, task_id, success}` plus
/// `status`, `attempt`, `retryable` and an optional bounded `summary`, so a
/// consumer can interpret a finish without a follow-up `tasks.get`.
///
/// `attempt` is the 1-based attempt counter recorded on the task (0 only for a
/// task that never reached a claim), matching `workflow.inspect`.
pub(crate) fn finished_payload(task: &Task, success: bool) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "name": &task.name,
        "task_id": task.id,
        "success": success,
        "status": task.status.as_str(),
        "attempt": task.attempt,
        "retryable": task.failure.as_ref().is_some_and(|f| f.retryable),
    });
    if let Some(summary) = finished_summary(task) {
        payload["summary"] = serde_json::Value::String(summary);
    }
    payload
}

/// The envelope for a run whose agent produced no structured one: a summary
/// synthesised from the raw transcript and empty collections.
fn synthesized_envelope(capped_raw: &str) -> serde_json::Value {
    serde_json::json!({
        "summary": synthesized_summary(capped_raw),
        "artifacts": [],
        "findings": [],
        "outputs": {},
        "continuation": serde_json::Value::Null,
    })
}

/// Whether an object is a result envelope. `summary` is the one required field,
/// so a JSON string there is the discriminator.
fn is_envelope_object(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.get("summary").is_some_and(serde_json::Value::is_string)
}

/// Extract a result envelope from an agent's structured `result`, if it carries
/// one: an explicit `envelope` wrapper wins, then the result itself when it
/// looks like an envelope, then an envelope encoded in the result's `text`
/// (which covers agents that emit the envelope as their final answer).
fn envelope_from_result(result: &serde_json::Value) -> Option<serde_json::Value> {
    let obj = result.as_object()?;
    if let Some(envelope) = obj.get("envelope") {
        if envelope.is_object() {
            return Some(envelope.clone());
        }
    }
    if is_envelope_object(obj) {
        return Some(result.clone());
    }
    if let Some(text) = obj.get("text").and_then(|value| value.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text.trim()) {
            if parsed.as_object().is_some_and(is_envelope_object) {
                return Some(parsed);
            }
        }
    }
    None
}

/// Ensure an envelope always has the five documented keys with sane types,
/// preserving any extra keys the agent supplied. `summary` is always a string,
/// `artifacts`/`findings` are always arrays, `outputs` defaults to `{}`, and
/// `continuation` defaults to `null`.
fn normalize_envelope(mut envelope: serde_json::Value) -> serde_json::Value {
    let Some(obj) = envelope.as_object_mut() else {
        return synthesized_envelope("");
    };
    if !obj.get("summary").is_some_and(serde_json::Value::is_string) {
        obj.insert(
            "summary".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    for key in ["artifacts", "findings"] {
        if !obj.get(key).is_some_and(serde_json::Value::is_array) {
            obj.insert(key.to_string(), serde_json::Value::Array(Vec::new()));
        }
    }
    if !obj.contains_key("outputs") {
        obj.insert("outputs".to_string(), serde_json::json!({}));
    }
    if !obj.contains_key("continuation") {
        obj.insert("continuation".to_string(), serde_json::Value::Null);
    }
    envelope
}

/// Bound an envelope's fields in place so an embedded `_prev` copy stays within
/// `budget`. Oversized `artifacts`/`findings`/`outputs` are replaced by a
/// bounded string, trading shape for a hard byte cap; the outer `truncated` flag
/// records the loss.
fn bound_envelope(env: &mut serde_json::Map<String, serde_json::Value>, budget: usize) {
    if let Some(serde_json::Value::String(summary)) = env.get_mut("summary") {
        *summary = tail_truncate(summary.as_str(), ENVELOPE_SUMMARY_BYTES);
    }
    let per_field = (budget / 3).max(MIN_ENVELOPE_FIELD_BYTES);
    for key in ["artifacts", "findings", "outputs"] {
        let Some(value) = env.get_mut(key) else {
            continue;
        };
        let serialized = value.to_string();
        if serialized.len() > per_field {
            let (capped, _) = truncate_text(&serialized, per_field);
            *value = serde_json::Value::String(capped);
        }
    }
}

/// Bound a finished task's stored `output` (or `input`) before embedding it as
/// `_prev`. A payload at or under `cap` is returned verbatim. A larger one keeps
/// its object shape but truncates the bulky `output` field head+tail (the tail
/// usually carries the agent's final summary), bounds the `envelope`, and drops
/// the parsed `result` duplicate, flagging `truncated`. An unknown shape falls
/// back to a bounded JSON string so the prompt can never exceed the argument
/// limit.
fn bounded_prev_value(value: Option<&serde_json::Value>, cap: usize) -> serde_json::Value {
    let Some(value) = value else {
        return serde_json::Value::Null;
    };
    let serialized = value.to_string();
    if serialized.len() <= cap {
        return value.clone();
    }
    if let Some(obj) = value.as_object() {
        let mut obj = obj.clone();
        let mut changed = false;
        if let Some(serde_json::Value::String(text)) = obj.get_mut("output") {
            let (capped, _) = truncate_text(text, cap);
            *text = capped;
            changed = true;
        }
        if obj.remove("result").is_some() {
            changed = true;
        }
        if let Some(env) = obj
            .get_mut("envelope")
            .and_then(|value| value.as_object_mut())
        {
            bound_envelope(env, cap);
            changed = true;
        }
        if changed {
            obj.insert("truncated".to_string(), serde_json::Value::Bool(true));
            return serde_json::Value::Object(obj);
        }
    }
    let (capped, _) = truncate_text(&serialized, cap);
    serde_json::Value::String(capped)
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
    // An envelope is embedded when the agent's structured result carries one;
    // otherwise the summary is synthesised from the tail of the raw output.
    let structured = envelope_from_result(parsed);
    // The default parser wraps the raw text as `{"text": raw}`; that is a
    // byte-for-byte duplicate of `output`, so store `null` instead. When the
    // parsed result *is* the envelope, the envelope is the canonical copy and
    // `result` is de-duped the same way.
    let result = if parsed
        .as_object()
        .map(|o| o.len() == 1 && o.get("text").and_then(|v| v.as_str()) == Some(raw))
        .unwrap_or(false)
        || structured.as_ref() == Some(parsed)
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
    let mut envelope = structured
        .map(normalize_envelope)
        .unwrap_or_else(|| normalize_envelope(synthesized_envelope(&capped_raw)));
    // `normalize_envelope` always yields an object; guard anyway so a future
    // change to it cannot panic a long-running run.
    if let Some(env) = envelope.as_object_mut() {
        bound_envelope(env, max / 2);
    }
    serde_json::json!({
        "agent": agent,
        "session_id": session_id,
        "session_title": session_title,
        "output_bytes": raw.len(),
        "truncated": truncated,
        "output": capped_raw,
        "result": result,
        "envelope": envelope,
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

    // A new-root spawn restarts the workflow: each child is its own root, so its
    // own `:all_finished` fan-in is not deduped against this task's root. Otherwise
    // the child inherits this task's lineage, as with any other spawn.
    let lineage = if def.spawn_new_root {
        Lineage::default()
    } else {
        Lineage::child_of(task)
    };
    for (i, item) in items.into_iter().enumerate() {
        let dedupe = format!("spawn:{}:{i}", task.id);
        match enqueue_with_lineage(
            state,
            spawn_task.to_string(),
            item,
            Some(dedupe),
            lineage,
            false,
        )
        .await
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

    // A user-started task runs in the agent's real interactive TUI, seeded with
    // the rendered prompt, so the Agent panel attaches to the single writer while
    // the task executes. Programmatic runs (scheduler, webhooks, hooks, `needs`,
    // `spawn`) stay headless. The agent must be able to run a TUI and accept a
    // seeded prompt; otherwise the task falls back to headless.
    let interactive = task.interactive
        && agent.capabilities().interactive
        && agent.capabilities().interactive_prompt;

    // Give every headless run a deterministic session id. Agents that accept
    // `{session_id}` (pi) bind it with `--session-id`, and the manager seeds it
    // onto the session so the task row and panel can reopen it later. Agents
    // that report their own id (opencode) ignore the generated one. Interactive
    // runs never seed one: the TUI may not accept the flag.
    let mut ctx = crate::agents::AgentContext {
        cwd: Some(cwd.to_path_buf()),
        provider: def.provider.clone(),
        model: def.model.clone(),
        prompt: Some(prompt.to_string()),
        session_id: (!interactive).then(|| Uuid::new_v4().to_string()),
        managed_session: false,
        rows: 40,
        cols: 120,
        git_signing: def.sign,
    };
    let invocation = if interactive {
        crate::agents::Invocation::Interactive {
            prompt: Some(prompt),
            provider: def.provider.as_deref(),
            model: def.model.as_deref(),
        }
    } else {
        crate::agents::Invocation::Headless {
            prompt,
            provider: def.provider.as_deref(),
            model: def.model.as_deref(),
        }
    };
    // The agent may prepare its own session on a managed transport (opencode's
    // managed server) before the PTY is spawned; headless opencode is a no-op.
    let _ = agent.prepare_launch(&invocation, &mut ctx).await;
    let info = state.agents.start(
        agent_name,
        agent.clone(),
        Some(task.id.to_string()),
        invocation,
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

    let (detect, quiet) = (
        state.config.executor.detect_awaiting_input,
        Duration::from_millis(state.config.executor.awaiting_input_quiet_ms),
    );
    // A user-started interactive run is watched for a finished turn as well as
    // for the process exit: opencode's TUI stays open after the agent answers
    // the prompt, so waiting for it to exit would leave the task `running`
    // forever and never fire its `spawn`/`needs` successors.
    let end = if interactive {
        let since = task.started_at.unwrap_or_else(Utc::now);
        crate::attention::watch_run(
            state,
            crate::attention::WatchRun {
                session: &info.id,
                agent: agent.clone(),
                task_id: Some(task.id),
                quiet,
                detect,
                cwd,
                since,
            },
        )
        .await
    } else if detect {
        crate::attention::WatchEnd::Exited(
            crate::attention::watch(state, &info.id, agent.clone(), Some(task.id), quiet).await,
        )
    } else {
        crate::attention::WatchEnd::Exited(state.agents.wait(&info.id).await)
    };
    let (code, turn_finished) = match end {
        crate::attention::WatchEnd::Exited(code) => (code, false),
        crate::attention::WatchEnd::TurnFinished { success } => {
            (Some(if success { 0 } else { 1 }), true)
        }
    };
    // An interactive run's PTY holds a TUI, not machine output; capture the
    // emulator's plain-text screen instead of the raw escape stream.
    let raw = if interactive {
        state.agents.screen_text(&info.id)
    } else {
        state.agents.output(&info.id)
    };
    let mut result = agent.parse_output(&raw, code);
    // The manager also captures the id live from the PTY; prefer it if the
    // implementation's parser did not find one.
    if result.session_id.is_none() {
        result.session_id = state.agents.external_session_id(&info.id);
    }
    // Prefer the title resolved (and already published) while the run was still
    // in progress. Only fall back to a fresh bounded lookup when the watcher
    // never saw a session id, so the retry budget is never spent twice in a row.
    //
    // When the task ended because the agent finished its turn, the process is
    // still alive and (for an interactive run) never reports a session id, so
    // the title watcher would block forever. Drop it and resolve best-effort.
    let session_title = match (turn_finished, title_watch) {
        (true, handle) => {
            if let Some(handle) = handle {
                handle.abort();
            }
            resolve_title_fallback(agent.clone(), result.session_id.clone(), cwd).await
        }
        (false, Some(handle)) => match handle.await {
            Ok(TitleWatch::Observed(title)) => title,
            Ok(TitleWatch::Skipped) | Err(_) => {
                resolve_title_fallback(agent.clone(), result.session_id.clone(), cwd).await
            }
        },
        (false, None) => {
            resolve_title_fallback(agent.clone(), result.session_id.clone(), cwd).await
        }
    };
    // The run's PTY only carried machine output (e.g. JSON events); drop it once
    // its session id is captured, since reattaching launches a fresh interactive
    // TUI on that session. Agents without resume keep the PTY so its final screen
    // can still be replayed.
    if !interactive && result.session_id.is_some() && agent.capabilities().resume {
        let _ = state.agents.close(&info.id);
    }
    let max = state.config.executor.max_output_bytes;
    let output = build_task_output(
        &result.raw,
        &result.output,
        agent_name,
        result.session_id.clone(),
        session_title.clone(),
        max,
    );
    let failure = match result.exit_code {
        Some(0) => None,
        Some(_) if turn_finished => Some(Failure::new(
            FailureKind::Agent,
            format!("agent '{agent_name}' finished the task unsuccessfully"),
        )),
        Some(c) => Some(Failure::new(
            FailureKind::Agent,
            format!("agent '{agent_name}' exited with {c}:\n{raw}"),
        )),
        // No exit code: the PTY/session disappeared, i.e. a daemon/supervisor fault.
        None => Some(Failure::new(
            FailureKind::Infrastructure,
            format!("agent '{agent_name}' session disappeared"),
        )),
    };
    // Structured agents serialize their folded `RunSummary` into `result.output`;
    // recover its usage from there. Interactive runs carry a TUI screen rather
    // than machine output, so fall back to the folded live state observed while
    // the run was in progress. A run that reported nothing stays `None`.
    let live_usage = state
        .agents
        .find_latest_by_task(&task.id.to_string())
        .and_then(|session| session.usage);
    let usage = reported_usage(&result.output, live_usage);
    Ok(RunOutcome {
        output,
        session_id: result.session_id,
        session_title,
        exit_code: result.exit_code,
        usage,
        failure,
        alive: interactive && turn_finished,
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
    // Fast read for the idempotence check only; do NOT keep this snapshot across
    // the long `resolve_session_title` await below.
    let Ok(Some(existing)) = db::get_task(&state.db, task_id).await else {
        return None;
    };
    // Already complete: don't do a redundant write/publish.
    if existing.session_id.as_deref() == Some(session_id.as_str())
        && existing.session_title.is_some()
    {
        return existing.session_title;
    }

    // The long await happens with no snapshot held.
    let title = if existing.session_title.is_none() {
        resolve_session_title(agent, &session_id, &cwd, attempts, interval).await
    } else {
        None
    };

    // Targeted write: never touches status, so a concurrent mark/resume stands.
    let changed = db::set_task_session(&state.db, task_id, &session_id, title.as_deref())
        .await
        .unwrap_or(false);
    if changed {
        if let Ok(Some(task)) = db::get_task(&state.db, task_id).await {
            state
                .bus
                .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
        }
    }
    // Re-read so the returned value is the authoritative stored title.
    db::get_task(&state.db, task_id)
        .await
        .ok()
        .flatten()
        .and_then(|t| t.session_title)
}

/// Mark a task failed (used when it can't even be started).
///
/// The task is claimed here first — atomically creating the attempt's run — so a
/// failure that happens before dispatch still records exactly one failed run.
async fn fail_task(state: &State, task: Task, kind: FailureKind, error: &str) {
    let mut task = task;
    let claimed = db::claim_task(&state.db, &task).await.ok().flatten();
    if let Some(run) = &claimed {
        task.attempt = run.attempt;
        task.started_at = run.started_at;
    }
    task.status = TaskStatus::Failed;
    task.error = Some(error.to_string());
    task.failure = Some(Failure::new(kind, error));
    task.finished_at = Some(Utc::now());
    // Compare-and-set: only write while the task is still active, so a
    // cancellation that landed between the claim and here wins.
    let finished = db::finish_active_task(&state.db, &task)
        .await
        .unwrap_or(false);
    if let Some(mut run) = claimed {
        run.status = if finished {
            RunStatus::Failed
        } else {
            RunStatus::Cancelled
        };
        run.finished_at = task.finished_at;
        run.error = task.error.clone();
        run.failure = task.failure.clone();
        let _ = db::finalize_task_run(&state.db, &run).await;
    }
    if !finished {
        // Cancelled out from under the pre-dispatch failure: the cancellation owns
        // the row and already emitted `TaskCancelled`; do not fire `TaskFinished`.
        return;
    }
    state
        .bus
        .publish(ServerPush::TaskUpdated(Box::new(task.summary())));
    // Pre-dispatch failures can be retryable too (e.g. a transient worktree
    // setup fault); only finalize the task with a `TaskFinished` when no retry
    // was scheduled.
    if schedule_auto_retry(state, &task, &state.config.executor.retry).await {
        return;
    }
    state
        .emit_event(EventKind::TaskFinished, finished_payload(&task, false))
        .await;
}

fn lookup_def(state: &State, name: &str) -> Option<TaskDef> {
    state
        .catalog
        .read()
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
    /// `favetto/*` branches reclaimed by the git-native sweep (including
    /// rowless leftovers the tracked retention pass cannot see).
    pub branches_removed: usize,
    /// Linked worktrees under the worktree root reclaimed by the sweep.
    pub untracked_worktrees_removed: usize,
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
}

/// What one repo's git-native [`sweep_repo`] pass reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SweepStats {
    branches_removed: usize,
    worktrees_removed: usize,
}

/// Choose the task's working directory: a fresh git worktree when parallel +
/// `worktree` and the base dir is inside a repository, otherwise the base dir.
///
/// `worktree_enabled` is the task's [`TaskDef::worktree`] opt-out resolved
/// against the global default (`None`/`true` inherit it). A `false` task always
/// runs in `base` with `needs_lock = true`, so read-only runs never create a
/// `favetto/*` branch or linked worktree.
async fn make_plan(
    state: &State,
    cfg: &ExecutorSettings,
    base: &Path,
    task: &Task,
    worktree_enabled: bool,
) -> anyhow::Result<Plan> {
    if worktree_enabled && cfg.parallel && cfg.worktree {
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

/// The directory a reopened panel session should be launched in.
///
/// A headless run executes inside the task's worktree and some agents (pi)
/// scope their session store to the project directory, so resuming from the
/// daemon's cwd cannot find the run's session. Reuse [`make_plan`] to resolve
/// the same directory, recreating the worktree when retention already reclaimed
/// it. Returns `None` when the task or its catalog definition is missing.
pub async fn resume_cwd(state: &State, task_id: Uuid) -> Option<PathBuf> {
    let task = db::get_task(&state.db, task_id).await.ok().flatten()?;
    let def = lookup_def(state, &task.name)?;
    let cfg = state.config.executor.clone();
    let base = resolve_base_dir(&def, &task);
    match make_plan(state, &cfg, &base, &task, def.worktree.unwrap_or(true)).await {
        Ok(plan) => Some(plan.cwd),
        Err(e) => {
            tracing::warn!(
                error = %e,
                task_id = %task_id,
                "failed to resolve the resumed session's working directory"
            );
            None
        }
    }
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

/// The first 8 hex of a task id, exactly the suffix [`branch_name`] appends.
fn task_id_prefix(task_id: Uuid) -> String {
    task_id.to_string()[..8].to_string()
}

/// The 8-hex task-id suffix of a `favetto/<slug>-<8hex>` branch name.
///
/// This is the inverse of [`branch_name`]; keep the two in sync if the scheme
/// changes. Returns `None` for a branch that does not carry the suffix.
fn branch_suffix(branch: &str) -> Option<&str> {
    let rest = branch.strip_prefix("favetto/")?;
    let (_, suffix) = rest.rsplit_once('-')?;
    (suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())).then_some(suffix)
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

/// Delete `branch` with `git branch -D`, returning whether git accepted it.
///
/// The `--` separator keeps a branch name from ever being parsed as an option.
async fn delete_branch(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", "--", branch])
        .output()
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `git worktree prune`: clear registrations whose directory has vanished.
async fn prune_repo(repo: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output()
        .await;
}

/// Remove a task's worktree directory and branch, and drop its tracking row.
///
/// A worktree directory cleaned up out-of-band leaves git's *registration* in
/// place, which pins the branch as "checked out" and makes `git branch -D`
/// fail. Prune the stale registration before deleting the branch, and retry
/// once after another prune, so a tracked removal can never leak the branch.
async fn remove_worktree(pool: &SqlitePool, task_id: Uuid, repo: &Path, path: &Path, branch: &str) {
    if let Err(e) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await
    {
        tracing::warn!(error = %e, path = %path.display(), "failed to remove worktree");
    }
    prune_repo(repo).await;
    if !delete_branch(repo, branch).await {
        // A surviving registration elsewhere can still pin the branch; prune
        // once more and retry before giving up.
        prune_repo(repo).await;
        if !delete_branch(repo, branch).await {
            tracing::warn!(
                branch,
                repo = %repo.display(),
                "failed to delete worktree branch after pruning"
            );
        }
    }
    let _ = db::forget_worktree(pool, task_id).await;
}

/// A terminal task is finished: its worktree may be reclaimed by retention.
pub(crate) fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
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

/// Reclaim `favetto/*` branches and linked worktrees under `worktree_root`
/// whose owning task is known-terminal or unknown.
///
/// A branch/worktree is kept when its 8-hex task-id prefix is in
/// `active_prefixes` (a pending/running/awaiting task), when a live interactive
/// session still runs in it, or when `tracked_branches` still has a `worktrees`
/// row (a `keep_worktree`/age-retained worktree owned by the row-based pass).
///
/// Unlike the row-based retention pass this sees leftovers that never got a
/// `worktrees` row: a failed record write, a row dropped after a failed branch
/// delete, or a branch created by an older favetto.
async fn sweep_repo(
    state: &State,
    repo: &Path,
    worktree_root: &Path,
    active_prefixes: &HashSet<String>,
    tracked_branches: &HashSet<String>,
) -> SweepStats {
    let mut stats = SweepStats::default();

    // Clear stale registrations first: a vanished directory otherwise keeps the
    // branch pinned as "checked out" and `git branch -D` refuses it.
    prune_repo(repo).await;

    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await
    else {
        return stats;
    };
    if !out.status.success() {
        return stats;
    }
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();

    // Parse `worktree <path>` / `branch refs/heads/<name>` blocks.
    let mut entries: Vec<(PathBuf, Option<String>)> = Vec::new();
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.push((PathBuf::from(path), None));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(last) = entries.last_mut() {
                last.1 = Some(branch.to_string());
            }
        }
    }

    // A branch checked out by any surviving worktree is off limits, so a sweep
    // can never yank a live session's checkout.
    let linked_branches: HashSet<&str> = entries
        .iter()
        .filter_map(|(_, branch)| branch.as_deref())
        .collect();

    let mut handled: HashSet<String> = HashSet::new();
    for (path, branch) in &entries {
        let Some(branch) = branch else { continue };
        if !branch.starts_with("favetto/") || !path.starts_with(worktree_root) {
            continue;
        }
        if tracked_branches.contains(branch) {
            continue;
        }
        // The directory basename is the full task id; prefer it for the live
        // session check and the `worktrees` row lookup.
        let task_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let suffix_active = branch_suffix(branch).is_some_and(|p| active_prefixes.contains(p));
        let dir_active = task_id
            .map(task_id_prefix)
            .is_some_and(|p| active_prefixes.contains(&p));
        let live = task_id
            .map(|id| state.agents.has_live_interactive(&id.to_string()))
            .unwrap_or(false);
        if suffix_active || dir_active || live {
            handled.insert(branch.clone());
            continue;
        }
        remove_worktree(
            &state.db,
            task_id.unwrap_or(Uuid::nil()),
            repo,
            path,
            branch,
        )
        .await;
        handled.insert(branch.clone());
        stats.worktrees_removed += 1;
        stats.branches_removed += 1;
    }

    // Reclaim bare `favetto/*` branches with no linked worktree. A branch whose
    // owner is active, still checked out, or still tracked by a row is kept.
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--list", "--format=%(refname:short)", "favetto/*"])
        .output()
        .await
    else {
        return stats;
    };
    if !out.status.success() {
        return stats;
    }
    let branch_list = String::from_utf8_lossy(&out.stdout).into_owned();
    for branch in branch_list.lines().map(str::trim).filter(|b| !b.is_empty()) {
        if handled.contains(branch)
            || linked_branches.contains(branch)
            || tracked_branches.contains(branch)
        {
            continue;
        }
        if branch_suffix(branch).is_some_and(|p| active_prefixes.contains(p)) {
            continue;
        }
        if delete_branch(repo, branch).await {
            stats.branches_removed += 1;
        }
    }

    stats
}

/// Remove tracked worktrees the retention policy has expired, drop their
/// branches, and `git worktree prune` each repo. Then run a git-native sweep
/// that also reclaims `favetto/*` branches and linked worktrees with no
/// tracking row. Never fatal.
pub async fn prune_worktrees(state: &State) -> anyhow::Result<WorktreePruneStats> {
    let cfg = state.config.executor.clone();
    let retention = cfg.worktree_retention.clone();
    let all_records = db::list_worktrees(&state.db).await?;

    // A worktree whose task still has a live interactive Agent-panel session
    // attached must not be reclaimed: that session runs inside it.
    let records: Vec<db::WorktreeRecord> = all_records
        .iter()
        .filter(|r| !state.agents.has_live_interactive(&r.task_id.to_string()))
        .cloned()
        .collect();

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
    for repo in &repos {
        stats.repos_pruned += 1;
        prune_repo(repo).await;
    }

    // Git-native sweep. Re-read the rows after the tracked pass: a branch whose
    // delete just failed lost its row, so the sweep must see it and retry.
    let tracked_branches: HashSet<String> = db::list_worktrees(&state.db)
        .await?
        .into_iter()
        .map(|r| r.branch)
        .collect();

    // Never touch a task that is still pending, running, or awaiting input.
    let mut active_prefixes: HashSet<String> = HashSet::new();
    for task in db::list_active_tasks(&state.db).await? {
        active_prefixes.insert(task_id_prefix(task.id));
    }
    for task in db::list_pending_tasks(&state.db).await? {
        active_prefixes.insert(task_id_prefix(task.id));
    }

    // Sweep every repo favetto may have created a worktree in: the tracked ones,
    // plus the resolved base dir of each task row, so a repo whose rows are gone
    // is still reclaimed.
    let mut sweep_repos: Vec<PathBuf> = all_records.iter().map(|r| r.repo.clone()).collect();
    let mut bases: HashSet<PathBuf> = HashSet::new();
    for task in db::list_tasks(&state.db, i64::MAX).await? {
        if let Some(def) = lookup_def(state, &task.name) {
            bases.insert(resolve_base_dir(&def, &task));
        }
    }
    for base in bases {
        if let Some(repo) = git_toplevel(&base).await {
            sweep_repos.push(repo);
        }
    }
    sweep_repos.sort();
    sweep_repos.dedup();
    for repo in sweep_repos {
        let root = worktree_root(&cfg, &state.data_dir, &repo);
        let sweep = sweep_repo(state, &repo, &root, &active_prefixes, &tracked_branches).await;
        stats.branches_removed += sweep.branches_removed;
        stats.untracked_worktrees_removed += sweep.worktrees_removed;
    }

    Ok(stats)
}

/// Per-directory locks, so tasks without worktree isolation serialize.
#[derive(Clone, Default)]
struct DirLocks(Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>);

impl DirLocks {
    fn try_lock(&self, dir: &Path) -> Option<OwnedMutexGuard<()>> {
        let lock = self.0.lock().entry(dir.to_path_buf()).or_default().clone();
        lock.try_lock_owned().ok()
    }
}

/// React to a `TaskFinished` event: start the outcome dependents (`:finished`,
/// `:terminal`, `:succeeded`, `:failed` — once per matching instance) and resolve
/// any `:all_finished` fan-in barriers targeting the finished task's name.
///
/// `:finished` / `:terminal` fire on either outcome; `:succeeded` and `:failed`
/// filter on the `success` field carried by the event. `needs` (and `spawn`)
/// values are relative-path names, so a dependency on a task in a subfolder is
/// written as `pipelines/plan:finished`.
async fn start_dependents(state: &Arc<State>, ev: &Event) {
    let Some(name) = ev.payload.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let success = ev.payload.get("success").and_then(|v| v.as_bool());

    let catalog = state.catalog.read().clone();
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
            NeedsKind::Succeeded if success == Some(true) => finished.push(def.name.clone()),
            NeedsKind::Failed if success == Some(false) => finished.push(def.name.clone()),
            NeedsKind::Succeeded | NeedsKind::Failed => {}
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
                    "output": bounded_prev_value(prev.output.as_ref(), PREV_OUTPUT_BYTES),
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
                enqueue_with_lineage(state, dep, input.clone(), Some(dedupe), lineage, false).await
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
        false,
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
    let per_task = (PREV_TASKS_BYTES / tasks.len().max(1)).max(MIN_PREV_OUTPUT_BYTES);
    let entries: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "task_id": t.id.to_string(),
                "name": t.name,
                "status": t.status.as_str(),
                "success": t.status == TaskStatus::Succeeded,
                "session_id": t.session_id,
                "input": bounded_prev_value(Some(&t.input), per_task),
                "output": bounded_prev_value(t.output.as_ref(), per_task),
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
#[path = "executor_tests.rs"]
mod tests;
